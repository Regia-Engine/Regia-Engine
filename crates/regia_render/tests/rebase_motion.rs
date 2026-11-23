// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 18 test 9, the read-back half: on the frame a floating-origin rebase moves the camera
//! and every static object by the same whole number of cells, the velocity pass writes no motion.
//!
//! The rebase is what `regia_world` does in `StreamSet::Rebase`: every position goes down by `shift`
//! and the renderer's history is re-expressed with `FrameRenderer::shift_history`.
//!
//! A draw that was on screen last frame cannot show the difference: its previous model and the
//! previous view-projection are both old, so the pair still projects to the same place even with no
//! history shift. A draw that **first appears on the rebase frame** can: it has no previous model, so
//! its current model goes through the previous view-projection (a cell that streams in as the origin
//! moves does exactly this). The control frame skips the history shift and must show the jump on that
//! draw, which proves the read-back sees it.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_OPAQUE, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabVertex, MotionField, PostSettings, RenderDevice, ShadowCast,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

const W: u32 = 96;
const H: u32 = 64;
/// Where the scene sits before the rebase, about 4 km from the origin as a streamed level has it.
const SITE: [f32; 3] = [4020.0, 2.0, 3004.0];
/// A rebase by (4, 0, 3) cells of 1024 m: exact in `f32`.
const SHIFT: [f32; 3] = [4096.0, 0.0, 3072.0];

struct Rig {
    cube: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
    post: PostSettings,
}

impl Rig {
    fn new() -> Self {
        Self {
            cube: regia_render::primitives::box_mesh([0.5, 0.5, 0.5]),
            material: LabMaterial {
                base_color: [0.7, 0.7, 0.7, 1.0],
                roughness: 1.0,
                alpha_mode: ALPHA_OPAQUE,
                ..LabMaterial::default()
            },
            post: PostSettings {
                exposure_ev: 14.5,
                ..PostSettings::golden_deterministic()
            },
        }
    }

    /// The scene with everything moved by `-offset`: three cubes and the camera looking at them, plus
    /// a fourth cube when `streamed_in` (it has no history in the renderer the first time).
    fn scene(&self, offset: [f32; 3], streamed_in: bool, depth_velocity: bool) -> FrameScene<'_> {
        let at = |dx: f32, dz: f32| {
            Vec3::new(
                SITE[0] + dx - offset[0],
                SITE[1] - offset[1],
                SITE[2] + dz - offset[2],
            )
        };
        let mut quality = FrameQuality::LAB;
        quality.taau = true;
        quality.depth_velocity = depth_velocity;
        let fourth = streamed_in.then_some((0.0, 1.0, 4u64));
        let meshes = [(-1.5, 0.0, 1u64), (0.0, -1.0, 2), (1.5, 0.5, 3)]
            .into_iter()
            .chain(fourth)
            .map(|(dx, dz, key)| FrameMesh {
                is_static: false,
                mesh_key: 0xC0BE,
                material_key: 1,
                object_key: key,
                vertices: &self.cube.0,
                indices: &self.cube.1,
                material: &self.material,
                model: Mat4::from_translation(at(dx, dz)).to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            })
            .collect();
        let eye = at(0.3, 5.0) + Vec3::new(0.0, 1.5, 0.0);
        FrameScene {
            width: W,
            height: H,
            eye: eye.to_array(),
            target: at(0.0, 0.0).to_array(),
            fov_y: 0.9,
            near: 0.05,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.5,
            ambient_lux: 200.0,
            double_sided: false,
            post: Some(&self.post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
            local_lights: &[],
            quality,
            meshes,
        }
    }
}

fn motion(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> MotionField {
    renderer.render_offscreen(device, scene).expect("frame");
    renderer
        .read_motion(device)
        .expect("read motion")
        .expect("velocity target")
}

fn max_len(field: &MotionField) -> f32 {
    field.motion.iter().map(|m| m[0].hypot(m[1])).fold(0.0, f32::max)
}

/// The frames of the test with one velocity mode: per-draw, or the depth-reprojected path that skips
/// draws whose model did not move.
fn run(rig: &Rig, device: &RenderDevice, depth_velocity: bool) {
    // Three still frames settle the history; the last one writes no motion.
    let mut shifted = FrameRenderer::new();
    for _ in 0..3 {
        motion(&mut shifted, device, &rig.scene([0.0; 3], false, depth_velocity));
    }
    let still = max_len(&motion(
        &mut shifted,
        device,
        &rig.scene([0.0; 3], false, depth_velocity),
    ));

    // The rebase frame: positions down by the shift, history shifted with them, one new draw.
    shifted.shift_history(SHIFT);
    let rebased = max_len(&motion(
        &mut shifted,
        device,
        &rig.scene(SHIFT, true, depth_velocity),
    ));
    // And the frame after it, in the new space.
    let after = max_len(&motion(
        &mut shifted,
        device,
        &rig.scene(SHIFT, true, depth_velocity),
    ));

    // The rebase frame is not taken for a camera cut (the eye moved 5 km in local space).
    assert_eq!(shifted.camera_cut_count(), 0, "{:?}", shifted.last_camera_cut());

    // Control: the same frames without the history shift see the whole jump on the new draw. Its cut
    // detection is off, or the jump would reset the history and hide the motion.
    let mut control = FrameRenderer::new();
    control.set_camera_cut_detection(None);
    for _ in 0..4 {
        motion(&mut control, device, &rig.scene([0.0; 3], false, depth_velocity));
    }
    let jumped = max_len(&motion(
        &mut control,
        device,
        &rig.scene(SHIFT, true, depth_velocity),
    ));

    assert!(
        jumped > 0.05,
        "the read-back must see an unshifted rebase: {jumped}"
    );
    // A matrix product at 4 km in f32 is not exact; what is left stays a thousand times below the
    // jump and under the static bound the skinned-velocity test uses (UV units: a tenth of a pixel).
    for (name, value) in [
        ("still", still),
        ("rebase frame", rebased),
        ("frame after", after),
    ] {
        assert!(value < jumped * 1.0e-3, "{name}: {value} against {jumped}");
        assert!(value < 1.0e-3, "{name}: {value}");
    }
    eprintln!(
        "rebase motion (depth velocity {depth_velocity}): still {still}, rebase frame {rebased}, after {after}, \
         unshifted jump {jumped}"
    );
}

#[test]
fn a_rebase_frame_writes_no_motion_for_static_content() {
    let device = device();
    let rig = Rig::new();
    run(&rig, &device, false);
    run(&rig, &device, true);
}
