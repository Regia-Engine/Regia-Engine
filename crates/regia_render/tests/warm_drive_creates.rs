// SPDX-License-Identifier: MPL-2.0
//! A warm scene creates no bind group on the frame path for the rest of its drive
//! (`specs/27` § 2.4, ADR-2303).
//!
//! The DO27 hitch soak found bind groups created long after warm-up, each the first time a
//! configuration occurred: the receiver group of the first projected decal, the froxel fog
//! groups of the first camera cut (no fog history), and the histogram and post groups of the
//! first converged held frame (the TAAU history handed to post as it is). This drive does the
//! same on lavapipe: a level warm (`prewarm`, `warm_scene`), a moving warm-up, then a cut, a
//! held camera until it freezes, a decal, another cut and moving frames. It counts the
//! renderer's frame-path calls (`frame_path_counters`, this thread) and asserts none after
//! the warm-up. Fixed frame counts, no wall clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use regia_render::{
    DirectionalLightUniform, FramePathCounters, FramePathKind, FrameQuality, FrameRenderer, LabMaterial,
    LabMesh, LabScene, LabVertex, LayerCamera, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, ScreenPassReport, TimeOfDay, TranslucentLayer, VolumetricFogSettings,
    frame_path_counters, take_frame_path_events, trace_frame_path,
};

const W: u32 = 160;
const H: u32 = 90;
/// Moving frames after the level warm, before counting starts (the soak's warm-up).
const WARMUP: usize = 12;
/// Held frames: the 8 the GPU irradiance probes take to settle (`DDGI_REUSE_FRAMES`), then
/// past the 32 a TAAU view takes to converge, so the view freezes (as in `held_aa`).
const HELD: usize = 56;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// An effect layer that draws nothing but reports decals once `decals` is set: the frame then
/// stamps the decal receiver, as the soak's first VFX decal does.
struct DecalLayer {
    decals: Arc<AtomicBool>,
}

impl TranslucentLayer for DecalLayer {
    fn simulate(
        &self,
        _depth: &wgpu::TextureView,
        _camera: &LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    // Like the effect runtime: the layer draws into the HDR image before the post stack, so a
    // TAAU frame's post input is a copy of the resolve.
    fn participates_in_hdr(&self) -> bool {
        true
    }

    fn has_decals(&self) -> bool {
        self.decals.load(Ordering::Relaxed)
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

/// Ground, a wall behind haze and a slab in front, seen from `eye`.
fn scene(eye: [f32; 3]) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target: [eye[0], 4.0, eye[2] - 60.0],
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
        materials: vec![
            LabMaterial {
                base_color: [0.4, 0.3, 0.25, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 0.5,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
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
                    [-150.0, 0.0, -120.0],
                    [150.0, 0.0, -120.0],
                    [150.0, 60.0, -120.0],
                    [-150.0, 60.0, -120.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
            quad(
                [
                    [-3.0, 0.0, -12.0],
                    [3.0, 0.0, -12.0],
                    [3.0, 4.0, -12.0],
                    [-3.0, 4.0, -12.0],
                ],
                [0.0, 0.0, 1.0],
                1,
            ),
        ],
    }
}

/// Desktop High with TAAU: the soak's tier (static-screen reuse, held-view freeze).
fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = true;
    q
}

/// Auto exposure on (the histogram), bloom on: the soak's post chain.
fn post() -> PostSettings {
    PostSettings {
        taa: false,
        ..PostSettings::default()
    }
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

struct Drive {
    device: RenderDevice,
    renderer: FrameRenderer,
    decals: Arc<AtomicBool>,
    post: PostSettings,
}

impl Drive {
    /// A renderer after the level warm: `prewarm` with the scene's meshes and `warm_scene`.
    fn warmed() -> Self {
        let device = device();
        let decals = Arc::new(AtomicBool::new(false));
        let mut renderer = FrameRenderer::new();
        renderer.set_translucent_layers(vec![Arc::new(DecalLayer {
            decals: Arc::clone(&decals),
        })]);
        renderer.set_outdoor(outdoor());
        let post = post();
        let lab = scene([0.0, 1.8, 0.0]);
        let mut frame = lab.as_frame_scene_with_quality(quality());
        frame.post = Some(&post);
        renderer.prewarm(&device, &frame.meshes, 64).expect("prewarm");
        renderer
            .warm_scene(&device, &frame, wgpu::TextureFormat::Rgba8UnormSrgb, true)
            .expect("warm scene");
        Self {
            device,
            renderer,
            decals,
            post,
        }
    }

    /// One frame seen from `eye`.
    fn frame(&mut self, eye: [f32; 3]) {
        let lab = scene(eye);
        let mut frame = lab.as_frame_scene_with_quality(quality());
        frame.post = Some(&self.post);
        self.renderer
            .render_offscreen(&self.device, &frame)
            .expect("frame");
    }

    /// `n` frames moving along the road from `start`.
    fn moving(&mut self, start: [f32; 3], n: usize) {
        for i in 0..n {
            let step = i as f32 * 0.4;
            self.frame([start[0] + step * 0.3, start[1], start[2] - step]);
        }
    }
}

/// Every frame-path create the closure's frames made: `(kind, label)`.
fn creates_during(drive: &mut Drive, run: impl FnOnce(&mut Drive)) -> (FramePathCounters, Vec<String>) {
    let before = frame_path_counters();
    trace_frame_path(true);
    run(drive);
    let events = take_frame_path_events();
    trace_frame_path(false);
    let counters = frame_path_counters().since(before);
    let labels = events
        .iter()
        .filter(|e| e.kind != FramePathKind::BlockingPoll)
        .map(|e| format!("{:?} {:?}", e.kind, e.label))
        .collect();
    (counters, labels)
}

#[test]
fn a_warm_drive_with_decals_cuts_and_a_held_view_creates_no_bind_group() {
    let mut drive = Drive::warmed();
    drive.moving([0.0, 1.8, 0.0], WARMUP);
    let (counters, labels) = creates_during(&mut drive, |drive| {
        // A cut to a held camera: no fog history on the cut frame, then the view freezes.
        drive.renderer.camera_cut();
        for _ in 0..HELD {
            drive.frame([6.0, 2.5, -4.0]);
        }
        // A frozen held frame runs no screen pass: the resolve's last output goes to post.
        assert_eq!(
            drive.renderer.screen_pass_report(),
            ScreenPassReport::default(),
            "the held view did not freeze, so this drive does not cover it"
        );
        // The first projected decal of the level.
        drive.decals.store(true, Ordering::Relaxed);
        drive.moving([6.0, 2.5, -4.0], 4);
        // Another cut, then moving again.
        drive.renderer.camera_cut();
        drive.moving([-5.0, 1.8, 3.0], 6);
    });
    println!("after warm-up: {counters:?}\n{labels:#?}");
    assert_eq!(
        counters.bind_groups, 0,
        "bind groups created after warm-up: {labels:#?}"
    );
    assert_eq!(
        counters.pipelines, 0,
        "pipelines created after warm-up: {labels:#?}"
    );
}

/// The soak's plate cameras, in turn: a cut to a held camera until it freezes, a cut back to
/// moving frames, then a cut to another held camera. The DO27 soak created `hist bg` ×3 and
/// `post bg` on the first frozen frame of its second plate camera (ADR-2585); the first freeze
/// of a session created nothing, so the drive above did not see it.
#[test]
fn a_second_held_view_after_moving_frames_creates_no_bind_group() {
    let mut drive = Drive::warmed();
    drive.moving([0.0, 1.8, 0.0], WARMUP);
    let (counters, labels) = creates_during(&mut drive, |drive| {
        for plate in 0..3 {
            drive.renderer.camera_cut();
            for _ in 0..HELD {
                drive.frame([6.0 + plate as f32 * 7.0, 2.5, -4.0]);
            }
            assert_eq!(
                drive.renderer.screen_pass_report(),
                ScreenPassReport::default(),
                "plate {plate}: the held view did not freeze, so this drive does not cover it"
            );
            drive.renderer.camera_cut();
            drive.moving([-5.0, 1.8, 3.0], 20);
        }
    });
    println!("after warm-up: {counters:?}\n{labels:#?}");
    assert_eq!(
        counters.bind_groups, 0,
        "bind groups created after warm-up: {labels:#?}"
    );
    assert_eq!(
        counters.pipelines, 0,
        "pipelines created after warm-up: {labels:#?}"
    );
}
