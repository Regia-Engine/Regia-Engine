// SPDX-License-Identifier: MPL-2.0
//! Skinned instancing (ADR-1368): vertex-shader-skinned meshes with a joint-palette bound draw
//! in the GPU-driven instanced batches of the colour pass, one indirect draw per
//! `(cull mode, mesh, material, skinned)` batch, each unit skinned from its own `joint_base` in
//! the frame's shared palette.
//!
//! - A 64-unit skinned crowd renders bit-identical through the instanced batches and through
//!   the per-object path (instancing switched off) in the same run, and its 64 colour draws
//!   become one batch.
//! - A unit outside the frustum by its palette bound is culled, and the frame still matches the
//!   per-object path.
//! - Compute-skinned twins keep drawing one by one.
//!
//! No golden: every check compares two paths of the same build or reads the draw counters.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, JointMatrix, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, RenderDevice,
};

const W: u32 = 128;
const H: u32 = 96;
/// Units in the crowd.
const UNITS: usize = 64;
/// Shared mesh key of the units: one GPU mesh, so they can batch.
const UNIT_MESH: u64 = 0x5C_0001;
/// Second unit mesh (the mixed crowd).
const OTHER_MESH: u64 = 0x5C_0002;
const GROUND_MESH: u64 = 0x5C_0003;
const UNIT_MATERIAL: u64 = 0x5C_1000;
const GROUND_MATERIAL: u64 = 0x5C_1001;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Instancing needs `INDIRECT_FIRST_INSTANCE` + multi-draw indirect.
fn gpu_driven(device: &RenderDevice) -> bool {
    device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance
}

fn quality() -> FrameQuality {
    FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
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

/// A 0.3 × 0.6 × 0.3 m unit standing on the origin: the lower half weighted to joint 0, the
/// upper half to joint 1 (two joints, so the skin is not folded into a rigid draw).
fn unit_mesh() -> LabMesh {
    let (vertices, indices) = regia_render::box_mesh([0.15, 0.3, 0.15]);
    let vertices = vertices
        .into_iter()
        .map(|mut v: LabVertex| {
            v.position[1] += 0.3;
            let upper = v.position[1] > 0.3;
            v.joints = [u32::from(upper), 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
            v
        })
        .collect();
    LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// Unit `i`'s palette: joint 0 at rest, joint 1 leaning its upper half by a per-unit amount, so
/// every unit has its own pose and its own `joint_base` in the frame palette.
fn pose(i: usize) -> Vec<JointMatrix> {
    let lean = 0.02 * (i % 7) as f32;
    let lift = 0.01 * (i % 5) as f32;
    vec![
        JointMatrix::identity(),
        JointMatrix::translation(lean, lift, -lean * 0.5),
    ]
}

/// Ground plane facing +Y.
fn ground(half: f32) -> LabMesh {
    let corners = [
        [-half, 0.0, -half],
        [half, 0.0, -half],
        [half, 0.0, half],
        [-half, 0.0, half],
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&position| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0]))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 1,
        ..LabMesh::unskinned()
    }
}

/// The ground and an 8 × 8 crowd seen from above and behind, the sun casting their shadows.
/// `away` moves that many units (the first ones) far outside the view by their joints alone.
fn crowd(away: usize) -> LabScene {
    let mut meshes = vec![ground(8.0)];
    let unit = unit_mesh();
    for i in 0..UNITS {
        let (x, z) = ((i % 8) as f32 * 0.6 - 2.1, (i / 8) as f32 * 0.6 - 2.1);
        let mut joints = pose(i);
        if i < away {
            // Both joints carry the unit 200 m to the side: the bind pose is in view, the
            // posed unit (and its palette bound) is not.
            joints = vec![JointMatrix::translation(200.0, 0.0, 0.0); 2];
        }
        meshes.push(LabMesh {
            model: translate(x, 0.0, z),
            joints,
            ..unit.clone()
        });
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 4.0, -6.0],
        target: [0.0, 0.2, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 60.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 3_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -0.8, 0.4], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.7, 0.45, 0.3, 1.0],
                roughness: 0.6,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

/// Stable keys: one mesh and material key for the units (they batch), one object per unit.
/// Units at an index in `other` use [`OTHER_MESH`] (same vertices, a second batch).
fn keyed<'a>(lab: &'a LabScene, other: &[usize]) -> FrameScene<'a> {
    let mut scene = lab.as_frame_scene_with_quality(quality());
    for (i, mesh) in scene.meshes.iter_mut().enumerate() {
        mesh.object_key = 0x5C_2000 + i as u64;
        if i == 0 {
            mesh.mesh_key = GROUND_MESH;
            mesh.material_key = GROUND_MATERIAL;
        } else {
            mesh.mesh_key = if other.contains(&(i - 1)) {
                OTHER_MESH
            } else {
                UNIT_MESH
            };
            mesh.material_key = UNIT_MATERIAL;
        }
    }
    scene
}

/// The frame with skinned instancing on and off (fresh renderers), and the counters of each.
fn on_and_off(
    device: &RenderDevice,
    scene: &FrameScene<'_>,
) -> (
    Vec<u8>,
    Vec<u8>,
    regia_render::DrawStats,
    regia_render::DrawStats,
    regia_render::DrawIntegrity,
) {
    let mut on = FrameRenderer::new();
    assert!(on.skinned_instancing(), "skinned instancing is on by default");
    let image_on = on.render_offscreen(device, scene).expect("instancing on").pixels;
    let mut off = FrameRenderer::new();
    off.set_skinned_instancing(false);
    let image_off = off
        .render_offscreen(device, scene)
        .expect("instancing off")
        .pixels;
    (
        image_on,
        image_off,
        on.draw_stats(),
        off.draw_stats(),
        on.draw_integrity(),
    )
}

fn differing_pixels(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| x != y)
        .count()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_skinned_crowd_renders_bit_identical_in_one_batch() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = crowd(0);
    let scene = keyed(&lab, &[]);
    let (image_on, image_off, on, off, integrity) = on_and_off(&device, &scene);
    eprintln!("on  {on:?}\noff {off:?}");
    // The counters see the crowd either way: 64 skinned colour draws, one possible batch.
    assert_eq!(off.skinned_draws, UNITS, "{off:?}");
    assert_eq!(off.skinned_batches_possible, 1, "{off:?}");
    assert_eq!(off.skinned_instanced, 0, "{off:?}");
    assert_eq!(off.skinned_batches, 0, "{off:?}");
    assert_eq!(integrity.skinned_bounded, UNITS, "{integrity:?}");
    assert_eq!(on.skinned_draws, UNITS, "{on:?}");
    assert_eq!(on.skinned_batches_possible, 1, "{on:?}");
    if !gpu_driven(&device) {
        eprintln!("device has no indirect first-instance: skinned draws stay on the CPU path");
        assert_eq!(on.skinned_instanced, 0, "{on:?}");
        assert_eq!(image_on, image_off);
        return;
    }
    // 64 per-object colour draws become one instanced draw.
    assert_eq!(on.skinned_instanced, UNITS, "{on:?}");
    assert_eq!(on.skinned_batches, 1, "{on:?}");
    assert_eq!(
        off.per_object - on.per_object,
        UNITS,
        "the crowd left the per-object loop: {off:?} vs {on:?}"
    );
    assert_eq!(on.instanced - off.instanced, UNITS);
    let empty = crowd(UNITS);
    let ground_only = FrameRenderer::new()
        .render_offscreen(&device, &keyed(&empty, &[]))
        .expect("ground only")
        .pixels;
    assert!(
        differing_pixels(&image_on, &ground_only) > 200,
        "the crowd is on screen"
    );
    let diff = differing_pixels(&image_on, &image_off);
    assert_eq!(
        diff, 0,
        "the instanced skinned crowd must be bit-identical to the per-object frame ({diff} px differ)"
    );
}

#[test]
fn a_mixed_crowd_batches_per_mesh() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = crowd(0);
    // Every third unit uses the second mesh key: two batches.
    let other: Vec<usize> = (0..UNITS).filter(|i| i % 3 == 0).collect();
    let scene = keyed(&lab, &other);
    let (image_on, image_off, on, off, _) = on_and_off(&device, &scene);
    assert_eq!(off.skinned_batches_possible, 2, "{off:?}");
    if gpu_driven(&device) {
        assert_eq!(on.skinned_instanced, UNITS, "{on:?}");
        assert_eq!(on.skinned_batches, 2, "{on:?}");
    }
    assert_eq!(differing_pixels(&image_on, &image_off), 0);
}

#[test]
fn a_unit_outside_the_frustum_by_its_palette_bound_is_culled() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = crowd(1);
    let scene = keyed(&lab, &[]);
    let (image_on, image_off, on, off, integrity) = on_and_off(&device, &scene);
    assert_eq!(integrity.skinned_culled, 1, "{integrity:?}");
    assert_eq!(on.skinned_draws, UNITS - 1, "{on:?}");
    assert_eq!(off.skinned_draws, UNITS - 1, "{off:?}");
    if gpu_driven(&device) {
        assert_eq!(on.skinned_instanced, UNITS - 1, "{on:?}");
        assert_eq!(on.skinned_batches, 1, "{on:?}");
    }
    assert_eq!(differing_pixels(&image_on, &image_off), 0);
}

#[test]
fn compute_skinned_twins_stay_per_instance() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = crowd(0);
    let scene = keyed(&lab, &[]);
    let mut renderer = FrameRenderer::new();
    renderer.set_compute_skinning(true);
    // Twins are built on the first frame; the second draws from them.
    renderer.render_offscreen(&device, &scene).expect("first frame");
    renderer.render_offscreen(&device, &scene).expect("second frame");
    let stats = renderer.draw_stats();
    let skinning = renderer.skinning_stats();
    eprintln!("{stats:?}\n{skinning:?}");
    assert_eq!(stats.skinned_draws, UNITS, "{stats:?}");
    if skinning.instances == 0 {
        eprintln!("device without compute skinning: the vertex-shader path batches instead");
        return;
    }
    // Each twin is a mesh of its own: nothing to batch, nothing instanced.
    assert_eq!(stats.skinned_instanced, 0, "{stats:?}");
    assert_eq!(stats.skinned_batches, 0, "{stats:?}");
    assert_eq!(stats.skinned_batches_possible, UNITS, "{stats:?}");
}
