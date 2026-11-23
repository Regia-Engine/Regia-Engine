// SPDX-License-Identifier: MPL-2.0
//! Frame-path creates the hitch gate could not see (`specs/27` § 2.4, ADR-2585): the fullscreen
//! video's draw groups, the HW-RT shadow passes' uniforms and groups, and the outdoor targets were
//! made on a raw `wgpu::Device` (or made again every frame), so `frame_path_counters` stayed at
//! zero while they were created. Each is now counted when built (by label) and built once: a
//! steady frame after the first creates nothing. The fade overlay and the HDR display stage have
//! the same tests as unit tests in their modules (their types are private).
//!
//! Lavapipe, counters and fixed frame counts; no wall clock.

use std::sync::Arc;

use regia_render::hwrt::{
    ShadowBvh, ShadowFallbackPass, ShadowFallbackScene, ShadowParams, ShadowTargets,
    create_shadow_visibility, upload_shadow_origins,
};
use regia_render::{
    DirectionalLightUniform, FramePathKind, FrameQuality, FrameRenderer, FullscreenFit, FullscreenVideo,
    LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, RenderDevice, RenderProfile,
    RenderProfilePreset, TimeOfDay, VideoFullscreenQueue, VolumetricFogSettings, take_frame_path_events,
    trace_frame_path,
};

/// One GPU test at a time: the frame-path trace is per thread, but lavapipe is shared.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Every counted create `(kind, label)` the closure made on this thread (blocking polls left out).
fn traced(run: impl FnOnce()) -> Vec<(FramePathKind, String)> {
    trace_frame_path(true);
    run();
    let events = take_frame_path_events();
    trace_frame_path(false);
    events
        .into_iter()
        .filter(|e| e.kind != FramePathKind::BlockingPoll)
        .map(|e| (e.kind, e.label))
        .collect()
}

fn labelled<'a>(events: &'a [(FramePathKind, String)], labels: &[&str]) -> Vec<&'a (FramePathKind, String)> {
    events
        .iter()
        .filter(|(_, label)| labels.contains(&label.as_str()))
        .collect()
}

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// A ground plane and a wall seen from a fixed eye.
fn scene(width: u32, height: u32) -> LabScene {
    LabScene {
        width,
        height,
        eye: [0.0, 1.8, 0.0],
        target: [0.0, 4.0, -60.0],
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
        materials: vec![LabMaterial {
            base_color: [0.4, 0.3, 0.25, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            quad(
                [
                    [-200.0, 0.0, 20.0],
                    [200.0, 0.0, 20.0],
                    [200.0, 0.0, -200.0],
                    [-200.0, 0.0, -200.0],
                ],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [
                    [-150.0, 0.0, -120.0],
                    [150.0, 0.0, -120.0],
                    [150.0, 60.0, -120.0],
                    [-150.0, 60.0, -120.0],
                ],
                [0.0, 0.0, 1.0],
            ),
        ],
    }
}

/// A playing fullscreen video (letterboxed: a bar draw and a picture draw per frame) creates its
/// draw group once per picture, not once per draw per frame (it did, counted, every frame).
#[test]
fn a_playing_fullscreen_video_creates_its_group_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    let queue = VideoFullscreenQueue::default();
    assert!(renderer.enable_video_fullscreen(&queue));
    let lab = scene(64, 48);
    let (pw, ph) = (8u32, 4u32);
    let rgba: Arc<[u8]> = Arc::from(vec![200u8; (pw * ph * 4) as usize]);
    let mut frame = |serial: u64| {
        queue.set(vec![FullscreenVideo {
            player: 3,
            layer: 0,
            fit: FullscreenFit::Letterbox,
            aspect: 2.0,
            serial,
            width: pw,
            height: ph,
            rgba: Arc::clone(&rgba),
        }]);
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(FrameQuality::LAB))
            .expect("frame");
    };
    let first = traced(|| frame(1));
    let groups = |events: &[(FramePathKind, String)]| {
        labelled(events, &["video fullscreen bg"])
            .into_iter()
            .filter(|(kind, _)| *kind == FramePathKind::BindGroup)
            .count()
    };
    assert_eq!(groups(&first), 1, "one group for the picture: {first:?}");
    // The same picture, then new frames of it (same size): uploads, no new group.
    for serial in [1, 2, 3, 4] {
        let later = traced(|| frame(serial));
        assert_eq!(
            groups(&later),
            0,
            "a playing video created groups at serial {serial}: {later:?}"
        );
    }
    assert!(queue.stats().uploads >= 4, "the new serials uploaded");
}

/// The HW-RT shadow compute fallback counts its uniform and group, and tracing the same samples
/// toward the same light again (a held sun) creates nothing; a new light makes one group, counted.
#[test]
fn the_shadow_fallback_counts_its_creates_and_reuses_a_held_trace() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tris = vec![[[-1.0, 2.0, -1.0], [1.0, 2.0, -1.0], [1.0, 2.0, 1.0]]];
    let bvh = ShadowBvh::build(&tris).expect("bvh");
    let points: Vec<[f32; 3]> = (0..16).map(|i| [i as f32 * 0.1 - 0.8, 0.0, 0.2]).collect();
    let pass = ShadowFallbackPass::new(&device.device);
    let scene = ShadowFallbackScene::upload(&device.device, &bvh);
    let origins = upload_shadow_origins(&device.device, &points);
    let visibility = create_shadow_visibility(&device.device, points.len() as u32);
    let trace = |light: [f32; 3]| {
        let params = ShadowParams::toward(light).expect("params");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("shadow fallback counted"),
            });
        pass.encode(
            &device.device,
            &mut encoder,
            &scene,
            &params,
            &ShadowTargets {
                origins: &origins,
                visibility: &visibility,
                sample_count: points.len() as u32,
            },
        );
        device.queue.submit([encoder.finish()]);
    };
    let first = traced(|| trace([0.0, 1.0, 0.0]));
    for (kind, label) in [
        (FramePathKind::Buffer, "hwrt shadow fallback params"),
        (FramePathKind::BindGroup, "hwrt shadow fallback"),
    ] {
        assert!(
            first.iter().any(|(k, l)| *k == kind && l == label),
            "the first trace's {kind:?} {label:?} is counted: {first:?}"
        );
    }
    for _ in 0..3 {
        let held = traced(|| trace([0.0, 1.0, 0.0]));
        assert!(held.is_empty(), "a held trace created {held:?}");
    }
    let moved = traced(|| trace([0.3, 1.0, 0.0]));
    assert_eq!(
        moved
            .iter()
            .filter(|(k, _)| *k == FramePathKind::BindGroup)
            .count(),
        1,
        "a new light builds one group: {moved:?}"
    );
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// The outdoor sky, fog and cloud targets are counted when built, and a steady outdoor frame
/// builds none of them again. (The sky tables are left out: their texture follows the bake, which
/// may land on a later frame.)
#[test]
fn outdoor_targets_are_counted_and_built_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    let lab = scene(96, 54);
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.volumetric_fog = true;
    let first = traced(|| {
        renderer.set_outdoor(OutdoorParams {
            time: Some(TimeOfDay::noon()),
            fog: VolumetricFogSettings {
                density: 0.01,
                height_falloff: 0.05,
                ..VolumetricFogSettings::default()
            },
            ..OutdoorParams::default()
        });
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality))
            .expect("first outdoor frame");
    });
    let outdoor = [
        "cloud dummy",
        "dummy depth",
        "dummy rgba",
        "sky body placeholder",
        "froxel scatter",
        "froxel integrated",
    ];
    let made = labelled(&first, &outdoor);
    assert!(
        made.iter().any(|(kind, _)| *kind == FramePathKind::Texture),
        "the outdoor targets of the first frame are counted: {first:?}"
    );
    for _ in 0..3 {
        let later = traced(|| {
            renderer
                .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality))
                .expect("outdoor frame");
        });
        let again = labelled(&later, &outdoor);
        assert!(again.is_empty(), "a steady outdoor frame rebuilt {again:?}");
    }
}
