// SPDX-License-Identifier: MPL-2.0
//! Morph weights end to end (Tincal engine items C1 + C2 → B1): a skinned rig plays a clip with a
//! morph weight track, an additive expression layer adds a smile, `SkinnedMeshPlugin` hands the
//! palette to the renderer, extraction picks up the weights, and the GPU morph pass deforms the
//! mesh exactly as the CPU reference does for the composed weights.

use std::sync::Arc;

use regia_animation::clip::{MorphTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_framework::skinning::SkinnedMeshPlugin;
use regia_render::{
    CpuMaterial, CpuMesh, ExtractedFrame, FrameRenderer, LabVertex, MeshMorphTargets, MeshRenderer,
    MorphMode, MorphTargets, RenderDevice, SkinJoints,
};
use regia_scene::Transform;

fn panel() -> (Vec<LabVertex>, Vec<u32>) {
    const N: usize = 4;
    let mut vertices = Vec::new();
    for j in 0..N {
        for i in 0..N {
            let mut v = LabVertex::from_pos_normal_uv(
                [i as f32 * 0.3 - 0.45, j as f32 * 0.3 - 0.45, -2.0],
                [0.0, 0.0, 1.0],
                [0.0, 0.0],
            );
            v.weights = [1.0, 0.0, 0.0, 0.0];
            vertices.push(v);
        }
    }
    let mut indices = Vec::new();
    for j in 0..N - 1 {
        for i in 0..N - 1 {
            let a = (j * N + i) as u32;
            indices.extend([a, a + 1, a + N as u32 + 1, a, a + N as u32 + 1, a + N as u32]);
        }
    }
    (vertices, indices)
}

fn targets(vertices: &[LabVertex]) -> MorphTargets {
    let jaw = regia_asset::CpuMorphTarget {
        name: "jawOpen".into(),
        vertex_indices: vec![0, 1, 2, 3],
        position_deltas: vec![[0.0, -0.2, 0.0]; 4],
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
    };
    let smile = regia_asset::CpuMorphTarget {
        name: "mouthSmile".into(),
        vertex_indices: vec![4, 7],
        position_deltas: vec![[-0.1, 0.1, 0.0], [0.1, 0.1, 0.0]],
        normal_deltas: vec![[0.0, 0.3, 0.0]; 2],
        tangent_deltas: Vec::new(),
    };
    MorphTargets::new(vertices, &[jaw, smile]).expect("targets")
}

#[test]
fn a_clip_track_and_a_layer_reach_the_gpu_morph_pass() {
    let mut app = App::new();
    app.add_plugins((
        regia_app::MinimalPlugins,
        regia_scene::ScenePlugin,
        AnimationPlugin,
        SkinnedMeshPlugin,
    ));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(16),
    ));
    let skeleton = Arc::new(
        Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton"),
    );
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut clip = AnimationClip::empty("talk", 1.0);
    clip.morph_tracks
        .push(MorphTrack::new(0, TrackCurve::Static(0.6)));
    let (vertices, indices) = panel();
    let set = Arc::new(targets(&vertices));
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let rig = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::ZERO),
            binding,
            Animator::single(clip, true),
            MeshRenderer::new(Arc::clone(&mesh), Arc::new(CpuMaterial::default())),
            MeshMorphTargets {
                targets: Arc::clone(&set),
                mode: MorphMode::Game,
            },
            MorphTargetNames::new(["jawOpen", "mouthSmile"]),
            MorphLayers {
                layers: vec![MorphLayer {
                    weight: 0.5,
                    ..MorphLayer::new("smile", MorphLayerMode::Additive).with("mouthSmile", 1.0)
                }],
                ..MorphLayers::default()
            },
        ))
        .id();
    for _ in 0..3 {
        app.update();
    }

    // Clip track (jaw 0.6) + additive layer (smile 1.0 × 0.5).
    let weights = app.world().get::<MorphWeights>(rig).expect("weights").0.clone();
    assert_eq!(weights.len(), 2);
    assert!(
        (weights[0] - 0.6).abs() < 1e-6 && (weights[1] - 0.5).abs() < 1e-6,
        "{weights:?}"
    );
    // C1: the skinning bridge handed the renderer the rig's palette.
    let palette = &app
        .world()
        .get::<SkinJoints>(rig)
        .expect("SkinJoints from SkinnedMeshPlugin")
        .palette;
    assert_eq!(palette.len(), 1);

    // Extraction.
    let world = app.world_mut();
    world.init_resource::<ExtractedFrame>();
    world
        .run_system_once(regia_render::extract_frame)
        .expect("extract");
    world
        .run_system_once(regia_render::extract_morphs)
        .expect("extract morphs");
    let extracted = world.resource::<ExtractedFrame>().clone();
    assert_eq!(extracted.morphs.len(), 1);
    let instance = &extracted.morphs[0];
    assert_eq!(instance.object_key, rig.to_bits());
    assert_eq!(instance.weights, weights);

    // B1: the GPU pass deforms the mesh as the CPU reference does.
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_morph_instances(&extracted.morphs);
    let scene = extracted.frame_scene(0, 64, 64);
    renderer.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(renderer.morph_stats().dispatched, 1);
    let (first, span) = renderer.read_morphed_span(&device, rig.to_bits()).expect("span");
    let mut reference = mesh.vertices.clone();
    set.apply(&set.effective_weights(&weights, MorphMode::Game), &mut reference);
    for (i, v) in span.iter().enumerate() {
        let r = &reference[first as usize + i];
        for k in 0..3 {
            assert!((v.position[k] - r.position[k]).abs() < 1e-4, "vertex {i}");
            assert!((v.normal[k] - r.normal[k]).abs() < 1e-4, "vertex {i}");
        }
    }
    assert!((reference[0].position[1] - (mesh.vertices[0].position[1] - 0.12)).abs() < 1e-3);
}
