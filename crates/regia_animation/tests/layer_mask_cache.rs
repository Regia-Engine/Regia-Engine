// SPDX-License-Identifier: MPL-2.0
//! A layer mask is resolved once per skeleton, not once per evaluation.
//!
//! Two evaluations of the same layer must not walk joint names or allocate a weight vector
//! the second time.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, ClipNode, JointMaskDef, LayerEntry, LayerMode, LayerNode};
use regia_animation::hot_count::{self, mask_resolves};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::{Pose, translated};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into(), "hand".into()],
        vec![None, Some(0), Some(1)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y), translated(Vec3::Y)],
    )
    .expect("skeleton")
}

fn clip_node(name: &str) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: name.into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    })
}

#[test]
fn the_second_evaluation_does_not_resolve_the_layer_mask() {
    let _guard = hot_count::lock_for_test();
    let skeleton = skeleton();
    let graph = AnimGraph {
        parameters: vec![ParamDecl("W".into(), ParamValue::Float(1.0))],
        root: AnimNode::Layer(LayerNode {
            base: Box::new(clip_node("idle")),
            layers: vec![LayerEntry {
                graph: Box::new(clip_node("wave")),
                mask: JointMaskDef {
                    joints: vec!["arm".into()],
                    include_children: true,
                },
                mode: LayerMode::Override,
                weight_param: "W".into(),
                weight_inertialize: 0.0,
            }],
        }),
    };
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    clips.insert(AnimationClip::empty("wave", 1.0));
    let params = graph.default_parameters();
    let mut instance = GraphInstance::from_node(&graph.root);
    let sockets = HashMap::new();
    let mut reuse = EvalReuse::default();
    let mut step = |scratch: Vec<Pose>| {
        let mut pose = Pose::from_identity(skeleton.len());
        let mut ctx = EvalContext::new(
            &skeleton,
            &clips,
            &params,
            1.0 / 30.0,
            Entity::PLACEHOLDER,
            &sockets,
            scratch,
        )
        .with_reuse(std::mem::take(&mut reuse));
        evaluate_graph(&graph, &mut instance, &mut ctx, &mut pose).expect("evaluate");
        reuse = ctx.take_reuse();
        ctx.finish().scratch
    };
    let scratch = step(Vec::new());
    let before = mask_resolves();
    let _scratch = step(scratch);
    let delta = mask_resolves() - before;
    assert_eq!(
        delta, 0,
        "the second evaluation resolved the layer mask ({delta} times)"
    );
}
