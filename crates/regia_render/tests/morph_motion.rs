// SPDX-License-Identifier: MPL-2.0
//! Morph targets write motion vectors (`specs/13` § 8, Tincal B2 remainder; ADR-2652).
//!
//! Before this, the morph pre-pass deformed only the current frame's copy, so a morphing face
//! wrote zero motion and TAA/TSR reprojected its history from where it no longer was. Now a rigid
//! instance keeps two whole-mesh copies of its deformation, written only on frames whose weights
//! change, and the velocity pass draws both poses on those frames.
//!
//! - a morphing quad's velocity target equals the CPU delta (barycentric over the current pose,
//!   projected) at every covered pixel, with and without depth velocity;
//! - steady weights write zero motion;
//! - a weight change, and frames whose weights keep changing, create no buffer, bind group,
//!   pipeline or texture (the two copies come with the instance's first draw);
//! - steady weights dispatch nothing and write no motion; a change after a pause moves from the
//!   steady pose.

use std::sync::Arc;

use regia_render::{
    ALPHA_OPAQUE, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabVertex, MorphInstance, MorphMode, MorphTargets, MotionField, PostSettings, RenderDevice,
    ShadowCast, frame_path_counters,
};

const W: u32 = 128;
const H: u32 = 96;
const EYE_Z: f32 = 3.0;
const FOV_Y: f32 = 0.9;
const OBJECT: u64 = 41;
const MESH: u64 = 0x0B2_0001;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The quad's corners (z = 0, facing +Z) and the target's per-corner deltas: the right edge
/// stretches and shears, the left edge slides, so the motion differs across the quad.
const CORNERS: [[f32; 2]; 4] = [[-0.8, -0.5], [0.8, -0.5], [0.8, 0.5], [-0.8, 0.5]];
const DELTAS: [[f32; 2]; 4] = [[-0.1, 0.0], [0.3, 0.06], [0.3, -0.06], [-0.1, 0.0]];
const TRIANGLES: [[usize; 3]; 2] = [[0, 1, 2], [0, 2, 3]];

fn quad() -> (Vec<LabVertex>, Vec<u32>) {
    let vertices = CORNERS
        .iter()
        .map(|&[x, y]| LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [x * 0.5 + 0.5, 0.5 - y]))
        .collect();
    (vertices, TRIANGLES.iter().flatten().map(|&i| i as u32).collect())
}

fn stretch() -> regia_asset::CpuMorphTarget {
    regia_asset::CpuMorphTarget {
        name: "stretch".into(),
        vertex_indices: (0..4).collect(),
        position_deltas: DELTAS.iter().map(|&[dx, dy]| [dx, dy, 0.0]).collect(),
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
    }
}

/// The view at z = 0, metres.
fn view_size_m() -> (f32, f32) {
    let h = 2.0 * EYE_Z * (FOV_Y * 0.5).tan();
    (h * W as f32 / H as f32, h)
}

/// UV of a point of the z = 0 plane (the plane is parallel to the image: the map is affine).
fn uv_of(p: [f32; 2]) -> [f32; 2] {
    let (vw, vh) = view_size_m();
    [0.5 + p[0] / vw, 0.5 - p[1] / vh]
}

fn pose(weight: f32) -> [[f32; 2]; 4] {
    std::array::from_fn(|i| {
        [
            CORNERS[i][0] + weight * DELTAS[i][0],
            CORNERS[i][1] + weight * DELTAS[i][1],
        ]
    })
}

/// Barycentrics of `p` in triangle `t` of `corners`, when inside with `margin` (UV units) to
/// every edge.
fn inside(corners: &[[f32; 2]; 4], t: [usize; 3], p: [f32; 2], margin: f32) -> Option<[f32; 3]> {
    let [a, b, c] = t.map(|i| uv_of(corners[i]));
    let area = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
    let edge = |u: [f32; 2], v: [f32; 2]| {
        let len = ((v[0] - u[0]).powi(2) + (v[1] - u[1]).powi(2)).sqrt();
        ((v[0] - u[0]) * (p[1] - u[1]) - (p[0] - u[0]) * (v[1] - u[1])) / area.signum() / len.max(1e-9)
    };
    let (ea, eb, ec) = (edge(b, c), edge(c, a), edge(a, b));
    if ea < margin || eb < margin || ec < margin {
        return None;
    }
    let w0 = ((b[0] - p[0]) * (c[1] - p[1]) - (c[0] - p[0]) * (b[1] - p[1])) / area;
    let w1 = ((c[0] - p[0]) * (a[1] - p[1]) - (a[0] - p[0]) * (c[1] - p[1])) / area;
    Some([w0, w1, 1.0 - w0 - w1])
}

/// The CPU motion (`prev_uv - uv`) at every pixel the current pose covers with a two-pixel
/// margin: `(pixel index, motion)`.
fn expected(prev_w: f32, cur_w: f32) -> Vec<(usize, [f32; 2])> {
    let (cur, prev) = (pose(cur_w), pose(prev_w));
    let margin = 2.0 / H as f32;
    let mut out = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let p = [(x as f32 + 0.5) / W as f32, (y as f32 + 0.5) / H as f32];
            for t in TRIANGLES {
                if let Some(b) = inside(&cur, t, p, margin) {
                    let mut prev_uv = [0.0f32; 2];
                    for (k, &i) in t.iter().enumerate() {
                        let uv = uv_of(prev[i]);
                        prev_uv[0] += b[k] * uv[0];
                        prev_uv[1] += b[k] * uv[1];
                    }
                    out.push(((y * W + x) as usize, [prev_uv[0] - p[0], prev_uv[1] - p[1]]));
                    break;
                }
            }
        }
    }
    out
}

struct Rig {
    quad: (Vec<LabVertex>, Vec<u32>),
    targets: Arc<MorphTargets>,
    material: LabMaterial,
    post: PostSettings,
}

impl Rig {
    fn new() -> Self {
        let quad = quad();
        let targets = Arc::new(MorphTargets::new(&quad.0, &[stretch()]).expect("targets"));
        Self {
            quad,
            targets,
            material: LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
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

    fn scene(&self, depth_velocity: bool) -> FrameScene<'_> {
        let mut quality = FrameQuality::LAB;
        quality.taau = true;
        quality.depth_velocity = depth_velocity;
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
            double_sided: true,
            post: Some(&self.post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
            local_lights: &[],
            quality,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: MESH,
                material_key: 1,
                object_key: OBJECT,
                vertices: &self.quad.0,
                indices: &self.quad.1,
                material: &self.material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        }
    }

    fn frame(
        &self,
        renderer: &mut FrameRenderer,
        device: &RenderDevice,
        weight: f32,
        depth_velocity: bool,
    ) -> MotionField {
        renderer.set_morph_instances(&[MorphInstance {
            object_key: OBJECT,
            targets: Arc::clone(&self.targets),
            weights: vec![weight],
            mode: MorphMode::Game,
        }]);
        renderer
            .render_offscreen(device, &self.scene(depth_velocity))
            .expect("frame");
        renderer
            .read_motion(device)
            .expect("read motion")
            .expect("velocity target")
    }
}

fn max_len(field: &MotionField) -> f32 {
    field.motion.iter().map(|m| m[0].hypot(m[1])).fold(0.0, f32::max)
}

/// Worst difference between the field and the CPU motion over the covered pixels, and how many.
fn worst(field: &MotionField, expected: &[(usize, [f32; 2])]) -> (f32, usize) {
    let worst = expected
        .iter()
        .map(|(i, e)| {
            let m = field.motion[*i];
            (m[0] - e[0]).hypot(m[1] - e[1])
        })
        .fold(0.0, f32::max);
    (worst, expected.len())
}

fn a_morphing_quad_writes_the_cpu_motion(depth_velocity: bool) {
    let device = device();
    let rig = Rig::new();
    let mut renderer = FrameRenderer::new();
    // First deformation: no earlier pose, no motion.
    let first = rig.frame(&mut renderer, &device, 0.2, depth_velocity);
    assert!(max_len(&first) < 1e-5, "first frame {}", max_len(&first));
    // The weight moves 0.2 -> 0.8: every covered pixel moves by the CPU delta.
    let moved = rig.frame(&mut renderer, &device, 0.8, depth_velocity);
    let want = expected(0.2, 0.8);
    let (err, n) = worst(&moved, &want);
    let largest = want.iter().map(|(_, m)| m[0].hypot(m[1])).fold(0.0, f32::max);
    println!(
        "depth_velocity {depth_velocity}: {n} covered pixels, largest CPU motion {largest:.4} uv, worst error {err:.6} uv"
    );
    assert!(n > 1000, "the quad covers the view: {n}");
    assert!(largest > 0.02, "the deltas move it by pixels: {largest}");
    // Half a pixel at this size (the TAA jitter moves coverage by less than that).
    assert!(
        err < 0.5 / W as f32,
        "worst motion error {err} uv over {n} pixels"
    );
    assert_eq!(renderer.morph_stats().motion_instances, 1);
    // Steady weights: zero motion.
    let steady = rig.frame(&mut renderer, &device, 0.8, depth_velocity);
    assert!(max_len(&steady) < 1e-5, "steady weights {}", max_len(&steady));
    assert_eq!(renderer.morph_stats().motion_instances, 0);
}

#[test]
fn a_morphing_quad_writes_the_cpu_motion_at_every_covered_pixel() {
    a_morphing_quad_writes_the_cpu_motion(false);
}

#[test]
fn a_morphing_quad_writes_its_motion_under_depth_velocity() {
    a_morphing_quad_writes_the_cpu_motion(true);
}

#[test]
fn changing_weights_create_nothing_and_steady_weights_cost_nothing() {
    let device = device();
    let rig = Rig::new();
    let mut renderer = FrameRenderer::new();
    // The first draw creates the instance's buffers, its two motion copies included.
    rig.frame(&mut renderer, &device, 0.1, false);
    let created = renderer.morph_stats().buffers_created;
    let resident = renderer.morph_stats().gpu_bytes;
    // The first change and the frames after it (the temporal targets warm up meanwhile).
    for w in [0.2, 0.3, 0.4, 0.5] {
        rig.frame(&mut renderer, &device, w, false);
    }
    assert_eq!(
        renderer.morph_stats().buffers_created,
        created,
        "a weight change creates no buffer"
    );
    // Weights changing every frame: motion every frame, nothing created.
    let before = frame_path_counters();
    for w in [0.6, 0.7, 0.8, 0.9] {
        let field = rig.frame(&mut renderer, &device, w, false);
        assert!(max_len(&field) > 1e-3, "weight {w} moves the quad");
        assert_eq!(renderer.morph_stats().motion_instances, 1);
    }
    let after = frame_path_counters().since(before);
    assert_eq!(
        (after.pipelines, after.bind_groups, after.buffers, after.textures),
        (0, 0, 0, 0),
        "{after:?}"
    );
    // Steady weights: no dispatch, no motion, nothing more resident.
    for _ in 0..4 {
        let field = rig.frame(&mut renderer, &device, 0.9, false);
        assert!(max_len(&field) < 1e-5);
        let stats = renderer.morph_stats();
        assert_eq!((stats.dispatched, stats.motion_instances), (0, 0));
    }
    assert_eq!(renderer.morph_stats().gpu_bytes, resident);
    // A change after a pause moves from the steady pose.
    let moved = rig.frame(&mut renderer, &device, 0.3, false);
    let want = expected(0.9, 0.3);
    let (err, n) = worst(&moved, &want);
    assert!(
        err < 0.5 / W as f32,
        "after a pause: worst error {err} over {n} pixels"
    );
}
