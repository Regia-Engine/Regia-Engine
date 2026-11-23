// SPDX-License-Identifier: MPL-2.0
//! Compute skinning, eight influences and dual quaternions (Tincal B2).
//!
//! - `kernel_matches_the_cpu_reference_*`: the compute kernel's skinned vertices equal
//!   `skin_vertex_reference` (which repeats the vertex shaders' linear blend) on a 67-joint
//!   tube (DO27's pilot skeleton size), with four and eight influences, linear and dual
//!   quaternion, scaled joints included.
//! - `dual_quaternions_keep_a_twisted_tube_round`: the candy-wrapper test. A tube twisted 160°
//!   between two joints collapses to a sixth of its radius at the blend with linear skinning;
//!   dual quaternions keep every ring at its radius.
//! - `compute_path_draws_like_the_vertex_path`: a frame with shadows and the depth prepass
//!   draws the animated 67-joint tube the same through the compute path as through the
//!   vertex shaders; options that change nothing leave the frame on the vertex path.
//! - `eight_influences_and_dual_quaternions_reach_the_frame`: options change the frame, are
//!   counted, and a table of the wrong length falls back to four influences.
//! - `compute_twins_write_motion_vectors`: the velocity pass reads last frame's skinned
//!   vertices: a moved joint writes its motion (as on the vertex path), a held pose writes none,
//!   including the frames whose dispatch is skipped because the pose settled.

use std::sync::Arc;

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{
    ALPHA_OPAQUE, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    JointMatrix, LabMaterial, LabVertex, MotionField, PostSettings, RenderDevice, ShadowCast,
    SkinInfluences8, SkinningMethod, SkinningOptions, skin_on_gpu, skin_vertex_reference,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

const TUBE_LENGTH: f32 = 1.6;
const TUBE_RADIUS: f32 = 0.12;
const SEGMENTS: usize = 12;

/// A capped-free tube along +Y from 0 to `TUBE_LENGTH`, `rings` rings of `SEGMENTS` vertices.
/// Each vertex is bound to the joints of a `joints`-long chain near its height: `influences`
/// of them (4 in the vertex, the rest in the returned eight-influence table), tent weights
/// summing to 1.
fn tube(rings: usize, joints: usize, influences: usize) -> (Vec<LabVertex>, Vec<u32>, Vec<SkinInfluences8>) {
    let seg = TUBE_LENGTH / (joints - 1) as f32;
    let mut vertices = Vec::new();
    let mut table = Vec::new();
    for r in 0..rings {
        let y = TUBE_LENGTH * r as f32 / (rings - 1) as f32;
        let u = y / seg;
        // The `influences` joints nearest to `u`, weighted by a tent of half-width influences / 2.
        let first =
            (u.floor() as i64 - (influences as i64 / 2 - 1)).clamp(0, (joints - influences) as i64) as usize;
        let mut slots: Vec<(u32, f32)> = (first..first + influences)
            .map(|j| {
                let w = (1.0 - (j as f32 - u).abs() / (influences as f32 * 0.5)).max(0.02);
                (j as u32, w)
            })
            .collect();
        let sum: f32 = slots.iter().map(|s| s.1).sum();
        for s in &mut slots {
            s.1 /= sum;
        }
        for s in 0..SEGMENTS {
            let a = std::f32::consts::TAU * s as f32 / SEGMENTS as f32;
            let (sin, cos) = a.sin_cos();
            let mut v = LabVertex::from_pos_normal_uv(
                [TUBE_RADIUS * cos, y, TUBE_RADIUS * sin],
                [cos, 0.0, sin],
                [s as f32 / SEGMENTS as f32, y],
            );
            v.tangent = [-sin, 0.0, cos, 1.0];
            v.color = [0.5, 0.25, 0.75, 1.0];
            let mut eight = SkinInfluences8::default();
            for (k, &(j, w)) in slots.iter().enumerate() {
                if k < 4 {
                    v.joints[k] = j;
                    v.weights[k] = w;
                }
                eight.joints[k] = j;
                eight.weights[k] = w;
            }
            vertices.push(v);
            table.push(eight);
        }
    }
    let mut indices = Vec::new();
    for r in 0..rings - 1 {
        for s in 0..SEGMENTS {
            let a = (r * SEGMENTS + s) as u32;
            let b = (r * SEGMENTS + (s + 1) % SEGMENTS) as u32;
            let (c, d) = (a + SEGMENTS as u32, b + SEGMENTS as u32);
            indices.extend([a, c, b, b, c, d]);
        }
    }
    (vertices, indices, table)
}

/// A bending, twisting pose of a `joints`-long chain along the tube at time `t` (skin matrices:
/// joint world × inverse bind). `scale` scales every joint about its own origin.
fn chain_pose(joints: usize, t: f32, scale: f32) -> Vec<JointMatrix> {
    let seg = TUBE_LENGTH / (joints - 1) as f32;
    let mut world = Mat4::IDENTITY;
    (0..joints)
        .map(|j| {
            if j > 0 {
                world *= Mat4::from_translation(Vec3::Y * seg);
            }
            let phase = t + j as f32 * 0.37;
            world *= Mat4::from_rotation_z(0.05 * phase.sin())
                * Mat4::from_rotation_x(0.03 * phase.cos())
                * Mat4::from_rotation_y(0.04);
            let inverse_bind = Mat4::from_translation(-Vec3::Y * seg * j as f32);
            JointMatrix::from_mat4(world * Mat4::from_scale(Vec3::splat(scale)) * inverse_bind)
        })
        .collect()
}

fn reference(
    vertices: &[LabVertex],
    table: Option<&[SkinInfluences8]>,
    joints: &[JointMatrix],
    method: SkinningMethod,
) -> Vec<([f32; 3], [f32; 3], [f32; 3])> {
    vertices
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let influences: Vec<(u32, f32)> = match table {
                Some(t) => (0..8).map(|k| (t[i].joints[k], t[i].weights[k])).collect(),
                None => (0..4).map(|k| (v.joints[k], v.weights[k])).collect(),
            };
            let tangent = [v.tangent[0], v.tangent[1], v.tangent[2]];
            skin_vertex_reference((v.position, v.normal, tangent), &influences, joints, method)
        })
        .collect()
}

fn max_error(gpu: &[LabVertex], cpu: &[([f32; 3], [f32; 3], [f32; 3])]) -> f32 {
    let diff = |a: [f32; 3], b: [f32; 3]| (0..3).map(|k| (a[k] - b[k]).abs()).fold(0.0, f32::max);
    gpu.iter()
        .zip(cpu)
        .map(|(g, c)| {
            let tangent = [g.tangent[0], g.tangent[1], g.tangent[2]];
            diff(g.position, c.0)
                .max(diff(g.normal, c.1))
                .max(diff(tangent, c.2))
        })
        .fold(0.0, f32::max)
}

fn check_kernel(influences: usize, method: SkinningMethod, scale: f32) {
    let device = device();
    let (vertices, _, table) = tube(40, 67, influences);
    let joints = chain_pose(67, 0.7, scale);
    let eight = (influences > 4).then(|| Arc::<[SkinInfluences8]>::from(table.clone()));
    let options = SkinningOptions {
        method,
        compute: true,
        influences8: eight.clone(),
    };
    let gpu = skin_on_gpu(&device, &vertices, &joints, &options).expect("compute skinning");
    let cpu = reference(&vertices, eight.as_deref(), &joints, method);
    let error = max_error(&gpu, &cpu);
    eprintln!("{influences} influences, {method:?}, scale {scale}: max error {error:e}");
    assert!(error < 1e-4, "kernel differs from the CPU reference by {error}");
    for (g, v) in gpu.iter().zip(&vertices) {
        assert_eq!(g.weights, [0.0; 4], "twin vertices carry no weights");
        assert_eq!(g.joints, [0; 4]);
        assert_eq!((g.uv, g.color, g.tangent[3]), (v.uv, v.color, v.tangent[3]));
    }
    // The pose moved the tube: the comparison is not of two rest poses.
    let moved = gpu
        .iter()
        .zip(&vertices)
        .map(|(g, v)| Vec3::from(g.position).distance(Vec3::from(v.position)))
        .fold(0.0, f32::max);
    assert!(moved > 0.05, "the pose moved the tube only {moved} m");
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn kernel_matches_the_cpu_reference_linear_four() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_kernel(4, SkinningMethod::Linear, 1.0);
}

#[test]
fn kernel_matches_the_cpu_reference_linear_eight() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_kernel(8, SkinningMethod::Linear, 1.0);
}

#[test]
fn kernel_matches_the_cpu_reference_dual_quaternion() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_kernel(4, SkinningMethod::DualQuaternion, 1.0);
    check_kernel(8, SkinningMethod::DualQuaternion, 1.0);
    // Scaled joints take the scale phase first.
    check_kernel(4, SkinningMethod::DualQuaternion, 0.8);
}

/// Eight influences are all read: the result differs from the vertex's own four (the first four
/// of a wide tent), and a table whose last four weights are zero equals the four.
#[test]
fn eight_influences_are_all_read() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (vertices, _, table) = tube(30, 67, 8);
    let joints = chain_pose(67, 1.3, 1.0);
    let eight = SkinningOptions {
        influences8: Some(Arc::from(table.clone())),
        ..SkinningOptions::default()
    };
    let with_eight = skin_on_gpu(&device, &vertices, &joints, &eight).expect("eight");
    let with_four = skin_on_gpu(&device, &vertices, &joints, &SkinningOptions::compute()).expect("four");
    let apart = with_eight
        .iter()
        .zip(&with_four)
        .map(|(a, b)| Vec3::from(a.position).distance(Vec3::from(b.position)))
        .fold(0.0, f32::max);
    assert!(apart > 1e-3, "eight influences drew like four ({apart})");
    let padded: Vec<SkinInfluences8> = vertices
        .iter()
        .map(|v| {
            let mut e = SkinInfluences8::default();
            e.joints[..4].copy_from_slice(&v.joints);
            e.weights[..4].copy_from_slice(&v.weights);
            e
        })
        .collect();
    let padded = SkinningOptions {
        influences8: Some(Arc::from(padded)),
        ..SkinningOptions::default()
    };
    let same = skin_on_gpu(&device, &vertices, &joints, &padded).expect("padded");
    let error = same
        .iter()
        .zip(&with_four)
        .map(|(a, b)| Vec3::from(a.position).distance(Vec3::from(b.position)))
        .fold(0.0, f32::max);
    assert!(
        error < 1e-5,
        "four influences in the table differ from the vertex's four ({error})"
    );
}

/// Review w11 #7: the kernel aligns dual quaternion signs to the heaviest influence, not to slot
/// 0. An eight-influence table whose slot 0 is unused (weight 0, joint 0 at rest) and whose real
/// influences turn 179° and 181° about Z blends to a 180° turn (it used to collapse to rest), and
/// matches the CPU reference.
#[test]
fn an_unused_slot_zero_is_not_the_dual_quaternion_pivot() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let turn = |deg: f32| JointMatrix::from_mat4(Mat4::from_quat(Quat::from_rotation_z(deg.to_radians())));
    let joints = vec![JointMatrix::identity(), turn(179.0), turn(181.0)];
    let vertices = vec![LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0]); 64];
    let mut entry = SkinInfluences8::default();
    entry.joints[1] = 1;
    entry.weights[1] = 0.5;
    entry.joints[2] = 2;
    entry.weights[2] = 0.5;
    let options = SkinningOptions {
        method: SkinningMethod::DualQuaternion,
        influences8: Some(Arc::from(vec![entry; vertices.len()])),
        ..SkinningOptions::default()
    };
    let skinned = skin_on_gpu(&device, &vertices, &joints, &options).expect("kernel");
    let cpu = skin_vertex_reference(
        ([1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        &entry
            .joints
            .iter()
            .copied()
            .zip(entry.weights)
            .collect::<Vec<_>>(),
        &joints,
        SkinningMethod::DualQuaternion,
    );
    for v in &skinned {
        let p = Vec3::from(v.position);
        assert!(
            (p - Vec3::new(-1.0, 0.0, 0.0)).length() < 1e-3,
            "turned half way round: {p}"
        );
        assert!(
            (p - Vec3::from(cpu.0)).length() < 1e-5,
            "kernel {p} vs CPU {:?}",
            cpu.0
        );
    }
}

/// Two joints at the tube's ends; the upper one twisted `angle` about the tube's axis, the
/// weights blending over the middle third.
fn twist(angle: f32) -> (Vec<LabVertex>, Vec<JointMatrix>) {
    let (mut vertices, _, _) = tube(31, 2, 2);
    for v in &mut vertices {
        let w = ((v.position[1] / TUBE_LENGTH - 1.0 / 3.0) * 3.0).clamp(0.0, 1.0);
        v.joints = [0, 1, 0, 0];
        v.weights = [1.0 - w, w, 0.0, 0.0];
    }
    let joints = vec![
        JointMatrix::identity(),
        JointMatrix::from_mat4(Mat4::from_quat(Quat::from_rotation_y(angle))),
    ];
    (vertices, joints)
}

/// The smallest distance of a skinned vertex from the tube's axis.
fn min_radius(vertices: &[LabVertex]) -> f32 {
    vertices
        .iter()
        .map(|v| v.position[0].hypot(v.position[2]))
        .fold(f32::MAX, f32::min)
}

#[test]
fn dual_quaternions_keep_a_twisted_tube_round() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (vertices, joints) = twist(160f32.to_radians());
    let linear = skin_on_gpu(&device, &vertices, &joints, &SkinningOptions::compute()).expect("linear");
    let dual = skin_on_gpu(&device, &vertices, &joints, &SkinningOptions::dual_quaternion()).expect("dual");
    let (r_linear, r_dual) = (min_radius(&linear), min_radius(&dual));
    eprintln!(
        "twist 160°: linear min radius {r_linear:.4} ({:.2} r), dual quaternion {r_dual:.4} ({:.2} r)",
        r_linear / TUBE_RADIUS,
        r_dual / TUBE_RADIUS
    );
    // cos(80°) = 0.17: linear blending pinches the middle ring (the candy wrapper).
    assert!(
        r_linear < 0.25 * TUBE_RADIUS,
        "linear skinning did not collapse ({r_linear})"
    );
    assert!(
        r_dual > 0.99 * TUBE_RADIUS,
        "dual quaternions lost volume ({r_dual})"
    );
    // The ends follow their joints with either method.
    let top = vertices.len() - 1;
    for skinned in [&linear, &dual] {
        let want = joints[1].transform_point(vertices[top].position);
        assert!(Vec3::from(skinned[top].position).distance(Vec3::from(want)) < 1e-5);
        assert!(Vec3::from(skinned[0].position).distance(Vec3::from(vertices[0].position)) < 1e-5);
    }
    // The CPU reference agrees.
    let cpu = reference(&vertices, None, &joints, SkinningMethod::DualQuaternion);
    assert!(max_error(&dual, &cpu) < 1e-4);
}

// ---- Frames ----

const W: u32 = 160;
const H: u32 = 120;

struct Tube {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    table: Arc<[SkinInfluences8]>,
    material: LabMaterial,
}

impl Tube {
    fn new(influences: usize) -> Self {
        let (vertices, indices, table) = tube(48, 67, influences);
        Self {
            vertices,
            indices,
            table: Arc::from(table),
            material: LabMaterial {
                base_color: [0.8, 0.5, 0.3, 1.0],
                roughness: 0.6,
                alpha_mode: ALPHA_OPAQUE,
                ..LabMaterial::default()
            },
        }
    }

    fn scene<'a>(
        &'a self,
        joints: &'a [JointMatrix],
        ground: &'a (Vec<LabVertex>, Vec<u32>),
    ) -> FrameScene<'a> {
        let mesh = FrameMesh {
            is_static: false,
            mesh_key: 0x7B0E,
            material_key: 1,
            object_key: 31,
            vertices: &self.vertices,
            indices: &self.indices,
            material: &self.material,
            model: Mat4::from_translation(Vec3::new(0.0, -0.8, 0.0)).to_cols_array_2d(),
            joints,
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        };
        let floor = FrameMesh {
            mesh_key: 0xF100,
            object_key: 32,
            vertices: &ground.0,
            indices: &ground.1,
            model: Mat4::from_translation(Vec3::new(0.0, -0.8, 0.0)).to_cols_array_2d(),
            joints: &[],
            ..mesh
        };
        FrameScene {
            width: W,
            height: H,
            eye: [0.0, 0.2, 3.2],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.8,
            near: 0.05,
            far: 50.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 14.5,
            ambient_lux: 3000.0,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.6, -1.0, -0.4], [1.0, 1.0, 1.0], 80_000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: vec![mesh, floor],
        }
    }
}

fn ground() -> (Vec<LabVertex>, Vec<u32>) {
    let s = 3.0;
    let vertices = [[-s, 0.0, -s], [s, 0.0, -s], [s, 0.0, s], [-s, 0.0, s]]
        .into_iter()
        .map(|p| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

/// Pixels whose channels differ by more than 3 levels, and the mean absolute difference.
fn image_diff(a: &[u8], b: &[u8]) -> (usize, f32) {
    let mut over = 0;
    let mut sum = 0u64;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = (0..3).map(|k| pa[k].abs_diff(pb[k])).max().unwrap_or(0);
        if d > 3 {
            over += 1;
        }
        sum += u64::from(d);
    }
    (over, sum as f32 / (a.len() / 4) as f32)
}

/// Pixels where the tube draws (differs from the frame without its pose moved far away).
fn covered(a: &[u8], background: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(background.as_chunks::<4>().0)
        .filter(|(p, q)| (0..3).any(|k| p[k].abs_diff(q[k]) > 8))
        .count()
}

/// Review w11 #6: the compute path's GPU memory is reported (`skinning_gpu_bytes`: at least the
/// source copy and two skinned outputs per instance) and freed under memory pressure: after
/// `trim_memory`, a twin not drawn on the frame before goes on the next frame.
#[test]
fn twin_memory_is_reported_and_trimmed() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tube = Tube::new(4);
    let floor = ground();
    let mut compute = FrameRenderer::new();
    compute.set_compute_skinning(true);
    assert_eq!(compute.skinning_gpu_bytes(), 0);
    let pose = chain_pose(67, 0.4, 1.0);
    let scene = tube.scene(&pose, &floor);
    compute.render_offscreen(&device, &scene).expect("frame");
    let vertex_bytes = (tube.vertices.len() * std::mem::size_of::<LabVertex>()) as u64;
    let held = compute.skinning_gpu_bytes();
    assert!(held >= 3 * vertex_bytes, "{held} B for a {vertex_bytes} B mesh");
    // Drawn every frame: a trim keeps it.
    compute.trim_memory();
    compute.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(compute.skinning_stats().resident, 1);
    assert_eq!(compute.skinning_gpu_bytes(), held);
    // A frame without the tube, then a trim: the next frame frees its twin and source copy.
    let empty = FrameScene {
        meshes: vec![scene.meshes[1]],
        ..tube.scene(&pose, &floor)
    };
    compute.render_offscreen(&device, &empty).expect("frame");
    assert_eq!(
        compute.skinning_gpu_bytes(),
        held,
        "idle twins wait out the window"
    );
    compute.trim_memory();
    compute.render_offscreen(&device, &empty).expect("frame");
    assert_eq!(compute.skinning_stats().resident, 0);
    // Only the one-element stand-in influence table may stay.
    assert!(
        compute.skinning_gpu_bytes() < 256,
        "{} B left",
        compute.skinning_gpu_bytes()
    );
    // Drawn again, it is rebuilt.
    compute.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(compute.skinning_gpu_bytes(), held);
}

#[test]
fn compute_path_draws_like_the_vertex_path() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tube = Tube::new(4);
    let floor = ground();
    let mut vertex_path = FrameRenderer::new();
    let mut compute = FrameRenderer::new();
    compute.set_compute_skinning(true);
    // Options that change nothing keep a renderer on the vertex path.
    let mut neutral = FrameRenderer::new();
    neutral.set_skinning_options([(31, SkinningOptions::default())]);
    let away = vec![JointMatrix::translation(0.0, 40.0, 0.0); 67];
    let background = FrameRenderer::new()
        .render_offscreen(&device, &tube.scene(&away, &floor))
        .expect("background")
        .pixels;
    let mut pixels = 0;
    for frame in 0..4 {
        let pose = chain_pose(67, frame as f32 * 0.9, 1.0);
        let scene = tube.scene(&pose, &floor);
        let a = vertex_path
            .render_offscreen(&device, &scene)
            .expect("vertex path");
        let b = compute.render_offscreen(&device, &scene).expect("compute path");
        let c = neutral
            .render_offscreen(&device, &scene)
            .expect("neutral options");
        let (over, mean) = image_diff(&a.pixels, &b.pixels);
        pixels = covered(&a.pixels, &background);
        let stats = compute.skinning_stats();
        eprintln!(
            "frame {frame}: tube covers {pixels} px; {over} px differ by > 3, mean {mean:.4}; {stats:?}"
        );
        // The tube's silhouette: 0.24 m × 1.6 m at 3.2 m, 120 px over fov 0.8 (about 44 px/m),
        // about 750 px (868 measured on lavapipe with its shadow). 600 is 80 % of it.
        assert!(pixels > 600, "the tube is not in view ({pixels} px)");
        // Edge pixels may flip on a last-bit difference of a position; nothing more.
        assert!(
            over <= (W * H) as usize / 400 && mean < 0.1,
            "frame {frame}: the compute path drew {over} px differently (mean {mean})"
        );
        assert_eq!(
            (stats.instances, stats.dispatched),
            (1, 1),
            "frame {frame}: {stats:?}"
        );
        assert_eq!(stats.unsupported, None);
        assert_eq!(stats.vertices, tube.vertices.len() as u64);
        assert_eq!(
            c.pixels, a.pixels,
            "frame {frame}: neutral options changed the frame"
        );
        assert_eq!(neutral.skinning_stats().instances, 0);
        assert_eq!(vertex_path.skinning_stats().instances, 0);
    }
    assert!(pixels > 0);
    // A held pose: the second frame of it is dispatched again (both buffers), the third not.
    let pose = chain_pose(67, 9.0, 1.0);
    let scene = tube.scene(&pose, &floor);
    let mut dispatched = Vec::new();
    for _ in 0..4 {
        let b = compute.render_offscreen(&device, &scene).expect("held");
        let a = vertex_path
            .render_offscreen(&device, &scene)
            .expect("held vertex path");
        assert!(image_diff(&a.pixels, &b.pixels).0 <= (W * H) as usize / 400);
        dispatched.push(compute.skinning_stats().dispatched);
    }
    assert_eq!(
        dispatched,
        [1, 1, 0, 0],
        "a held pose is skinned twice, then reused"
    );
}

#[test]
fn eight_influences_and_dual_quaternions_reach_the_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tube = Tube::new(8);
    let floor = ground();
    let pose = chain_pose(67, 2.1, 1.0);
    let scene = tube.scene(&pose, &floor);
    let render = |options: Option<SkinningOptions>| {
        let mut renderer = FrameRenderer::new();
        if let Some(options) = options {
            renderer.set_skinning_options([(31, options)]);
        } else {
            renderer.set_compute_skinning(true);
        }
        let image = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        (image, renderer.skinning_stats())
    };
    let (four, four_stats) = render(None);
    assert_eq!(four_stats.eight_influences, 0);
    let (eight, eight_stats) = render(Some(SkinningOptions {
        influences8: Some(tube.table.clone()),
        ..SkinningOptions::default()
    }));
    assert_eq!(
        (eight_stats.instances, eight_stats.eight_influences),
        (1, 1),
        "{eight_stats:?}"
    );
    assert!(
        image_diff(&four, &eight).0 > 20,
        "eight influences drew like four"
    );
    // A table of another length is ignored: four influences, counted.
    let short: Arc<[SkinInfluences8]> = Arc::from(&tube.table[..10]);
    let (fallback, fallback_stats) = render(Some(SkinningOptions {
        influences8: Some(short),
        ..SkinningOptions::default()
    }));
    assert_eq!(
        (fallback_stats.eight_influences, fallback_stats.influence_mismatch),
        (0, 1),
        "{fallback_stats:?}"
    );
    assert!(image_diff(&four, &fallback).0 <= (W * H) as usize / 400);
    let (dual, dual_stats) = render(Some(SkinningOptions::dual_quaternion()));
    assert_eq!(dual_stats.dual_quaternion, 1, "{dual_stats:?}");
    // A gentle bend: dual quaternions stay close to linear skinning, but not equal.
    let (over, _) = image_diff(&four, &dual);
    eprintln!("dual quaternion vs linear on a gentle bend: {over} px differ");
    assert!(over < (W * H) as usize / 10);
}

// ---- Motion vectors ----

const MW: u32 = 128;
const MH: u32 = 96;
const EYE_Z: f32 = 3.0;
const FOV_Y: f32 = 0.9;

fn view_width_m() -> f32 {
    2.0 * EYE_Z * (FOV_Y * 0.5).tan() * MW as f32 / MH as f32
}

/// Two quads: the left on joint 0, the right on joint 1.
fn limbs() -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (x0, x1, joint) in [(-1.4f32, -0.4f32, 0u32), (0.4, 1.4, 1)] {
        let base = vertices.len() as u32;
        for p in [[x0, -0.3, 0.0], [x1, -0.3, 0.0], [x1, 0.3, 0.0], [x0, 0.3, 0.0]] {
            let mut v = LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]);
            v.joints = [joint, 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
            vertices.push(v);
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

fn motion_scene<'a>(
    geometry: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
    post: &'a PostSettings,
    joints: &'a [JointMatrix],
) -> FrameScene<'a> {
    let mut quality = FrameQuality::LAB;
    quality.taau = true;
    quality.depth_velocity = false;
    FrameScene {
        width: MW,
        height: MH,
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
        post: Some(post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: &[],
        quality,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 0x51C1,
            material_key: 1,
            object_key: 21,
            vertices: &geometry.0,
            indices: &geometry.1,
            material,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints,
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

fn motion(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> MotionField {
    renderer.render_offscreen(device, scene).expect("frame");
    renderer
        .read_motion(device)
        .expect("read motion")
        .expect("velocity target")
}

/// Median X motion of the moved pixels of the left or right half, and how many moved.
fn moved_x(field: &MotionField, right: bool) -> (f32, usize) {
    let mid = field.width / 2;
    let (x0, x1) = if right { (mid, field.width) } else { (0, mid) };
    let mut xs: Vec<f32> = (0..field.height)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .map(|(x, y)| field.motion[(y * field.width + x) as usize])
        .filter(|m| m[0].hypot(m[1]) > 0.005)
        .map(|m| m[0])
        .collect();
    xs.sort_by(f32::total_cmp);
    (xs.get(xs.len() / 2).copied().unwrap_or(0.0), xs.len())
}

#[test]
fn compute_twins_write_motion_vectors() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let geometry = limbs();
    let material = LabMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        roughness: 1.0,
        alpha_mode: ALPHA_OPAQUE,
        ..LabMaterial::default()
    };
    let post = PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    };
    let mut compute = FrameRenderer::new();
    compute.set_compute_skinning(true);
    let mut vertex_path = FrameRenderer::new();
    let width = view_width_m();
    // Joint 1's X per frame and the motion expected (None: none).
    let steps: [(f32, Option<f32>); 7] = [
        (0.0, None),        // first frame: no previous pose
        (0.3, Some(0.3)),   // moved
        (0.3, None),        // held (dispatched again)
        (0.3, None),        // held (dispatch skipped)
        (0.3, None),        // held (dispatch skipped)
        (0.45, Some(0.15)), // moved after a skipped frame
        (0.45, None),
    ];
    for (i, &(dx, expected)) in steps.iter().enumerate() {
        let joints = [JointMatrix::identity(), JointMatrix::translation(dx, 0.0, 0.0)];
        let scene = motion_scene(&geometry, &material, &post, &joints);
        let field = motion(&mut compute, &device, &scene);
        let reference = motion(&mut vertex_path, &device, &scene);
        let (got, moved) = moved_x(&field, true);
        let (want_ref, _) = moved_x(&reference, true);
        let (left, _) = moved_x(&field, false);
        eprintln!(
            "step {i}: dx {dx}: compute {got:.5} ({moved} px), vertex path {want_ref:.5}; {:?}",
            compute.skinning_stats()
        );
        assert_eq!(compute.skinning_stats().instances, 1);
        assert_eq!(left, 0.0, "step {i}: the unmoved joint 0 wrote motion");
        match expected {
            Some(step) => {
                let want = -step / width;
                assert!(moved > 300, "step {i}: only {moved} px wrote motion");
                assert!(
                    (got - want).abs() <= want.abs() * 0.05,
                    "step {i}: wrote {got:.5}, want {want:.5}"
                );
                assert!(
                    (got - want_ref).abs() <= want.abs() * 0.02,
                    "step {i}: unlike the vertex path"
                );
            }
            None => assert_eq!(moved, 0, "step {i}: a held pose wrote motion ({got})"),
        }
    }
}
