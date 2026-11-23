// SPDX-License-Identifier: MPL-2.0
//! The frame's reusable lists stop growing once a scene settles (ADR-385).
//!
//! `FrameRenderer::frame_scratch_stats` counts every time one of `render_into`'s light,
//! shadow-caster, culling and object lists (and the GPU culler's upload lists) had to grow. A
//! scene drawn again and again, with a held or a moving camera, must stop growing after its
//! first frames: the steady frame allocates none of those lists. A larger scene grows them once,
//! which also shows the counter is live.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, JointMatrix, LabLocalLight,
    LabMaterial, LabMesh, LabPointLight, LabScene, LabSpotLight, LabVertex, RenderDevice,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad at `(x, y)`, `size` metres, every vertex weighted to joint 0 when skinned.
fn quad(x: f32, y: f32, size: f32, skinned: bool) -> Vec<LabVertex> {
    let h = size * 0.5;
    let mut vertices = vec![
        LabVertex::from_pos_normal_uv([x - h, y - h, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + h, y - h, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + h, y + h, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x - h, y + h, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
    ];
    if skinned {
        for v in &mut vertices {
            v.joints = [0, 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
        }
    }
    vertices
}

/// `rows` rows of eight quads (every fourth one skinned), a shadowed point and spot light, and
/// a shadowed sun: every list the frame builds has entries.
fn lab(rows: usize) -> LabScene {
    let mut meshes = Vec::new();
    for row in 0..rows {
        for column in 0..8 {
            let skinned = column % 4 == 3;
            meshes.push(LabMesh {
                vertices: quad(column as f32 * 0.4 - 1.4, row as f32 * 0.4 - 0.8, 0.3, skinned),
                indices: vec![0, 1, 2, 0, 2, 3],
                joints: if skinned {
                    vec![JointMatrix::translation(0.0, 0.05, 0.0)]
                } else {
                    Vec::new()
                },
                ..LabMesh::unskinned()
            });
        }
    }
    LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 0.0, 4.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: vec![
            LabLocalLight::Point(LabPointLight {
                position: [0.5, 0.5, 1.0],
                color: [1.0, 0.9, 0.8],
                intensity: 2_000.0,
                radius: 6.0,
                cast_shadows: true,
                source_radius: None,
            }),
            LabLocalLight::Spot(LabSpotLight {
                position: [-0.5, 0.5, 2.0],
                direction: [0.0, -0.2, -1.0],
                color: [0.8, 0.9, 1.0],
                intensity: 3_000.0,
                range: 8.0,
                inner_angle: 0.3,
                outer_angle: 0.6,
                cast_shadows: true,
                source_radius: None,
            }),
        ],
        materials: vec![LabMaterial {
            base_color: [0.7, 0.6, 0.5, 1.0],
            ..LabMaterial::default()
        }],
        meshes,
    }
}

/// Stable keys per mesh index.
fn keyed(lab: &LabScene, quality: FrameQuality) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    for (i, mesh) in scene.meshes.iter_mut().enumerate() {
        mesh.mesh_key = 0x5C00 + i as u64;
        mesh.object_key = 0x5C_0000 + i as u64;
        mesh.material_key = 0xD2_0000;
    }
    scene
}

fn qualities() -> [FrameQuality; 3] {
    [
        FrameQuality::default(),
        FrameQuality {
            gpu_driven: true,
            ..FrameQuality::default()
        },
        FrameQuality {
            gpu_driven: true,
            paged_sun_shadows: true,
            ..FrameQuality::default()
        },
    ]
}

const WARM_FRAMES: u32 = 4;
const STEADY_FRAMES: u64 = 24;

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_held_camera_stops_growing_the_frame_lists() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab(3);
    for quality in qualities() {
        let scene = keyed(&lab, quality);
        let mut renderer = FrameRenderer::new();
        renderer.render_offscreen(&device, &scene).expect("first frame");
        let first = renderer.frame_scratch_stats();
        assert!(first.growths > 0, "the first frame fills its lists: {first:?}");
        // At least one: a frame may render a capture of its own before the view.
        assert!(first.frames >= 1, "{first:?}");
        for _ in 1..WARM_FRAMES {
            renderer.render_offscreen(&device, &scene).expect("warm frame");
        }
        let warm = renderer.frame_scratch_stats();
        for _ in 0..STEADY_FRAMES {
            renderer.render_offscreen(&device, &scene).expect("steady frame");
            assert_eq!(
                renderer.frame_scratch_stats().last_frame_growths,
                0,
                "gpu_driven {}, paged {}: a steady frame grew a list",
                quality.gpu_driven,
                quality.paged_sun_shadows
            );
        }
        let steady = renderer.frame_scratch_stats();
        assert_eq!(steady.growths, warm.growths, "{warm:?} -> {steady:?}");
        assert!(
            steady.frames >= warm.frames + STEADY_FRAMES,
            "{warm:?} -> {steady:?}"
        );
    }
}

#[test]
fn a_moving_camera_stops_growing_the_frame_lists() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab(3);
    for quality in qualities() {
        let mut renderer = FrameRenderer::new();
        // The camera orbits: cascades, caster lists and the cull change every frame. Two laps
        // size the lists for every pose and for the jump from the last pose back to the first;
        // a third lap over the same poses grows none.
        let lap = |renderer: &mut FrameRenderer| {
            for frame in 0..STEADY_FRAMES {
                let angle = frame as f32 * 0.05;
                let mut scene = keyed(&lab, quality);
                scene.eye = [4.0 * angle.sin(), 0.3, 4.0 * angle.cos()];
                renderer.render_offscreen(&device, &scene).expect("frame");
            }
        };
        lap(&mut renderer);
        lap(&mut renderer);
        let warm = renderer.frame_scratch_stats();
        lap(&mut renderer);
        let steady = renderer.frame_scratch_stats();
        assert_eq!(
            steady.growths, warm.growths,
            "gpu_driven {}, paged {}: {warm:?} -> {steady:?}",
            quality.gpu_driven, quality.paged_sun_shadows
        );
        assert!(steady.frames >= 3 * STEADY_FRAMES, "{steady:?}");
    }
}

#[test]
fn a_larger_scene_grows_the_lists_once_then_settles() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let small = lab(1);
    let large = lab(6);
    let quality = FrameQuality {
        gpu_driven: true,
        ..FrameQuality::default()
    };
    let mut renderer = FrameRenderer::new();
    for _ in 0..WARM_FRAMES {
        renderer
            .render_offscreen(&device, &keyed(&small, quality))
            .expect("small scene");
    }
    let small_steady = renderer.frame_scratch_stats();
    renderer
        .render_offscreen(&device, &keyed(&large, quality))
        .expect("large scene");
    let grown = renderer.frame_scratch_stats();
    assert!(
        grown.growths > small_steady.growths,
        "six times the meshes must grow a list: {small_steady:?} -> {grown:?}"
    );
    for _ in 0..WARM_FRAMES {
        renderer
            .render_offscreen(&device, &keyed(&large, quality))
            .expect("large scene");
    }
    let warm = renderer.frame_scratch_stats();
    for _ in 0..STEADY_FRAMES {
        renderer
            .render_offscreen(&device, &keyed(&large, quality))
            .expect("large scene");
    }
    assert_eq!(renderer.frame_scratch_stats().growths, warm.growths);
    // Back to the small scene: the lists already have the room.
    renderer
        .render_offscreen(&device, &keyed(&small, quality))
        .expect("small scene again");
    assert_eq!(renderer.frame_scratch_stats().last_frame_growths, 0);
}
