// SPDX-License-Identifier: MPL-2.0
//! Render-world entry lifecycle (W2.2): every entry a frame's draws refer to lives until the
//! frame's last pass, under the three races the v0.1.13.1 hotfix suspected and one it did not.
//!
//! - The skin palette grows in a frame that also draws objects created before the growth.
//! - A mesh key is released in the same frame that still draws it.
//! - A morph instance draws under two mesh keys in one frame: each key keeps its own deformed
//!   copy, so none is replaced while a draw holds it, and both draw deformed.
//!
//! Each frame must skip no draw (`DrawIntegrity::skipped_draws`) and match the same scene drawn
//! by a fresh renderer. The sweep-with-stale-`last_used` race and forced missing entries are unit
//! tests in `frame/lifecycle.rs` (they need the renderer's private caches).

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, JointMatrix, LabMaterial,
    LabMesh, LabScene, LabVertex, MorphInstance, MorphMode, MorphTargets, RenderDevice,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad at `(x, y)`, `size` metres, every vertex weighted to joint `joint` when skinned.
fn quad(x: f32, y: f32, size: f32, joint: Option<u32>) -> Vec<LabVertex> {
    let h = size * 0.5;
    let mut vertices = vec![
        LabVertex::from_pos_normal_uv([x - h, y - h, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + h, y - h, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + h, y + h, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x - h, y + h, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
    ];
    if let Some(joint) = joint {
        for v in &mut vertices {
            v.joints = [joint, 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
        }
    }
    vertices
}

fn lab(meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 0.0, 4.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.6, 0.5, 1.0],
            ..LabMaterial::default()
        }],
        meshes,
    }
}

/// `count` skinned quads in a row, each with a palette of `joints` joints whose last one lifts
/// the quad 0.1 m (the vertices read that joint).
fn skinned_row(count: usize, joints: usize) -> Vec<LabMesh> {
    (0..count)
        .map(|i| {
            let mut palette = vec![JointMatrix::identity(); joints];
            palette[joints - 1] = JointMatrix::translation(0.0, 0.1, 0.0);
            LabMesh {
                vertices: quad(i as f32 * 0.3 - 1.2, -0.4, 0.25, Some(joints as u32 - 1)),
                indices: vec![0, 1, 2, 0, 2, 3],
                joints: palette,
                ..LabMesh::unskinned()
            }
        })
        .collect()
}

/// Stable keys per mesh index (`base` keeps scenes that share meshes on the same keys).
fn keyed<'a>(lab: &'a LabScene, quality: FrameQuality, base: u64) -> FrameScene<'a> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    for (i, mesh) in scene.meshes.iter_mut().enumerate() {
        mesh.mesh_key = base + i as u64;
        mesh.object_key = (base << 8) + i as u64;
        mesh.material_key = 0xD1_0000;
    }
    scene
}

fn qualities() -> [FrameQuality; 2] {
    [
        FrameQuality::default(),
        FrameQuality {
            gpu_driven: true,
            ..FrameQuality::default()
        },
    ]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Frame 1 draws 4 skinned quads (the palette fits the first buffer); frame 2 adds 6 more with
/// 256 joints each, so the palette outgrows its buffer while the first quads' objects (bound to
/// the old buffer) draw again. Nothing is skipped and frame 2 matches a fresh renderer.
#[test]
fn the_skin_palette_grows_mid_scene_without_losing_a_draw() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let small = lab(skinned_row(4, 2));
    let mut grown_meshes = skinned_row(4, 2);
    grown_meshes.extend(skinned_row(6, 256).into_iter().map(|mut m| {
        for v in &mut m.vertices {
            v.position[1] += 0.6;
        }
        m
    }));
    let grown = lab(grown_meshes);
    for quality in qualities() {
        let mut renderer = FrameRenderer::new();
        renderer
            .render_offscreen(&device, &keyed(&small, quality, 0x5100))
            .expect("small palette");
        let image = renderer
            .render_offscreen(&device, &keyed(&grown, quality, 0x5100))
            .expect("grown palette")
            .pixels;
        let integrity = renderer.draw_integrity();
        assert_eq!(integrity.skipped_draws, 0, "{integrity:?}");
        assert_eq!(integrity.objects_rebuilt, 0, "{integrity:?}");
        assert_eq!(renderer.draw_stats().drawn, 10, "{:?}", renderer.draw_stats());
        let fresh = FrameRenderer::new()
            .render_offscreen(&device, &keyed(&grown, quality, 0x5100))
            .expect("fresh")
            .pixels;
        assert_eq!(
            image, fresh,
            "gpu_driven {}: old objects rebound to the grown palette",
            quality.gpu_driven
        );
    }
}

/// The host releases a mesh key (its last `Arc` dropped) and the same frame still draws it:
/// the draw re-creates the entry, skips nothing and matches a fresh renderer.
#[test]
fn a_mesh_released_in_the_frame_that_draws_it_still_draws() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab(vec![
        LabMesh {
            vertices: quad(-0.5, 0.0, 0.6, None),
            indices: vec![0, 1, 2, 0, 2, 3],
            ..LabMesh::unskinned()
        },
        LabMesh {
            vertices: quad(0.5, 0.0, 0.6, Some(0)),
            indices: vec![0, 1, 2, 0, 2, 3],
            joints: vec![JointMatrix::translation(0.0, 0.2, 0.0)],
            ..LabMesh::unskinned()
        },
    ]);
    for quality in qualities() {
        let scene = keyed(&lab, quality, 0x7200);
        let mut renderer = FrameRenderer::new();
        let first = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        let uploads = renderer.cache_stats().mesh_uploads;
        let keys: Vec<u64> = scene.meshes.iter().map(|m| m.mesh_key).collect();
        renderer.release_meshes(&keys);
        let image = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        let integrity = renderer.draw_integrity();
        assert_eq!(integrity.skipped_draws, 0, "{integrity:?}");
        assert_eq!(image, first, "gpu_driven {}", quality.gpu_driven);
        assert!(
            renderer.cache_stats().mesh_uploads >= uploads + 2,
            "the released keys uploaded again: {:?}",
            renderer.cache_stats()
        );
    }
}

/// One morph instance (one object key) drawn under two mesh keys in the same frame. Each draw
/// used to replace the instance's deformed copy: the first draw's copy vanished before its
/// passes (a skipped draw), and once its removal was deferred it drew undeformed (review
/// holes-life #2), with a fresh copy uploaded every frame. Now each key keeps its own copy and
/// the pre-pass fills both: no draw is skipped, nothing is replaced or re-uploaded after the
/// first frame, and both quads draw deformed, as two separately morphed objects do.
///
/// The two primitives share the entity's vertex array (and so its morph targets) and its model
/// matrix, each drawing one quad of it. An object key names one model matrix
/// ([`FrameMesh::object_key`]): the test used to give the second primitive a model of its own,
/// and one of the two models then drew both quads (`gpu_driven` off: the second, from the
/// per-object uniform). The gate measured 130 pixels off the two-object reference; without any
/// morph the same scene is 100 pixels off it in both modes.
#[test]
fn a_morph_copy_replaced_mid_frame_stays_until_the_frame_ends() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut vertices = quad(0.0, 0.0, 0.8, None);
    vertices.extend(quad(1.0, 0.0, 0.8, None));
    let target = regia_asset::CpuMorphTarget {
        name: "push".into(),
        vertex_indices: vec![2, 3, 6, 7],
        position_deltas: vec![[0.0, 0.2, 0.3]; 4],
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
    };
    let targets = Arc::new(MorphTargets::new(&vertices, &[target]).expect("targets"));
    let lab = lab(vec![
        LabMesh {
            vertices: vertices.clone(),
            indices: vec![0, 1, 2, 0, 2, 3],
            ..LabMesh::unskinned()
        },
        LabMesh {
            vertices,
            indices: vec![4, 5, 6, 4, 6, 7],
            ..LabMesh::unskinned()
        },
    ]);
    for quality in qualities() {
        let mut scene = keyed(&lab, quality, 0x9300);
        // Same object, two mesh keys (two primitives of one entity).
        let object = scene.meshes[0].object_key;
        scene.meshes = scene
            .meshes
            .iter()
            .map(|m| FrameMesh {
                object_key: object,
                ..*m
            })
            .collect();
        let mut renderer = FrameRenderer::new();
        renderer.set_morph_instances(&[MorphInstance {
            object_key: object,
            targets: Arc::clone(&targets),
            weights: vec![1.0],
            mode: MorphMode::Game,
        }]);
        let mut image = Vec::new();
        let mut uploads = 0;
        for frame in 0..4 {
            image = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
            let integrity = renderer.draw_integrity();
            assert_eq!(integrity.skipped_draws, 0, "frame {frame}: {integrity:?}");
            assert_eq!(
                integrity.deferred_removals_total, 0,
                "frame {frame}: no copy is replaced"
            );
            if frame == 1 {
                uploads = renderer.cache_stats().mesh_uploads;
            }
        }
        assert_eq!(
            renderer.cache_stats().mesh_uploads,
            uploads,
            "gpu_driven {}: no deformed copy is uploaded again after the first frames",
            quality.gpu_driven
        );
        // The same two quads as two objects, each with its own instance: both deformed.
        let apart = keyed(&lab, quality, 0x9300);
        let objects: Vec<u64> = apart.meshes.iter().map(|m| m.object_key).collect();
        let mut reference = FrameRenderer::new();
        reference.set_morph_instances(
            &objects
                .iter()
                .map(|&object_key| MorphInstance {
                    object_key,
                    targets: Arc::clone(&targets),
                    weights: vec![1.0],
                    mode: MorphMode::Game,
                })
                .collect::<Vec<_>>(),
        );
        let mut want = Vec::new();
        for _ in 0..2 {
            want = reference
                .render_offscreen(&device, &apart)
                .expect("reference")
                .pixels;
        }
        let differ = |a: &[u8], b: &[u8]| {
            a.as_chunks::<4>()
                .0
                .iter()
                .zip(b.as_chunks::<4>().0)
                .filter(|(p, q)| (0..3).any(|k| p[k].abs_diff(q[k]) > 2))
                .count()
        };
        assert_eq!(
            differ(&image, &want),
            0,
            "gpu_driven {}: both draws of the object are deformed",
            quality.gpu_driven
        );
        // And the deformation shows: undeformed, the quads differ.
        let flat = FrameRenderer::new()
            .render_offscreen(&device, &apart)
            .expect("flat")
            .pixels;
        assert!(differ(&image, &flat) > 20, "gpu_driven {}", quality.gpu_driven);
    }
}
