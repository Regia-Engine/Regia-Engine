// SPDX-License-Identifier: MPL-2.0
//! The precipitation occlusion view's redraw cadence on a drive (spec 99 §§ 5.6, 12; ADR-2585).
//!
//! § 12 expects the view to redraw "once every several seconds for a walking camera; a car at
//! 20 m/s snaps every 0.4 s": about once per origin snap. A vehicle driving under the eye is a
//! caster over the square whose pose changes every frame, and keying it by its exact pose redrew
//! the view on every frame of the DO27 chase (608 of 662). This drive moves a vehicle and the
//! camera behind it at 20 m/s for 2 s at a fixed 60 Hz step over a 64 m view (8 m snap), then
//! parks. Counters only (`PrecipOcclusionInfo::redraws`), no wall clock.

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, RenderDevice, SurfaceWeatherFrame,
};

const W: u32 = 64;
const H: u32 = 64;
/// The view's side (m); it snaps to an eighth of it.
const EXTENT_M: f32 = 64.0;
/// The drive: 20 m/s at a 60 Hz step for 120 frames, 40 m.
const SPEED_M_PER_FRAME: f32 = 20.0 / 60.0;
const DRIVE_FRAMES: u32 = 120;
/// Parked frames: past the 30-frame settle window of a caster that stopped moving.
const PARKED_FRAMES: u32 = 40;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    (
        corners
            .into_iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// A floor and a static roof slab, seen from `eye`.
fn scene(eye: [f32; 3]) -> LabScene {
    let (floor_v, floor_i) = quad(
        [
            [-60.0, 0.0, -60.0],
            [60.0, 0.0, -60.0],
            [60.0, 0.0, 60.0],
            [-60.0, 0.0, 60.0],
        ],
        [0.0, 1.0, 0.0],
    );
    let (roof_v, roof_i) = quad(
        [
            [-6.0, 3.0, -3.0],
            [-1.0, 3.0, -3.0],
            [-1.0, 3.0, 3.0],
            [-6.0, 3.0, 3.0],
        ],
        [0.0, -1.0, 0.0],
    );
    LabScene {
        width: W,
        height: H,
        eye,
        target: [eye[0], 0.0, eye[2] - 8.0],
        fov_y: 0.9,
        near: 0.1,
        far: 120.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![
            LabMesh {
                vertices: floor_v,
                indices: floor_i,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: roof_v,
                indices: roof_i,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// Redraws after the drive, and after parking.
fn drive(quality: FrameQuality) -> (u64, u64) {
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(SurfaceWeatherFrame {
        wetness: 1.0,
        puddles: 0.0,
        snow_cover: 0.0,
        ripple: 0.0,
        clock_s: 0.0,
    });
    renderer.set_precipitation_occlusion_budget(Some((256, EXTENT_M)), 16_384);
    // The vehicle: a 4 m x 2 m roof at 1.5 m, moved by its model matrix.
    let (vertices, indices) = quad(
        [
            [-2.0, 1.5, -1.0],
            [2.0, 1.5, -1.0],
            [2.0, 1.5, 1.0],
            [-2.0, 1.5, 1.0],
        ],
        [0.0, 1.0, 0.0],
    );
    let material = LabMaterial::default();
    let mut frame = |z: f32| {
        // The chase camera 6 m behind and 5 m above the vehicle.
        let lab = scene([0.0, 5.0, z + 6.0]);
        let mut frame_scene = lab.as_frame_scene_with_quality(quality);
        let template = frame_scene.meshes[0];
        let mut model = regia_core::glam::Mat4::IDENTITY.to_cols_array_2d();
        model[3][2] = z;
        frame_scene.meshes.push(FrameMesh {
            mesh_key: 0x7720 << 3,
            object_key: 0x7720,
            material_key: 0x7721,
            material: &material,
            vertices: &vertices,
            indices: &indices,
            model,
            ..template
        });
        if let Err(err) = renderer.render_offscreen(&device, &frame_scene) {
            panic!("frame: {err}");
        }
        match renderer.precipitation_occlusion() {
            Some(info) => info.redraws,
            None => panic!("the wet frame has no occlusion view"),
        }
    };
    let start_z = 4.0;
    let mut redraws = 0;
    for i in 0..DRIVE_FRAMES {
        redraws = frame(start_z - i as f32 * SPEED_M_PER_FRAME);
    }
    let parked_z = start_z - (DRIVE_FRAMES - 1) as f32 * SPEED_M_PER_FRAME;
    let mut parked = redraws;
    for _ in 0..PARKED_FRAMES {
        parked = frame(parked_z);
    }
    (redraws, parked)
}

fn check(quality: FrameQuality) {
    let (driven, parked) = drive(quality);
    let snap = EXTENT_M / 8.0;
    let distance = (DRIVE_FRAMES - 1) as f32 * SPEED_M_PER_FRAME;
    let snaps = (distance / snap).ceil() as u64;
    // § 12: about one redraw per origin snap. The vehicle keys by its cell at the same snap, so it
    // adds at most one per cell it crosses on top (along one axis; its half size in cells stays
    // put), plus two at the start: the first draw, and the switch from its exact pose to its cell
    // once it moves. A margin of one snap's worth covers cell edges that do not line up with
    // the origin's.
    let bound = 2 + 3 * snaps;
    assert!(
        driven <= bound,
        "{driven} redraws in {DRIVE_FRAMES} frames over {distance} m (bound {bound}: {snaps} snaps)"
    );
    assert!(
        driven >= snaps,
        "the view still follows the eye: {driven} redraws for {snaps} snaps"
    );
    // Parked past the settle window: one redraw back to the exact pose, then held.
    assert_eq!(
        parked,
        driven + 1,
        "a parked vehicle settles to its exact shape once"
    );
}

#[test]
fn a_drive_redraws_the_view_about_once_per_snap_when_unpaged() {
    check(FrameQuality::LAB);
}

#[test]
fn a_drive_redraws_the_view_about_once_per_snap_when_paged() {
    check(FrameQuality {
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    });
}
