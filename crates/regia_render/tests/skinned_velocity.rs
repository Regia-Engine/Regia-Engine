// SPDX-License-Identifier: MPL-2.0
//! Skinned meshes write motion vectors (v0.1.14, DO27 playtest "moving unit blur").
//!
//! The velocity pass drew rigid meshes only: a skinned character (a 3P pilot walking) wrote no
//! motion, so TAA/TSR reprojected its history from where it no longer was and smeared it, while
//! rigid hulls in the same shot stayed sharp. The renderer now keeps last frame's joint palette
//! per object and skins each vertex with both palettes in the velocity pass.
//!
//! - `skinned_joints_write_motion_*`: a moved joint writes its motion, an unmoved joint and a
//!   static frame write none, a changed joint count, a mesh that reappears and a camera cut
//!   start from zero motion, two instances of one mesh batch with their own palettes, and the
//!   motion of a rigid mesh in the same frame is bit-identical to a frame without the skin.
//! - `tsr_keeps_a_moving_skinned_card_as_sharp_as_a_rigid_one`: a striped card sliding under
//!   TSR, moved by its joints, resolves like the same card moved by its model matrix. Without
//!   skinned motion (measured on lavapipe before the fix) the skinned card was 58 luma from the
//!   native frame against the rigid card's 11, with a ghost that doubled its stripe energy.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_OPAQUE, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    JointMatrix, LabMaterial, LabMesh, LabScene, LabVertex, MotionField, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, ShadowCast, TemporalUpscaler,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translate(x: f32, y: f32) -> [[f32; 4]; 4] {
    Mat4::from_translation(Vec3::new(x, y, 0.0)).to_cols_array_2d()
}

/// A quad facing +Z over `[x0, x1] × [y0, y1]`, appended to `vertices`/`indices`, every vertex
/// bound to `joints` with `weights`.
fn quad(
    vertices: &mut Vec<LabVertex>,
    indices: &mut Vec<u32>,
    (x0, x1, y0, y1): (f32, f32, f32, f32),
    joints: [u32; 4],
    weights: [f32; 4],
) {
    let base = vertices.len() as u32;
    for (p, uv) in [
        ([x0, y0, 0.0], [0.0, 1.0]),
        ([x1, y0, 0.0], [1.0, 1.0]),
        ([x1, y1, 0.0], [1.0, 0.0]),
        ([x0, y1, 0.0], [0.0, 0.0]),
    ] {
        let mut vertex = LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv);
        vertex.joints = joints;
        vertex.weights = weights;
        vertices.push(vertex);
    }
    indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

const W: u32 = 128;
const H: u32 = 96;
const EYE_Z: f32 = 3.0;
const FOV_Y: f32 = 0.9;

/// Width of the view at z = 0, metres.
fn view_width_m() -> f32 {
    2.0 * EYE_Z * (FOV_Y * 0.5).tan() * W as f32 / H as f32
}

/// Two quads in one skinned mesh: the left one on joint 0, the right one on joint 1.
fn limbs() -> (Vec<LabVertex>, Vec<u32>) {
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    quad(
        &mut vertices,
        &mut indices,
        (-1.4, -0.4, -0.3, 0.3),
        [0; 4],
        [1.0, 0.0, 0.0, 0.0],
    );
    quad(
        &mut vertices,
        &mut indices,
        (0.4, 1.4, -0.3, 0.3),
        [1, 0, 0, 0],
        [1.0, 0.0, 0.0, 0.0],
    );
    (vertices, indices)
}

fn bar() -> (Vec<LabVertex>, Vec<u32>) {
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    quad(
        &mut vertices,
        &mut indices,
        (-0.5, 0.5, -0.3, 0.3),
        [0; 4],
        [0.0; 4],
    );
    (vertices, indices)
}

/// Palette with joint 1 moved `dx` on X (`count` joints, the rest identity).
fn palette(dx: f32, count: usize) -> Vec<JointMatrix> {
    let mut joints = vec![JointMatrix::identity(); count];
    joints[1] = JointMatrix::translation(dx, 0.0, 0.0);
    joints
}

/// Rows of skinned instance A (y 0.85), B (y 0) and the rigid bar (y −0.95), with margins.
const ROWS_A: (u32, u32) = (6, 34);
const ROWS_B: (u32, u32) = (35, 62);
const ROWS_R: (u32, u32) = (65, 93);

/// One frame of the motion test: instance A's palette (`None` = A not submitted), the rigid
/// bar's X.
struct Step {
    a: Option<Vec<JointMatrix>>,
    bar_x: f32,
    cut: bool,
}

struct Rig {
    limbs: (Vec<LabVertex>, Vec<u32>),
    bar: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
    still: Vec<JointMatrix>,
    post: PostSettings,
}

impl Rig {
    fn new() -> Self {
        Self {
            limbs: limbs(),
            bar: bar(),
            material: LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 1.0,
                alpha_mode: ALPHA_OPAQUE,
                ..LabMaterial::default()
            },
            still: palette(0.0, 2),
            post: PostSettings {
                exposure_ev: 14.5,
                ..PostSettings::golden_deterministic()
            },
        }
    }

    fn scene<'a>(&'a self, step: &'a Step, skins: bool, depth_velocity: bool) -> FrameScene<'a> {
        let mut quality = FrameQuality::LAB;
        quality.taau = true;
        quality.depth_velocity = depth_velocity;
        let mesh = |mesh_key: u64, object_key: u64, geometry: &'a (Vec<LabVertex>, Vec<u32>)| FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 1,
            object_key,
            vertices: &geometry.0,
            indices: &geometry.1,
            material: &self.material,
            model: translate(0.0, 0.0),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        };
        let mut meshes = Vec::new();
        if skins {
            if let Some(a) = step.a.as_deref() {
                meshes.push(FrameMesh {
                    model: translate(0.0, 0.85),
                    joints: a,
                    ..mesh(0x51C1, 21, &self.limbs)
                });
            }
            // Instance B: the same mesh, never moving. It batches with A.
            meshes.push(FrameMesh {
                joints: &self.still,
                ..mesh(0x51C1, 22, &self.limbs)
            });
        }
        meshes.push(FrameMesh {
            model: translate(step.bar_x, -0.95),
            ..mesh(0xBA5, 23, &self.bar)
        });
        FrameScene {
            width: W,
            height: H,
            eye: [0.0, 0.0, EYE_Z],
            target: [0.0, 0.0, 0.0],
            fov_y: FOV_Y,
            near: 0.05,
            far: 50.0,
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

/// Pixels of `rows` and the left (`right = false`) or right half.
fn region(field: &MotionField, rows: (u32, u32), right: bool) -> Vec<[f32; 2]> {
    let mid = field.width / 2;
    let (x0, x1) = if right { (mid, field.width) } else { (0, mid) };
    (rows.0..rows.1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .map(|(x, y)| field.motion[(y * field.width + x) as usize])
        .collect()
}

fn max_len(motion: &[[f32; 2]]) -> f32 {
    motion.iter().map(|m| m[0].hypot(m[1])).fold(0.0, f32::max)
}

/// Median X motion of the pixels that moved, and how many moved.
fn moved_x(motion: &[[f32; 2]]) -> (f32, usize) {
    let mut xs: Vec<f32> = motion
        .iter()
        .filter(|m| m[0].hypot(m[1]) > 0.005)
        .map(|m| m[0])
        .collect();
    xs.sort_by(f32::total_cmp);
    (xs.get(xs.len() / 2).copied().unwrap_or(0.0), xs.len())
}

fn skinned_joints_write_motion(depth_velocity: bool) {
    let device = device();
    let rig = Rig::new();
    let steps = [
        // 0: first frame, no previous palette.
        Step {
            a: Some(palette(0.0, 2)),
            bar_x: 0.0,
            cut: false,
        },
        // 1: joint 1 moves 0.3 m, the bar 0.2 m.
        Step {
            a: Some(palette(0.3, 2)),
            bar_x: 0.2,
            cut: false,
        },
        // 2: nothing moves.
        Step {
            a: Some(palette(0.3, 2)),
            bar_x: 0.2,
            cut: false,
        },
        // 3: the palette grows to three joints (joint 1 moves too): no previous palette.
        Step {
            a: Some(palette(0.1, 3)),
            bar_x: 0.2,
            cut: false,
        },
        // 4: A is not drawn.
        Step {
            a: None,
            bar_x: 0.2,
            cut: false,
        },
        // 5: A is back, moved: no previous palette (it did not draw last frame).
        Step {
            a: Some(palette(0.2, 2)),
            bar_x: 0.2,
            cut: false,
        },
        // 6: joint 1 moves 0.15 m.
        Step {
            a: Some(palette(0.35, 2)),
            bar_x: 0.2,
            cut: false,
        },
        // 7: a camera cut before a moved joint: no previous palette.
        Step {
            a: Some(palette(0.0, 2)),
            bar_x: 0.2,
            cut: true,
        },
    ];
    let mut skinned = FrameRenderer::new();
    let mut rigid_only = FrameRenderer::new();
    let width = view_width_m();
    for (i, step) in steps.iter().enumerate() {
        if step.cut {
            skinned.camera_cut();
            rigid_only.camera_cut();
        }
        let field = motion(&mut skinned, &device, &rig.scene(step, true, depth_velocity));
        let reference = motion(&mut rigid_only, &device, &rig.scene(step, false, depth_velocity));
        let (a_left, a_right) = (region(&field, ROWS_A, false), region(&field, ROWS_A, true));
        let b = [region(&field, ROWS_B, false), region(&field, ROWS_B, true)].concat();
        eprintln!(
            "depth_velocity {depth_velocity} step {i}: A left {:.4}, A right {:.4} {:?}, B {:.4}, bar {:?}",
            max_len(&a_left),
            max_len(&a_right),
            moved_x(&a_right),
            max_len(&b),
            moved_x(&[region(&field, ROWS_R, false), region(&field, ROWS_R, true)].concat()),
        );
        // The rigid bar's motion does not depend on the skinned meshes beside it.
        let bar_rows = |f: &MotionField| -> Vec<[u32; 2]> {
            [region(f, ROWS_R, false), region(f, ROWS_R, true)]
                .concat()
                .iter()
                .map(|m| [m[0].to_bits(), m[1].to_bits()])
                .collect()
        };
        assert!(
            bar_rows(&field) == bar_rows(&reference),
            "step {i}: the rigid bar's motion changed with a skinned mesh in the frame"
        );
        assert!(
            max_len(&b) < 1e-3,
            "step {i}: static instance B wrote motion {}",
            max_len(&b)
        );
        assert!(
            max_len(&a_left) < 1e-3,
            "step {i}: A's unmoved joint 0 wrote motion {}",
            max_len(&a_left)
        );
        let expected = match i {
            1 => Some(0.3),
            6 => Some(0.15),
            _ => None,
        };
        match expected {
            Some(dx) => {
                // `previous uv − current uv`: a move to +X is a negative X.
                let want = -dx / width;
                let (got, moved) = moved_x(&a_right);
                assert!(
                    moved > 300,
                    "step {i}: only {moved} px of A's moved joint wrote motion (skinned velocity missing?)"
                );
                assert!(
                    (got - want).abs() <= want.abs() * 0.05,
                    "step {i}: A's moved joint wrote {got:.5}, want {want:.5}"
                );
            }
            None => assert!(
                max_len(&a_right) < 1e-3,
                "step {i}: A's joint 1 has no previous pose to move from, wrote {}",
                max_len(&a_right)
            ),
        }
        if i == 1 {
            let (got, moved) =
                moved_x(&[region(&field, ROWS_R, false), region(&field, ROWS_R, true)].concat());
            let want = -0.2 / width;
            assert!(
                moved > 300 && (got - want).abs() <= want.abs() * 0.05,
                "bar motion {got} ({moved} px)"
            );
        }
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn skinned_joints_write_motion_per_mesh_velocity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    skinned_joints_write_motion(false);
}

#[test]
fn skinned_joints_write_motion_depth_velocity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    skinned_joints_write_motion(true);
}

// ---- DO27 joint counts and the joint buffer growing ----

/// `palettes.len()` instances of the limbs mesh at one spot (object keys 100..), one palette each.
fn crowd_scene<'a>(rig: &'a Rig, palettes: &'a [Vec<JointMatrix>], depth_velocity: bool) -> FrameScene<'a> {
    let mut quality = FrameQuality::LAB;
    quality.taau = true;
    quality.depth_velocity = depth_velocity;
    let meshes = palettes
        .iter()
        .enumerate()
        .map(|(i, joints)| FrameMesh {
            is_static: false,
            mesh_key: 0x51C1,
            material_key: 1,
            object_key: 100 + i as u64,
            vertices: &rig.limbs.0,
            indices: &rig.limbs.1,
            material: &rig.material,
            model: translate(0.0, 0.85),
            joints,
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    FrameScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, EYE_Z],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.05,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 200.0,
        double_sided: false,
        post: Some(&rig.post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: &[],
        quality,
        meshes,
    }
}

/// DO27's skeletons: eight pilots plus first-person arms on 67 joints (animated every frame,
/// then a static pose), helmet-off pilots on 80, and the 256-joint per-skin cap. The crowds
/// grow the velocity joint buffer past its 128 KiB floor (six 256-joint instances need 144 KiB):
/// the buffer and the object bind group are rebuilt, and motion stays right after the growth.
fn do27_joint_counts(depth_velocity: bool) {
    let device = device();
    let rig = Rig::new();
    let width = view_width_m();
    let mut renderer = FrameRenderer::new();
    // (instances, joints, joint 1 offset, what the moved joint wrote: true = motion expected)
    let steps: [(usize, usize, f32, bool); 12] = [
        (9, 67, 0.0, false), // first frame: no previous palette
        (9, 67, 0.1, true),  // animated every frame ...
        (9, 67, 0.2, true),
        (9, 67, 0.3, true),
        (9, 67, 0.3, false), // ... then a static pose
        (9, 80, 0.3, false), // helmet-off: the count changed, no previous palette
        (9, 80, 0.45, true),
        (9, 80, 0.45, false),
        (3, 256, 0.45, false), // the cap: new count
        (3, 256, 0.6, true),
        (6, 256, 0.6, false), // 144 KiB of palettes: the joint buffer grows (more instances)
        (6, 256, 0.75, true),
    ];
    let mut last = 0.0_f32;
    for (i, &(instances, joints, dx, moves)) in steps.iter().enumerate() {
        let palettes: Vec<Vec<JointMatrix>> = (0..instances).map(|_| palette(dx, joints)).collect();
        let field = motion(
            &mut renderer,
            &device,
            &crowd_scene(&rig, &palettes, depth_velocity),
        );
        let a_left = region(&field, ROWS_A, false);
        let a_right = region(&field, ROWS_A, true);
        let (got, moved) = moved_x(&a_right);
        eprintln!(
            "depth_velocity {depth_velocity} step {i}: {instances} x {joints} joints, dx {dx}: right {got:.5} ({moved} px)"
        );
        assert!(
            max_len(&a_left) < 1e-3,
            "step {i}: the unmoved joint 0 wrote motion {}",
            max_len(&a_left)
        );
        if moves {
            let want = -(dx - last) / width;
            assert!(
                moved > 300,
                "step {i}: {instances} x {joints} joints: only {moved} px wrote motion"
            );
            assert!(
                (got - want).abs() <= want.abs() * 0.05,
                "step {i}: {instances} x {joints} joints wrote {got:.5}, want {want:.5}"
            );
        } else {
            assert!(
                max_len(&a_right) < 1e-3,
                "step {i}: {instances} x {joints} joints wrote motion {} with nothing moving",
                max_len(&a_right)
            );
        }
        last = dx;
    }
}

#[test]
fn do27_joint_counts_per_mesh_velocity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    do27_joint_counts(false);
}

#[test]
fn do27_joint_counts_depth_velocity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    do27_joint_counts(true);
}

// ---- TSR: a moving striped card, skinned vs rigid ----

const TW: u32 = 160;
const TH: u32 = 90;
const DIST: f32 = 8.0;

fn px_m() -> f32 {
    2.0 * DIST * (FOV_Y * 0.5).tan() / TH as f32
}

/// Card stripe rows and its start column (pixels at the start pose); stripes are `STRIPE_PX`
/// wide with gaps as wide.
const CARD_ROWS: (u32, u32) = (30, 60);
const CARD_X0: f32 = 20.0;
const CARD_STRIPES: u32 = 10;
const STRIPE_PX: u32 = 4;
/// Pixels per frame and frames: the last pose is pixel-aligned (40 px).
const STEP_PX: f32 = 2.5;
const FRAMES: usize = 17;

/// Dark wall (mesh 0) and the bright stripes of the card (mesh 1), bound 50/50 to joints 0 and
/// 1 when `skinned` (a two-joint palette never folds into the model matrix).
fn card_scene(skinned: bool) -> LabScene {
    let m = px_m();
    let wx = |px: f32| (px - TW as f32 * 0.5) * m;
    let wy = |py: f32| -(py - TH as f32 * 0.5) * m;
    let mut wall = LabMesh {
        material: 0,
        ..LabMesh::unskinned()
    };
    quad(
        &mut wall.vertices,
        &mut wall.indices,
        (-20.0, 20.0, -10.0, 10.0),
        [0; 4],
        [0.0; 4],
    );
    for v in &mut wall.vertices {
        v.position[2] = -0.01;
    }
    let mut card = LabMesh {
        material: 1,
        ..LabMesh::unskinned()
    };
    let (joints, weights) = if skinned {
        ([0, 1, 0, 0], [0.5, 0.5, 0.0, 0.0])
    } else {
        ([0; 4], [0.0; 4])
    };
    for s in 0..CARD_STRIPES {
        let x = CARD_X0 + (2 * STRIPE_PX * s) as f32;
        quad(
            &mut card.vertices,
            &mut card.indices,
            (
                wx(x),
                wx(x + STRIPE_PX as f32),
                wy(CARD_ROWS.1 as f32),
                wy(CARD_ROWS.0 as f32),
            ),
            joints,
            weights,
        );
    }
    LabScene {
        width: TW,
        height: TH,
        eye: [0.0, 0.0, DIST],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.05, 0.05, 0.05, 1.0],
                unlit: true,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.9, 0.9, 1.0],
                unlit: true,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![wall, card],
    }
}

fn tsr_quality(taa: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.screen_space_gi = false;
    q.ssr = false;
    q.ssao = regia_render::SsaoMode::Off;
    q.gi = regia_render::GiMode::Ambient;
    q.reflection_probes = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.taau = taa;
    q
}

/// The last frame's luma of the card sliding `STEP_PX` per frame for `frames` frames.
fn slide(device: &RenderDevice, skinned: bool, taa: bool, frames: usize) -> Vec<f32> {
    let lab = card_scene(skinned);
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    let shifts: Vec<f32> = (0..frames).map(|i| i as f32 * STEP_PX * px_m()).collect();
    let palettes: Vec<Vec<JointMatrix>> = shifts
        .iter()
        .map(|&dx| vec![JointMatrix::translation(dx, 0.0, 0.0); 2])
        .collect();
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(TemporalUpscaler::Tsr);
    let mut frame = lab.as_frame_scene_with_quality(tsr_quality(taa));
    frame.post = Some(&post);
    let mut last = Vec::new();
    for (dx, joints) in shifts.iter().zip(&palettes) {
        if skinned {
            frame.meshes[1].joints = joints;
        } else {
            frame.meshes[1].model = translate(*dx, 0.0);
        }
        let image = renderer.render_offscreen(device, &frame).expect("card frame");
        last = image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
            .collect();
    }
    last
}

/// Mean squared horizontal difference over the card's last pose.
fn stripe_energy(l: &[f32]) -> f32 {
    let x0 = (CARD_X0 + STEP_PX * (FRAMES - 1) as f32) as u32;
    let x1 = x0 + 2 * STRIPE_PX * CARD_STRIPES - 2;
    let (mut sum, mut n) = (0.0, 0.0);
    for y in CARD_ROWS.0 + 2..CARD_ROWS.1 - 2 {
        for x in x0..x1 {
            let i = (y * TW + x) as usize;
            let d = l[i + 1] - l[i];
            sum += d * d;
            n += 1.0;
        }
    }
    sum / n
}

/// Mean |difference| from `reference` over the card's last pose and the 8 px it left behind.
fn card_error(l: &[f32], reference: &[f32]) -> f32 {
    let x1 = (CARD_X0 + STEP_PX * (FRAMES - 1) as f32) as u32 + 2 * STRIPE_PX * CARD_STRIPES;
    let x0 = x1 - 2 * STRIPE_PX * CARD_STRIPES - 8;
    let (mut sum, mut n) = (0.0, 0.0);
    for y in CARD_ROWS.0..CARD_ROWS.1 {
        for x in x0..x1 {
            let i = (y * TW + x) as usize;
            sum += (l[i] - reference[i]).abs();
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn tsr_keeps_a_moving_skinned_card_as_sharp_as_a_rigid_one() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let native = slide(&device, false, false, FRAMES);
    let rigid = slide(&device, false, true, FRAMES);
    let skinned = slide(&device, true, true, FRAMES);
    let (e_native, e_rigid, e_skinned) = (
        stripe_energy(&native),
        stripe_energy(&rigid),
        stripe_energy(&skinned),
    );
    let (err_rigid, err_skinned) = (card_error(&rigid, &native), card_error(&skinned, &native));
    eprintln!(
        "stripe energy: native {e_native:.1}, TSR rigid {e_rigid:.1} ({:.3}), TSR skinned {e_skinned:.1} ({:.3}); \
         card error vs native: rigid {err_rigid:.2}, skinned {err_skinned:.2}",
        e_rigid / e_native,
        e_skinned / e_native,
    );
    assert!(e_native > 1000.0, "the native card has no stripes ({e_native})");
    // Two-sided: a smeared card loses energy, a ghosted one (history fetched from where the
    // card was) gains edges. Without skinned motion the ghost read 2.3× native here.
    assert!(
        (e_skinned / e_rigid - 1.0).abs() <= 0.1,
        "TSR resolves the skinned card unlike the rigid one: stripe energy {e_skinned:.1} vs {e_rigid:.1}"
    );
    assert!(
        err_skinned <= err_rigid * 1.25 + 1.0,
        "TSR's skinned card is {err_skinned:.2} from native, the rigid card {err_rigid:.2}"
    );
}
