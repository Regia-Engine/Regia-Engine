// SPDX-License-Identifier: MPL-2.0
//! Double-sided materials in the velocity pass and in material graphs.
//!
//! A moving double-sided quad seen from behind must write object motion. A material graph
//! honours `LabMaterial::double_sided` the same way a standard material does: cull-none, and
//! the back-face normal is already flipped by `graph_surface_input`.

use std::sync::Arc;

use regia_render::{
    ALPHA_OPAQUE, DirectionalLightUniform, FogSettings, FrameMesh, FramePathKind, FrameQuality,
    FrameRenderer, FrameScene, IndirectStrategy, LabMaterial, LabMesh, LabScene, LabVertex, MotionField,
    PostSettings, RenderDevice, ShadowCast, SurfaceGraph, frame_path_counters, take_frame_path_events,
    trace_frame_path,
};

const GRAPH: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.8, 0.8, 0.8),
        1.0,
        0.0,
        1.0,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translate(x: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, 0.0, 0.0, 1.0],
    ]
}

/// Quad in the XY plane, front face +Z (CCW from +Z). Object space, centred.
fn quad(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let normal = [0.0, 0.0, 1.0];
    let corners = [
        ([-half, -half, 0.0], [0.0, 1.0]),
        ([half, -half, 0.0], [1.0, 1.0]),
        ([half, half, 0.0], [1.0, 0.0]),
        ([-half, half, 0.0], [0.0, 0.0]),
    ];
    let vertices = corners
        .into_iter()
        .map(|(position, uv)| LabVertex::from_pos_normal_uv(position, normal, uv))
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

/// Sheet at `z`, `face_positive_z` is the glTF front face.
fn sheet(z: f32, half: f32, face_positive_z: bool) -> LabMesh {
    let normal = if face_positive_z {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 0.0, -1.0]
    };
    let corners = [
        ([-half, -half, z], [0.0, 1.0]),
        ([half, -half, z], [1.0, 1.0]),
        ([half, half, z], [1.0, 0.0]),
        ([-half, half, z], [0.0, 0.0]),
    ];
    let vertices = corners
        .into_iter()
        .map(|(position, uv)| LabVertex::from_pos_normal_uv(position, normal, uv))
        .collect();
    let indices = if face_positive_z {
        vec![0, 1, 2, 0, 2, 3]
    } else {
        vec![0, 2, 1, 0, 3, 2]
    };
    LabMesh {
        vertices,
        indices,
        ..LabMesh::unskinned()
    }
}

fn max_motion_half(field: &MotionField, right: bool) -> f32 {
    let mid = field.width / 2;
    let (x0, x1) = if right { (mid, field.width) } else { (0, mid) };
    let mut best = 0.0f32;
    for y in 0..field.height {
        for x in x0..x1 {
            let m = field.motion[(y * field.width + x) as usize];
            best = best.max(m[0].hypot(m[1]));
        }
    }
    best
}

fn max_motion(field: &MotionField) -> f32 {
    field
        .motion
        .iter()
        .map(|m| m[0].hypot(m[1]))
        .fold(0.0f32, f32::max)
}

struct VelocityRig {
    verts: Vec<LabVertex>,
    indices: Vec<u32>,
    materials: [LabMaterial; 2],
    post: PostSettings,
    quality: FrameQuality,
}

impl VelocityRig {
    fn new() -> Self {
        let (verts, indices) = quad(0.35);
        let plain = LabMaterial {
            base_color: [0.8, 0.8, 0.8, 1.0],
            roughness: 1.0,
            alpha_mode: ALPHA_OPAQUE,
            ..LabMaterial::default()
        };
        let mut double = plain.clone();
        double.double_sided = true;
        let mut quality = FrameQuality::LAB;
        quality.taau = true;
        quality.depth_velocity = false;
        Self {
            verts,
            indices,
            // Same mesh, two materials: the batch key must keep their cull modes apart.
            materials: [double, plain],
            post: PostSettings {
                exposure_ev: 14.5,
                ..PostSettings::golden_deterministic()
            },
            quality,
        }
    }

    fn scene(&self, eye_z: f32, left_x: f32, right_x: f32) -> FrameScene<'_> {
        let meshes = vec![
            self.mesh(0xBEEF, 1, 11, 0, translate(left_x)),
            self.mesh(0xBEEF, 2, 12, 1, translate(right_x)),
        ];
        FrameScene {
            width: 128,
            height: 64,
            eye: [0.0, 0.0, eye_z],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
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
            quality: self.quality,
            meshes,
        }
    }

    fn mesh(
        &self,
        mesh_key: u64,
        material_key: u64,
        object_key: u64,
        material: usize,
        model: [[f32; 4]; 4],
    ) -> FrameMesh<'_> {
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key,
            object_key,
            vertices: &self.verts,
            indices: &self.indices,
            material: &self.materials[material],
            model,
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }
}

fn render_motion(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> MotionField {
    renderer.render_offscreen(device, scene).expect("frame");
    renderer
        .read_motion(device)
        .expect("read motion")
        .expect("velocity target")
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn moving_double_sided_quad_seen_from_behind_writes_object_velocity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let rig = VelocityRig::new();
    let mut behind = FrameRenderer::new();
    // Frame 0 establishes the previous model. Motion is zero.
    let rest = rig.scene(-3.0, -1.1, 1.1);
    let first = render_motion(&mut behind, &device, &rest);
    assert!(
        max_motion(&first) < 0.002,
        "the first frame has no previous pose, motion {}",
        max_motion(&first)
    );
    // Both quads move +0.35 m on X. The camera stays put, on the back-face side.
    let moved = rig.scene(-3.0, -0.75, 1.45);
    let field = render_motion(&mut behind, &device, &moved);
    // Looking toward +Z, the view's right is world −X, so the double-sided quad
    // (world x < 0) is the right half and the single-sided quad is the left.
    let double = max_motion_half(&field, true);
    let single = max_motion_half(&field, false);
    eprintln!("behind: double-sided {double:.4}, single-sided {single:.4}");
    assert!(
        double > 0.02,
        "a moving double-sided quad seen from behind must write object velocity, got {double}"
    );
    assert!(
        single < 0.002,
        "a single-sided quad seen from behind must stay culled, got {single}"
    );

    // The same single-sided material, from the front, still writes motion of the same size.
    let mut front = FrameRenderer::new();
    let _ = render_motion(&mut front, &device, &rig.scene(3.0, -1.1, 1.1));
    let front_field = render_motion(&mut front, &device, &rig.scene(3.0, -0.75, 1.45));
    // Looking toward −Z, the view's right is world +X: the single-sided quad is the right half.
    let front_single = max_motion_half(&front_field, true);
    let front_double = max_motion_half(&front_field, false);
    eprintln!("front: double-sided {front_double:.4}, single-sided {front_single:.4}");
    assert!(
        front_single > 0.02,
        "a single-sided quad seen from the front must still write velocity, got {front_single}"
    );
    let ratio = front_single / double;
    assert!(
        (0.5..2.0).contains(&ratio),
        "front-face motion {front_single:.4} should match the back-face double-sided motion {double:.4}"
    );
    assert!(
        front_double > 0.02,
        "the double-sided quad's front face must still write velocity, got {front_double}"
    );
}

fn backdrop() -> LabMaterial {
    LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        unlit: true,
        roughness: 1.0,
        ..LabMaterial::default()
    }
}

fn graph_material(double_sided: bool) -> LabMaterial {
    LabMaterial {
        roughness: 1.0,
        metallic: 0.0,
        double_sided,
        unlit: false,
        surface_graph: Some(Arc::new(SurfaceGraph::new(GRAPH))),
        ..LabMaterial::default()
    }
}

fn graph_scene(eye_z: f32, double_sided: bool, with_sheet: bool) -> LabScene {
    let from_positive = eye_z > 0.0;
    let mut meshes = vec![sheet(if from_positive { -1.5 } else { 1.5 }, 8.0, from_positive)];
    if with_sheet {
        let mut pane = sheet(0.0, 2.0, true);
        pane.material = 1;
        meshes.push(pane);
    }
    // Travel +Z lights a flipped back face (normal −Z) for a camera on −Z.
    // Travel −Z lights the front face (normal +Z) for a camera on +Z.
    let travel = if from_positive {
        [0.0, 0.0, -1.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let post = PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    };
    LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 0.0, eye_z],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.6,
        near: 0.05,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 200.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new(travel, [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![backdrop(), graph_material(double_sided)],
        meshes,
    }
}

fn render_graph(scene: &LabScene) -> Vec<u8> {
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(&device, &scene.as_frame_scene())
        .expect("frame")
        .pixels
}

fn rect_luma(px: &[u8], w: u32, h: u32) -> f64 {
    let (x0, x1) = ((0.35 * w as f32) as u32, (0.65 * w as f32) as u32);
    let (y0, y1) = ((0.35 * h as f32) as u32, (0.65 * h as f32) as u32);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * w + x) * 4) as usize;
            sum += 0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn max_abs_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(p, q)| p.abs_diff(*q)).max().unwrap_or(0)
}

#[test]
fn double_sided_graph_material_renders_its_back_face() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    const W: u32 = 96;
    const H: u32 = 64;
    let bare = render_graph(&graph_scene(-2.5, true, false));
    let single = render_graph(&graph_scene(-2.5, false, true));
    let double = render_graph(&graph_scene(-2.5, true, true));
    let diff = max_abs_diff(&bare, &single);
    let bare_l = rect_luma(&bare, W, H);
    let double_l = rect_luma(&double, W, H);
    eprintln!("behind: backdrop {bare_l:.2}, double-sided graph {double_l:.2}, single max diff {diff}");
    assert_eq!(diff, 0, "a single-sided graph must not draw its back face");
    assert!(
        double_l > 40.0,
        "a double-sided graph seen from behind must be lit, luma {double_l:.2}"
    );
    assert!(
        double_l > bare_l * 4.0 + 20.0,
        "the back face must be much brighter than the backdrop ({double_l:.2} vs {bare_l:.2})"
    );

    let front_single = render_graph(&graph_scene(2.5, false, true));
    let front_double = render_graph(&graph_scene(2.5, true, true));
    let front_diff = max_abs_diff(&front_single, &front_double);
    let front_l = rect_luma(&front_single, W, H);
    eprintln!("front: single {front_l:.2}, max diff {front_diff}");
    assert!(
        front_l > 40.0,
        "the front face of the graph must be lit, luma {front_l:.2}"
    );
    assert!(
        front_diff <= 2,
        "cull-none must not change the front face of the same graph (max diff {front_diff})"
    );
}

#[test]
fn prewarmed_velocity_cull_variants_create_no_pipeline_on_later_frames() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut scene = graph_scene(-2.5, true, true);
    // The velocity pass has to run, and the graph target must be the one prewarm builds
    // (instanced only when this device draws GPU-driven).
    let mut quality = FrameQuality::LAB;
    quality.taau = true;
    quality.depth_velocity = false;
    quality.gpu_driven = IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
    let mut renderer = FrameRenderer::new();
    {
        let frame = scene.as_frame_scene_with_quality(quality);
        renderer.prewarm(&device, &frame.meshes, 8).expect("prewarm");
    }
    trace_frame_path(true);
    let _ = take_frame_path_events();
    {
        let frame = scene.as_frame_scene_with_quality(quality);
        renderer
            .render_offscreen(&device, &frame)
            .expect("settling frame");
    }
    let events = take_frame_path_events();
    let late: Vec<_> = events
        .iter()
        .filter(|event| {
            event.kind == FramePathKind::Pipeline
                && (event.label.contains("velocity") || event.label.contains("graph"))
        })
        .map(|event| event.label.clone())
        .collect();
    assert!(
        late.is_empty(),
        "a frame after prewarm compiled a velocity or graph pipeline: {late:?}"
    );

    // A moved double-sided draw, and the graph still in frame, must not compile either.
    scene.meshes[1].model = translate(0.4);
    for _ in 0..3 {
        let frame = scene.as_frame_scene_with_quality(quality);
        renderer.render_offscreen(&device, &frame).expect("warm frame");
    }
    let pipelines = frame_path_counters().pipelines;
    for _ in 0..2 {
        let frame = scene.as_frame_scene_with_quality(quality);
        renderer.render_offscreen(&device, &frame).expect("steady frame");
    }
    assert_eq!(
        frame_path_counters().pipelines,
        pipelines,
        "a frame after warm-up created a pipeline"
    );
    trace_frame_path(false);
}
