// SPDX-License-Identifier: MPL-2.0
//! Late pipelines and bind groups after warm-up (DO27 tester item pipeline-cache-cold, ADR-7885).
//!
//! DO27's late creations were whole per-renderer sets ("lab forward", "lab depth", "lab shadow",
//! "lab sky" and the effects' sets) for a colour format or sample count the warm never drew into
//! (a render-to-texture target, a second window). A target registered with the warm
//! (`pipeline_warm::register_warm_target`) is drawn by the warm, so its first frame after
//! warm-up compiles nothing. TSR's resolve bind groups survive a history reset (the 2026-11-15
//! soak built two "tsr resolve" groups after warm-up).
//!
//! Counters and fixed frame counts only; lavapipe in CI. One device at a time per binary.

use regia_render::pipeline_warm::{clear_warm_targets, register_warm_target};
use regia_render::{
    DirectionalLightUniform, FramePathKind, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, TemporalUpscaler, frame_path_counters, take_frame_path_events, trace_frame_path,
};

const W: u32 = 160;
const H: u32 = 90;
/// The window the warm draws for.
const WINDOW: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

/// The registry and the device: one test at a time in this binary.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3], material: usize) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A lit ground and wall, opaque and blended glass-free (the lab forward set and its sky).
fn lab() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.8, 0.0],
        target: [0.0, 2.0, -20.0],
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.6, -0.4], [1.0, 0.95, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.7, 0.7, 1.0],
            roughness: 0.6,
            ..LabMaterial::default()
        }],
        meshes: vec![
            quad(
                [
                    [-50.0, 0.0, 10.0],
                    [50.0, 0.0, 10.0],
                    [50.0, 0.0, -80.0],
                    [-50.0, 0.0, -80.0],
                ],
                [0.0, 1.0, 0.0],
                0,
            ),
            quad(
                [
                    [-6.0, 0.0, -12.0],
                    [2.0, 0.0, -12.0],
                    [2.0, 9.0, -12.0],
                    [-6.0, 9.0, -12.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
        ],
    }
}

fn quality(preset: RenderProfilePreset) -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(preset))
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    }
}

/// A render target texture of `format`.
fn target(device: &RenderDevice, format: wgpu::TextureFormat) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

fn draw(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &FrameScene<'_>,
    format: wgpu::TextureFormat,
) {
    let (_texture, view) = target(device, format);
    if let Err(err) = renderer.render_into(device, scene, FrameTarget::full(&view, format, W, H)) {
        panic!("frame into {format:?}: {err}");
    }
}

/// The frame-path creations of `frames` frames on this thread: pipelines, bind groups and
/// their traced events.
fn late(f: impl FnOnce()) -> (u64, u64, Vec<regia_render::FramePathEvent>) {
    let before = frame_path_counters();
    trace_frame_path(true);
    let _ = take_frame_path_events();
    f();
    let events = take_frame_path_events();
    trace_frame_path(false);
    let after = frame_path_counters();
    (
        after.pipelines - before.pipelines,
        after.bind_groups - before.bind_groups,
        events,
    )
}

fn describe(events: &[regia_render::FramePathEvent], kind: FramePathKind) -> String {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| {
            let d = &e.detail;
            let axes: Vec<&str> = d.new_axes.names().collect();
            format!(
                "`{}` format {:?} samples {} site {} new axes [{}]",
                e.label,
                d.format,
                d.samples,
                d.site
                    .map_or_else(|| "?".to_owned(), |s| format!("{}:{}", s.file(), s.line())),
                axes.join(","),
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// A render-to-texture target of another format with 4× MSAA (Rgba8UnormSrgb), registered
/// before the warm: the warm draws into it, so the first frames into it after warm-up compile
/// no pipeline. Fails without the registration (v0.5.0: the whole lab set and the post chain's
/// output pipelines compile on that frame).
#[test]
fn a_registered_msaa_target_of_another_format_compiles_nothing_after_the_warm() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    clear_warm_targets();
    let offscreen = wgpu::TextureFormat::Rgba8UnormSrgb;
    assert!(register_warm_target(offscreen, 4), "a new target registers");
    let lab = lab();
    let post = post();
    let mut window_scene = lab.as_frame_scene_with_quality(quality(RenderProfilePreset::DesktopHigh));
    window_scene.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    if let Err(err) = renderer.warm_scene(&device, &window_scene, WINDOW, false) {
        panic!("warm: {err}");
    }
    // Warm-up: the window's own frames.
    for _ in 0..3 {
        draw(&device, &mut renderer, &window_scene, WINDOW);
    }
    // After warm-up: the render-to-texture camera, with and without post.
    let mut offscreen_scene = window_scene.clone();
    offscreen_scene.quality.msaa_samples = 4;
    let mut no_post = offscreen_scene.clone();
    no_post.post = None;
    let (pipelines, bind_groups, events) = late(|| {
        for _ in 0..3 {
            draw(&device, &mut renderer, &offscreen_scene, offscreen);
            draw(&device, &mut renderer, &no_post, offscreen);
        }
    });
    clear_warm_targets();
    eprintln!("after warm-up into {offscreen:?} x4: {pipelines} pipelines, {bind_groups} bind groups");
    assert_eq!(
        pipelines,
        0,
        "pipelines created after warm-up for a registered target:\n  {}",
        describe(&events, FramePathKind::Pipeline)
    );
}

/// Every late pipeline names its target, sample count and the forward-set key axis the
/// renderer had no set for (ADR-7885): an unregistered MSAA target's first frame reports
/// `samples` 4 and the new `samples` axis on its "lab forward".
#[test]
fn a_late_forward_set_names_its_target_and_new_key_axes() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    clear_warm_targets();
    let lab = lab();
    let post = post();
    let mut scene = lab.as_frame_scene_with_quality(quality(RenderProfilePreset::DesktopHigh));
    scene.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    for _ in 0..3 {
        draw(&device, &mut renderer, &scene, WINDOW);
    }
    let mut msaa = scene.clone();
    msaa.quality.msaa_samples = 4;
    let (pipelines, _, events) = late(|| draw(&device, &mut renderer, &msaa, WINDOW));
    assert!(pipelines > 0, "an unregistered MSAA frame builds its set");
    let forward = events
        .iter()
        .find(|e| e.kind == FramePathKind::Pipeline && e.label == "lab forward")
        .unwrap_or_else(|| {
            panic!(
                "no `lab forward` among:\n  {}",
                describe(&events, FramePathKind::Pipeline)
            )
        });
    assert_eq!(forward.detail.samples, 4, "{forward:?}");
    assert!(forward.detail.format.is_some(), "{forward:?}");
    assert!(
        !forward.detail.warm_worker,
        "built on the frame thread: {forward:?}"
    );
    assert!(
        forward.detail.new_axes.contains(regia_render::KeyAxes::SAMPLES),
        "the new key axis is the sample count: {forward:?}"
    );
    assert!(forward.detail.site.is_some(), "{forward:?}");
}

/// A TSR frame sequence with history resets after warm-up creates no bind group: a moving
/// (chase) camera, a camera cut to a still camera held for 40 frames, then a cut back to the
/// chase, as the 2026-11-15 Switchbacks soak's fixed cut cameras do (TSR 0.87; it built two
/// "tsr resolve" groups at frames 991-992, about 30 frames into the still shot that starts at
/// frame 960). A still camera holds the static-screen composite across frames, so the resolve
/// meets that view on both ping-pong parities.
#[test]
fn a_tsr_history_reset_after_warmup_creates_no_bind_group() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    clear_warm_targets();
    let lab = lab();
    let post = post();
    let permille = 870u16;
    let mut q = quality(RenderProfilePreset::DesktopHigh);
    q.taau = true;
    q.render_scale_permille = permille;
    let mut scene = lab.as_frame_scene_with_quality(q);
    scene.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(TemporalUpscaler::Tsr);
    renderer.set_camera_cut_detection(None);
    let eye = scene.eye;
    let chase = |renderer: &mut FrameRenderer, scene: &mut FrameScene<'_>, frames: u32, from: u32| {
        for i in from..from + frames {
            scene.eye = [eye[0] + 0.05 * i as f32, eye[1], eye[2]];
            draw(&device, renderer, scene, WINDOW);
        }
    };
    // Warm-up: the chase camera only.
    chase(&mut renderer, &mut scene, 25, 0);
    let (pipelines, bind_groups, events) = late(|| {
        // A cut to a still camera held 40 frames, then a cut back to the chase.
        renderer.camera_cut();
        scene.eye = [eye[0] - 1.0, eye[1] + 0.5, eye[2]];
        for _ in 0..40 {
            draw(&device, &mut renderer, &scene, WINDOW);
        }
        renderer.camera_cut();
        chase(&mut renderer, &mut scene, 5, 40);
    });
    eprintln!("TSR at {permille}‰ after warm-up with cuts: {pipelines} pipelines, {bind_groups} bind groups");
    assert_eq!(
        bind_groups,
        0,
        "bind groups created by a TSR history reset after warm-up ({permille}‰):\n  {}",
        describe(&events, FramePathKind::BindGroup)
    );
    assert_eq!(pipelines, 0, "{}", describe(&events, FramePathKind::Pipeline));
}

/// The soak's real trigger (review rev-d row 5, ADR-8122): the stochastic SSR pass hands the
/// resolve its own input instead of its "ssr color" output once 30 readbacks saw no glossy pixel.
/// A chase over a glossy floor warms up (the pass is awake), then a cut to a still shot of a matte
/// wall: about 30 frames later the pass sleeps and the resolve's colour input switches to a view
/// it never bound during warm-up. v0.5.1 built two "tsr resolve" groups there (the 2026-11-15
/// Switchbacks soak, frames 991-992, 31 frames into the still shot from frame 960).
#[test]
fn tsr_creates_no_resolve_group_when_stochastic_ssr_falls_asleep_after_warmup() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    clear_warm_targets();
    let mut lab = lab();
    // The ground is a mirror (glossy pixels while it is in view); the wall stays matte.
    lab.materials.push(LabMaterial {
        base_color: [0.9, 0.9, 0.9, 1.0],
        metallic: 1.0,
        roughness: 0.0,
        ..LabMaterial::default()
    });
    lab.meshes[0].material = 1;
    // Well above the pass's roughness cutoff (0.55): no traced pixel on the wall.
    lab.materials[0].roughness = 0.9;
    let post = post();
    let permille = 870u16;
    let mut q = quality(RenderProfilePreset::DesktopHigh);
    q.taau = true;
    q.render_scale_permille = permille;
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    assert!(q.stochastic_ssr(), "Desktop High runs the stochastic SSR pass");
    let mut scene = lab.as_frame_scene_with_quality(q);
    scene.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(TemporalUpscaler::Tsr);
    renderer.set_camera_cut_detection(None);
    let frame = |renderer: &mut FrameRenderer, scene: &FrameScene<'_>| {
        draw(&device, renderer, scene, WINDOW);
        // The stochastic SSR pass's counter readbacks land (a presenting window polls each frame).
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    // Warm-up: the chase over the mirror floor, the pass awake.
    let eye = scene.eye;
    for i in 0..25 {
        scene.eye = [eye[0] + 0.05 * i as f32, eye[1], eye[2]];
        frame(&mut renderer, &scene);
    }
    assert!(!renderer.sssr_dormant(), "the mirror floor keeps the pass awake");
    let mut slept_at = None;
    let (_, bind_groups, events) = late(|| {
        // A cut to a still shot that sees only the matte wall.
        renderer.camera_cut();
        scene.eye = [-2.0, 4.5, -10.0];
        scene.target = [-2.0, 4.5, -12.0];
        for n in 1..=90u32 {
            frame(&mut renderer, &scene);
            if slept_at.is_none() && renderer.sssr_dormant() {
                slept_at = Some(n);
            }
            if slept_at.is_some_and(|at| n >= at + 5) {
                break;
            }
        }
    });
    let slept_at = slept_at.unwrap_or_else(|| panic!("the pass never went dormant on the matte wall"));
    let resolve: Vec<_> = events
        .iter()
        .filter(|e| e.kind == FramePathKind::BindGroup && e.label == "tsr resolve")
        .collect();
    eprintln!(
        "TSR at {permille}‰, the pass dormant after {slept_at} still frames: {bind_groups} bind groups, {} \"tsr resolve\"",
        resolve.len()
    );
    assert!(
        resolve.is_empty(),
        "\"tsr resolve\" groups created after warm-up when the stochastic SSR pass fell asleep ({permille}‰):\n  {}",
        describe(&events, FramePathKind::BindGroup)
    );
}
