// SPDX-License-Identifier: MPL-2.0
//! Temporal state must not leak across a camera cut (look lane finding, 2026-10-08).
//!
//! Four frames after a scene change, 1–10 % of the previous scene's light was still on
//! screen, TAA off included: the froxel fog history, the auto-exposure meter, the static-screen
//! cache and the shadow caches all carried the old view. After `FrameRenderer::camera_cut`
//! (or a detected cut) the first frame of the new view must be the frame a fresh renderer
//! draws first, and the frames after it must follow the fresh renderer's.

use regia_render::{
    CameraCutReason, CutExposure, DirectionalLightUniform, FramePathKind, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, TemporalUpscaler, TimeOfDay, VolumetricFogSettings, take_frame_path_events,
    trace_frame_path,
};

const W: u32 = 160;
const H: u32 = 90;
/// Largest channel difference a pixel may show against the fresh renderer.
const MAX_CHANNEL_DIFF: u8 = 2;
/// Pixels allowed beyond it (edge pixels of a nondeterministic GPU), per million.
const OUTLIER_PPM: usize = 1_000;

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

fn base(eye: [f32; 3], target: [f32; 3], materials: Vec<LabMaterial>, meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.9,
        near: 0.1,
        far: 2_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.6, -0.4], [1.0, 0.95, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Scene A: a bright white wall up close and a distant one, under haze.
fn scene_a() -> LabScene {
    base(
        [0.0, 1.8, 0.0],
        [0.0, 4.0, -60.0],
        vec![LabMaterial {
            base_color: [0.95, 0.95, 0.95, 1.0],
            roughness: 0.5,
            ..LabMaterial::default()
        }],
        vec![
            quad(
                [
                    [-200.0, 0.0, 20.0],
                    [200.0, 0.0, 20.0],
                    [200.0, 0.0, -200.0],
                    [-200.0, 0.0, -200.0],
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
            quad(
                [
                    [-150.0, 0.0, -120.0],
                    [150.0, 0.0, -120.0],
                    [150.0, 60.0, -120.0],
                    [-150.0, 60.0, -120.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
        ],
    )
}

/// Scene B, 400 m away: a dark ground, a red and a blue panel, open sky behind.
fn scene_b(eye: [f32; 3], target: [f32; 3]) -> LabScene {
    let (x, z) = (eye[0], eye[2]);
    base(
        eye,
        target,
        vec![
            LabMaterial {
                base_color: [0.05, 0.05, 0.05, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.1, 0.05, 1.0],
                roughness: 0.4,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.05, 0.2, 0.8, 1.0],
                roughness: 0.4,
                ..LabMaterial::default()
            },
        ],
        vec![
            quad(
                [
                    // Not A's ground at the same place: a scene change replaces every object.
                    [x - 200.0, 0.0, z + 25.0],
                    [x + 200.0, 0.0, z + 25.0],
                    [x + 200.0, 0.0, z - 200.0],
                    [x - 200.0, 0.0, z - 200.0],
                ],
                [0.0, 1.0, 0.0],
                0,
            ),
            quad(
                [
                    [x + 1.0, 0.0, z - 15.0],
                    [x + 7.0, 0.0, z - 15.0],
                    [x + 7.0, 5.0, z - 15.0],
                    [x + 1.0, 5.0, z - 15.0],
                ],
                [0.0, 0.0, 1.0],
                1,
            ),
            quad(
                [
                    [x - 9.0, 0.0, z - 30.0],
                    [x - 2.0, 0.0, z - 30.0],
                    [x - 2.0, 3.0, z - 30.0],
                    [x - 9.0, 3.0, z - 30.0],
                ],
                [0.0, 0.0, 1.0],
                2,
            ),
        ],
    )
}

fn scene_b_far() -> LabScene {
    scene_b([400.0, 1.6, 0.0], [400.0, 6.0, -60.0])
}

fn outdoor() -> OutdoorParams {
    OutdoorParams {
        time: Some(TimeOfDay::noon()),
        fog: VolumetricFogSettings {
            density: 0.01,
            height_falloff: 0.05,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    }
}

#[derive(Clone, Copy, Debug)]
struct Config {
    preset: RenderProfilePreset,
    taa: bool,
    upscaler: TemporalUpscaler,
}

fn quality(config: Config) -> FrameQuality {
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(config.preset));
    quality.taau = config.taa;
    quality
}

fn renderer(config: Config) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(outdoor());
    renderer.set_temporal_upscaler(config.upscaler);
    renderer
}

fn post(auto_exposure: bool) -> PostSettings {
    PostSettings {
        auto_exposure,
        exposure_ev: if auto_exposure { 0.0 } else { 14.0 },
        bloom: false,
        ..PostSettings::golden_deterministic()
    }
}

fn render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    config: Config,
    post: &PostSettings,
    frames: usize,
) -> Vec<Vec<u8>> {
    let mut frame = lab.as_frame_scene_with_quality(quality(config));
    frame.post = Some(post);
    (0..frames)
        .map(|i| match renderer.render_offscreen(device, &frame) {
            Ok(image) => image.pixels,
            Err(err) => panic!("{config:?} frame {i}: {err}"),
        })
        .collect()
}

/// Largest channel difference and the pixels beyond [`MAX_CHANNEL_DIFF`].
fn delta(a: &[u8], b: &[u8]) -> (u8, usize) {
    assert_eq!(a.len(), b.len());
    let mut max = 0;
    let mut outliers = 0;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let worst = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(worst);
        if worst > MAX_CHANNEL_DIFF {
            outliers += 1;
        }
    }
    (max, outliers)
}

fn mean_luma(pixels: &[u8]) -> f32 {
    let sum: f32 = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .sum();
    sum / (pixels.len() / 4) as f32
}

fn assert_matches(what: &str, actual: &[u8], fresh: &[u8]) {
    let (max, outliers) = delta(actual, fresh);
    let allowed = (W * H) as usize * OUTLIER_PPM / 1_000_000;
    eprintln!("{what}: max channel diff {max}, {outliers} px beyond ±{MAX_CHANNEL_DIFF} (allowed {allowed})");
    assert!(
        outliers <= allowed,
        "{what}: {outliers} px differ from a fresh renderer by more than {MAX_CHANNEL_DIFF} (max {max}); \
         temporal state leaked across the cut"
    );
}

const CONFIGS: [Config; 5] = [
    Config {
        preset: RenderProfilePreset::DesktopHigh,
        taa: true,
        upscaler: TemporalUpscaler::Taau,
    },
    Config {
        preset: RenderProfilePreset::DesktopHigh,
        taa: false,
        upscaler: TemporalUpscaler::Taau,
    },
    Config {
        preset: RenderProfilePreset::Deck,
        taa: true,
        upscaler: TemporalUpscaler::Taau,
    },
    Config {
        preset: RenderProfilePreset::Deck,
        taa: false,
        upscaler: TemporalUpscaler::Taau,
    },
    Config {
        preset: RenderProfilePreset::DesktopHigh,
        taa: true,
        upscaler: TemporalUpscaler::Tsr,
    },
];

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Scene A for eight frames, `camera_cut`, scene B: B's first frame is a fresh renderer's
/// first frame of B (auto exposure on, so the meter must snap too).
#[test]
fn first_frame_after_a_cut_is_a_fresh_first_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (a, b) = (scene_a(), scene_b_far());
    let post = post(true);
    for config in CONFIGS {
        let mut fresh = renderer(config);
        let want = render(&device, &mut fresh, &b, config, &post, 1);

        let mut cut = renderer(config);
        // Explicit cuts only: this test is about what the reset clears, not detection.
        cut.set_camera_cut_detection(None);
        render(&device, &mut cut, &a, config, &post, 8);
        cut.camera_cut();
        let got = render(&device, &mut cut, &b, config, &post, 1);
        assert_eq!(
            cut.last_camera_cut().map(|c| c.1),
            Some(CameraCutReason::Requested)
        );
        assert_matches(&format!("{config:?} frame 1 after the cut"), &got[0], &want[0]);

        // The finding, for the record: the same switch without a cut.
        let mut leak = renderer(config);
        leak.set_camera_cut_detection(None);
        render(&device, &mut leak, &a, config, &post, 8);
        let leaked = render(&device, &mut leak, &b, config, &post, 1);
        let (max, outliers) = delta(&leaked[0], &want[0]);
        eprintln!(
            "{config:?} without a cut: max channel diff {max}, {outliers} px beyond ±{MAX_CHANNEL_DIFF}"
        );
    }
}

/// With a fixed exposure the frames after the cut follow the fresh renderer's frame for
/// frame: history restarts and then accumulates as it does from a first frame.
#[test]
fn frames_after_a_cut_follow_a_fresh_renderer() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (a, b) = (scene_a(), scene_b_far());
    let post = post(false);
    for config in CONFIGS {
        let mut fresh = renderer(config);
        let want = render(&device, &mut fresh, &b, config, &post, 4);
        let mut cut = renderer(config);
        cut.set_camera_cut_detection(None);
        render(&device, &mut cut, &a, config, &post, 8);
        cut.camera_cut();
        let got = render(&device, &mut cut, &b, config, &post, 4);
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_matches(&format!("{config:?} frame {} after the cut", i + 1), g, w);
        }
    }
}

/// No call: a teleport, and a scene change at the same camera, are detected as cuts.
#[test]
fn detected_cuts_reset_like_an_explicit_cut() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let post = post(true);
    let config = CONFIGS[0];
    let a = scene_a();
    let cases = [
        (scene_b_far(), CameraCutReason::EyeJump),
        // Same eye and look as A, other objects.
        (
            scene_b([0.0, 1.8, 0.0], [0.0, 4.0, -60.0]),
            CameraCutReason::SceneChange,
        ),
    ];
    for (b, reason) in cases {
        let mut fresh = renderer(config);
        let want = render(&device, &mut fresh, &b, config, &post, 1);
        let mut auto = renderer(config);
        render(&device, &mut auto, &a, config, &post, 8);
        assert_eq!(auto.camera_cut_count(), 0, "a held view is not a cut");
        let got = render(&device, &mut auto, &b, config, &post, 1);
        assert_eq!(auto.camera_cut_count(), 1);
        assert_eq!(auto.last_camera_cut(), Some((auto.frame_index(), reason)));
        assert_matches(&format!("detected {reason:?}"), &got[0], &want[0]);
    }
}

/// A snapped cut lands on the new scene's exposure as fast as a fresh renderer; a fade
/// cut keeps adapting from the old one.
#[test]
fn exposure_snaps_on_a_cut_unless_asked_to_fade() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // A ceiling above A's metered EV, so the two scenes meter apart (about 4 EV) instead of
    // both clamping to the default EV100 15.
    let post = PostSettings {
        auto_exposure_max_ev100: 20.0,
        ..post(true)
    };
    let config = CONFIGS[1];
    // B looks down at its dark ground: no sky in the frame.
    let (a, b) = (scene_a(), scene_b([400.0, 1.6, 0.0], [400.0, 0.0, -8.0]));
    let frames = 8;
    let series = |images: Vec<Vec<u8>>| images.iter().map(|i| mean_luma(i)).collect::<Vec<f32>>();
    let mut fresh = renderer(config);
    let want = series(render(&device, &mut fresh, &b, config, &post, frames));
    let mut runs = Vec::new();
    for exposure in [CutExposure::Snap, CutExposure::Fade] {
        let mut r = renderer(config);
        r.set_camera_cut_detection(None);
        render(&device, &mut r, &a, config, &post, 12);
        r.camera_cut_with(exposure);
        runs.push(series(render(&device, &mut r, &b, config, &post, frames)));
    }
    eprintln!(
        "mean luma after the cut: fresh {want:.1?}\n  snap {:.1?}\n  fade {:.1?}",
        runs[0], runs[1]
    );
    let last = frames - 1;
    assert!(
        (runs[0][last] - want[last]).abs() <= 1.0,
        "snapped exposure {:.1} is not the fresh renderer's {:.1}",
        runs[0][last],
        want[last]
    );
    assert!(
        (runs[1][last] - want[last]).abs() > 2.0,
        "a fade cut must still be adapting from scene A ({:.1} vs settled {:.1})",
        runs[1][last],
        want[last]
    );
}

/// Without a cut nothing resets: a held view and a slow pan detect no cut, and the TAA
/// history keeps accumulating (the held frame differs from a view cut every frame).
#[test]
fn history_accumulates_without_a_cut() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let post = post(false);
    let config = CONFIGS[0];
    let a = scene_a();
    let mut held = renderer(config);
    let frames = render(&device, &mut held, &a, config, &post, 12);
    assert_eq!(held.camera_cut_count(), 0, "a held view must not cut");

    let mut always = renderer(config);
    let mut frame = a.as_frame_scene_with_quality(quality(config));
    frame.post = Some(&post);
    let mut last = Vec::new();
    for _ in 0..12 {
        always.camera_cut();
        last = always
            .render_offscreen(&device, &frame)
            .expect("cut frame")
            .pixels;
    }
    // Every frame of `always` is a first frame again.
    assert_matches("cut every frame vs first frame", &last, &frames[0]);
    let (max, outliers) = delta(&frames[11], &last);
    eprintln!(
        "held frame 12 vs a first frame: max channel diff {max}, {outliers} px beyond ±{MAX_CHANNEL_DIFF}"
    );
    assert!(
        outliers > 0,
        "twelve held TAA frames must differ from a first frame (history accumulating)"
    );

    // A slow pan (a walking camera) is not a cut either.
    // One scene kept alive: its object keys (mesh addresses) stay the same.
    let mut pan = renderer(config);
    let mut moved = scene_a();
    for _ in 0..12 {
        moved.eye[0] += 0.05;
        moved.target[0] += 0.05;
        render(&device, &mut pan, &moved, config, &post, 1);
    }
    assert_eq!(pan.camera_cut_count(), 0, "a walking camera must not cut");
}

/// A camera cut after warm-up creates no `taau` bind group. The TAAU resolve must keep its
/// previous-velocity view across the cut (`blend.w` already drops it in the shader), or every
/// cut swaps that view for the 1×1 dummy and rebuilds the resolve bind group (gpu-farm4: one
/// `taau` bind group per cut). Warm-up is odd so the cut lands on the opposite history/velocity
/// ping-pong slot from frame 0's cold start (the bug's dummy-view key otherwise collides with
/// frame 0's cached group and hides).
#[test]
fn a_camera_cut_after_warmup_creates_no_taau_bind_group() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let post = post(false);
    let config = CONFIGS[0]; // DesktopHigh, TAA on, Taau
    let a = scene_a();

    let mut renderer = renderer(config);
    renderer.set_camera_cut_detection(None);
    render(&device, &mut renderer, &a, config, &post, 25);

    renderer.camera_cut();
    trace_frame_path(true);
    let _ = take_frame_path_events();
    render(&device, &mut renderer, &a, config, &post, 1);
    let events = take_frame_path_events();
    trace_frame_path(false);
    let taau = events
        .iter()
        .filter(|e| e.kind == FramePathKind::BindGroup && e.label == "taau")
        .count();
    assert_eq!(taau, 0, "the camera cut created a `taau` bind group");
}
