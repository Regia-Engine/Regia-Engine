// SPDX-License-Identifier: MPL-2.0
//! GPU morph targets (Tincal engine item B1, `specs/13` § 8).
//!
//! A subdivided panel with two sparse targets is drawn three times from one mesh: skinned and
//! morphed, rigid and morphed (other weights), and plain. The compute pre-pass must match the CPU
//! reference ([`MorphTargets::apply`]) to 1e-4 m per vertex, the frame must match the same scene
//! drawn from CPU-deformed meshes (per-object and GPU-driven instanced paths), weight changes
//! must create no buffer, bind group or pipeline, steady frames must dispatch nothing, and
//! clearing the instances must restore the plain frame exactly.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, JointMatrix, LabMaterial, LabMesh, LabScene,
    LabVertex, MorphInstance, MorphMode, MorphTargets, PostSettings, RenderDevice, frame_path_counters,
};

const W: u32 = 192;
const H: u32 = 96;
const PANEL_MESH: u64 = 0x0B1D_0001;
const PANEL_X: [f32; 3] = [-2.2, 0.0, 2.2];
/// Grid vertices per side.
const N: usize = 9;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// An `N`×`N` grid panel, 1.6 m square in XY, facing +Z; skinned to joint 0 when `skinned`.
fn panel(skinned: bool) -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    for j in 0..N {
        for i in 0..N {
            let (u, v) = (i as f32 / (N - 1) as f32, j as f32 / (N - 1) as f32);
            let mut vertex = LabVertex::from_pos_normal_uv(
                [u * 1.6 - 0.8, v * 1.6 - 0.8, 0.0],
                [0.0, 0.0, 1.0],
                [u, 1.0 - v],
            );
            if skinned {
                vertex.weights = [1.0, 0.0, 0.0, 0.0];
            }
            vertices.push(vertex);
        }
    }
    let mut indices = Vec::new();
    for j in 0..N - 1 {
        for i in 0..N - 1 {
            let a = (j * N + i) as u32;
            let (b, c, d) = (a + 1, a + N as u32 + 1, a + N as u32);
            indices.extend([a, b, c, a, c, d]);
        }
    }
    (vertices, indices)
}

/// "bulge": the inner 5×5 vertices toward the camera with tilted normals; "lift": the top row
/// up. Both sparse.
fn targets() -> Vec<regia_asset::CpuMorphTarget> {
    let mut bulge = regia_asset::CpuMorphTarget {
        name: "bulge".into(),
        vertex_indices: Vec::new(),
        position_deltas: Vec::new(),
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
    };
    for j in 2..7 {
        for i in 2..7 {
            let (dx, dy) = (i as f32 - 4.0, j as f32 - 4.0);
            bulge.vertex_indices.push((j * N + i) as u32);
            bulge
                .position_deltas
                .push([0.0, 0.0, 0.5 - 0.05 * (dx * dx + dy * dy)]);
            bulge.normal_deltas.push([0.15 * dx, 0.15 * dy, 0.0]);
            bulge.tangent_deltas.push([0.0, 0.0, -0.1 * dx]);
        }
    }
    let top = (N - 1) * N;
    let lift = regia_asset::CpuMorphTarget {
        name: "lift".into(),
        vertex_indices: (top..top + N).map(|v| v as u32).collect(),
        position_deltas: (0..N).map(|i| [0.0, 0.3 + 0.02 * i as f32, 0.0]).collect(),
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
    };
    vec![bulge, lift]
}

struct Panels {
    rigid: (Vec<LabVertex>, Vec<u32>),
    skinned: (Vec<LabVertex>, Vec<u32>),
    rigid_set: Arc<MorphTargets>,
    skinned_set: Arc<MorphTargets>,
}

fn panels() -> Panels {
    let rigid = panel(false);
    let skinned = panel(true);
    let rigid_set = Arc::new(MorphTargets::new(&rigid.0, &targets()).expect("rigid set"));
    let skinned_set = Arc::new(MorphTargets::new(&skinned.0, &targets()).expect("skinned set"));
    Panels {
        rigid,
        skinned,
        rigid_set,
        skinned_set,
    }
}

const SKIN_WEIGHTS: [f32; 2] = [0.8, 0.5];
const RIGID_WEIGHTS: [f32; 2] = [-0.4, 1.0];

fn joint() -> JointMatrix {
    JointMatrix::translation(0.0, -0.25, 0.1)
}

/// Panel 0 skinned, panel 1 rigid, panel 2 plain. `deformed` swaps in CPU-deformed vertices
/// for panels 0 and 1 (the reference scene).
fn scene(p: &Panels, deformed: Option<([f32; 2], [f32; 2])>) -> LabScene {
    let mut skinned = p.skinned.0.clone();
    let mut rigid = p.rigid.0.clone();
    if let Some((skin_w, rigid_w)) = deformed {
        p.skinned_set.apply(&skin_w, &mut skinned);
        p.rigid_set.apply(&rigid_w, &mut rigid);
    }
    let mesh = |vertices: Vec<LabVertex>, indices: &[u32], x: f32, joints: Vec<JointMatrix>| LabMesh {
        vertices,
        indices: indices.to_vec(),
        model: translate(x, 0.0, 0.0),
        joints,
        ..LabMesh::unskinned()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.2, 5.0],
        target: [0.0, 0.2, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.4, -0.5, -1.0], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.6, 0.5, 1.0],
            roughness: 0.5,
            ..LabMaterial::default()
        }],
        meshes: vec![
            mesh(skinned, &p.skinned.1, PANEL_X[0], vec![joint()]),
            mesh(rigid, &p.rigid.1, PANEL_X[1], Vec::new()),
            mesh(p.rigid.0.clone(), &p.rigid.1, PANEL_X[2], Vec::new()),
        ],
    }
}

/// Pixels differing by more than 2/255, and the largest difference.
fn diff(a: &[u8], b: &[u8]) -> (usize, u8) {
    let mut count = 0;
    let mut max = 0;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(d);
        if d > 2 {
            count += 1;
        }
    }
    (count, max)
}

fn instances(p: &Panels, keys: &[u64], skin_w: [f32; 2], rigid_w: [f32; 2]) -> Vec<MorphInstance> {
    vec![
        MorphInstance {
            object_key: keys[0],
            targets: Arc::clone(&p.skinned_set),
            weights: skin_w.to_vec(),
            mode: MorphMode::Game,
        },
        MorphInstance {
            object_key: keys[1],
            targets: Arc::clone(&p.rigid_set),
            weights: rigid_w.to_vec(),
            mode: MorphMode::Creator,
        },
    ]
}

/// The morphed frame of `quality`, the CPU-deformed reference, and the plain frame.
fn check_paths(quality: FrameQuality) {
    let device = device();
    let p = panels();
    let lab = scene(&p, None);
    let mut frame = lab.as_frame_scene_with_quality(quality);
    // Panels 1 and 2 share the rigid mesh: the morphed one draws its own deformed copy.
    frame.meshes[0].mesh_key = PANEL_MESH;
    frame.meshes[1].mesh_key = PANEL_MESH + 1;
    frame.meshes[2].mesh_key = PANEL_MESH + 1;
    let keys: Vec<u64> = frame.meshes.iter().map(|m| m.object_key).collect();
    let mut renderer = FrameRenderer::new();
    let plain = renderer.render_offscreen(&device, &frame).expect("plain").pixels;

    renderer.set_morph_instances(&instances(&p, &keys, SKIN_WEIGHTS, RIGID_WEIGHTS));
    let morphed = renderer
        .render_offscreen(&device, &frame)
        .expect("morphed")
        .pixels;
    let stats = renderer.morph_stats();
    eprintln!("first morphed frame: {stats:?}");
    assert_eq!(stats.drawn, 2, "{stats:?}");
    assert_eq!(stats.dispatched, 2, "{stats:?}");
    assert_eq!(stats.mismatched, 0);

    // GPU span vs CPU reference, per vertex.
    for (key, set, base, weights) in [
        (keys[0], &p.skinned_set, &p.skinned.0, SKIN_WEIGHTS),
        (keys[1], &p.rigid_set, &p.rigid.0, RIGID_WEIGHTS),
    ] {
        let (first, span) = renderer.read_morphed_span(&device, key).expect("deformed span");
        assert_eq!(first, set.moved_span().start);
        let mut reference = base.clone();
        set.apply(
            &set.effective_weights(&weights, MorphMode::Creator),
            &mut reference,
        );
        let (mut pos, mut nrm, mut tan) = (0.0f32, 0.0f32, 0.0f32);
        for (i, gpu) in span.iter().enumerate() {
            let cpu = &reference[first as usize + i];
            for k in 0..3 {
                pos = pos.max((gpu.position[k] - cpu.position[k]).abs());
                nrm = nrm.max((gpu.normal[k] - cpu.normal[k]).abs());
                tan = tan.max((gpu.tangent[k] - cpu.tangent[k]).abs());
            }
            assert_eq!(gpu.uv, cpu.uv);
            assert_eq!(gpu.weights, cpu.weights);
            assert_eq!(gpu.tangent[3], cpu.tangent[3], "handedness kept");
        }
        eprintln!("object {key:#x}: max error position {pos:e} m, normal {nrm:e}, tangent {tan:e}");
        assert!(pos < 1e-4, "position error {pos}");
        assert!(nrm < 1e-4 && tan < 1e-4, "normal {nrm} tangent {tan}");
    }

    // The frame matches the CPU-deformed scene drawn without morphs.
    let reference_lab = scene(&p, Some((SKIN_WEIGHTS, RIGID_WEIGHTS)));
    let mut reference_frame = reference_lab.as_frame_scene_with_quality(quality);
    reference_frame.meshes[0].mesh_key = PANEL_MESH + 10;
    reference_frame.meshes[1].mesh_key = PANEL_MESH + 11;
    reference_frame.meshes[2].mesh_key = PANEL_MESH + 1;
    let mut reference_renderer = FrameRenderer::new();
    let reference = reference_renderer
        .render_offscreen(&device, &reference_frame)
        .expect("reference")
        .pixels;
    let (changed, _) = diff(&plain, &morphed);
    assert!(changed > 500, "the morphs show ({changed} px changed)");
    let (off, max) = diff(&morphed, &reference);
    eprintln!("morphed vs CPU reference: {off} px over 2/255 (max {max})");
    assert!(
        off <= 8,
        "GPU morphs match the CPU-deformed frame ({off} px, max {max})"
    );

    // Steady frames dispatch nothing; weight changes create nothing.
    let _ = renderer.render_offscreen(&device, &frame).expect("steady");
    assert_eq!(renderer.morph_stats().dispatched, 0, "steady frame");
    let before = frame_path_counters();
    let created = renderer.morph_stats().buffers_created;
    let new_skin = [0.2, 0.9];
    let new_rigid = [1.0, -0.2];
    renderer.set_morph_instances(&instances(&p, &keys, new_skin, new_rigid));
    let changed_frame = renderer
        .render_offscreen(&device, &frame)
        .expect("changed")
        .pixels;
    let spent = frame_path_counters().since(before);
    eprintln!("weight change frame: {spent:?}");
    assert_eq!(renderer.morph_stats().dispatched, 2);
    assert_eq!(renderer.morph_stats().buffers_created, created);
    assert_eq!(spent.buffers, 0, "{spent:?}");
    assert_eq!(spent.bind_groups, 0, "{spent:?}");
    assert_eq!(spent.pipelines, 0, "{spent:?}");
    let changed_lab = scene(&p, Some((new_skin, new_rigid)));
    let mut changed_ref = changed_lab.as_frame_scene_with_quality(quality);
    changed_ref.meshes[0].mesh_key = PANEL_MESH + 20;
    changed_ref.meshes[1].mesh_key = PANEL_MESH + 21;
    changed_ref.meshes[2].mesh_key = PANEL_MESH + 1;
    let changed_reference = FrameRenderer::new()
        .render_offscreen(&device, &changed_ref)
        .expect("changed reference")
        .pixels;
    let (off, max) = diff(&changed_frame, &changed_reference);
    assert!(off <= 8, "new weights match the CPU frame ({off} px, max {max})");

    // Clearing restores the plain frame exactly.
    renderer.set_morph_instances(&[]);
    let restored = renderer
        .render_offscreen(&device, &frame)
        .expect("restored")
        .pixels;
    assert_eq!(restored, plain, "clearing the morphs restores the frame");
    assert_eq!(renderer.morph_stats().instances, 0);
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn morphs_match_the_cpu_reference_per_object() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_paths(FrameQuality::LAB);
}

#[test]
fn morphs_match_the_cpu_reference_instanced() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_paths(FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    });
}

#[test]
fn a_morph_on_a_mesh_with_another_vertex_count_draws_undeformed() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let p = panels();
    let lab = scene(&p, None);
    let mut frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    frame.meshes.truncate(2);
    frame.meshes[0].mesh_key = PANEL_MESH;
    frame.meshes[1].mesh_key = PANEL_MESH + 1;
    let keys: Vec<u64> = frame.meshes.iter().map(|m| m.object_key).collect();
    let short = Arc::new(MorphTargets::new(&p.rigid.0[..N * N - 1], &[]).expect("short set"));
    let mut renderer = FrameRenderer::new();
    let plain = renderer.render_offscreen(&device, &frame).expect("plain").pixels;
    renderer.set_morph_instances(&[MorphInstance {
        object_key: keys[1],
        targets: short,
        weights: vec![1.0],
        mode: MorphMode::Game,
    }]);
    let frame_px = renderer
        .render_offscreen(&device, &frame)
        .expect("mismatched")
        .pixels;
    assert_eq!(frame_px, plain);
    let stats = renderer.morph_stats();
    assert_eq!((stats.drawn, stats.mismatched), (0, 1), "{stats:?}");
}
