// SPDX-License-Identifier: MPL-2.0
//! A sync group reuses its leader clip name when the same clip wins again.
//!
//! Two steps of a walk/run blend must not allocate a new leader string, and the leader's
//! buffer must stay put.

use std::collections::HashMap;

use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, BlendNode, ClipNode};
use regia_animation::hot_count::{self, sync_clip_strings};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::{Pose, translated};
use regia_animation::sync::{SyncGroupDef, SyncGroups, SyncSetup};
use regia_animation::{SYNC_FORMAT_VERSION, Skeleton};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

const DT: f32 = 1.0 / 30.0;

fn clip_node(clip: &str) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: clip.into(),
        looping: true,
        speed: 1.0,
        sync_group: Some("gait".into()),
    })
}

struct Driver {
    skeleton: Skeleton,
    clips: ClipLibrary,
    graph: AnimGraph,
    params: ParameterSet,
    instance: GraphInstance,
    sync: SyncGroups,
    scratch: Vec<Pose>,
}

impl Driver {
    fn new() -> Self {
        let graph = AnimGraph {
            parameters: vec![ParamDecl("Alpha".into(), ParamValue::Float(0.0))],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(clip_node("walk")),
                b: Box::new(clip_node("run")),
                alpha_param: "Alpha".into(),
            }),
        };
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("walk", 1.0));
        clips.insert(AnimationClip::empty("run", 0.6));
        let params = graph.default_parameters();
        let instance = GraphInstance::from_node(&graph.root);
        Driver {
            skeleton: Skeleton::from_local_bind(
                vec!["root".into()],
                vec![None],
                vec![translated(Vec3::ZERO)],
            )
            .expect("skeleton"),
            clips,
            graph,
            params,
            instance,
            sync: SyncGroups::default(),
            scratch: Vec::new(),
        }
    }

    fn step(&mut self, setup: &SyncSetup) {
        let sockets = HashMap::new();
        let mut pose = Pose::from_identity(1);
        let mut ctx = EvalContext::new(
            &self.skeleton,
            &self.clips,
            &self.params,
            DT,
            Entity::PLACEHOLDER,
            &sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_sync(Some(setup), std::mem::take(&mut self.sync));
        evaluate_graph(&self.graph, &mut self.instance, &mut ctx, &mut pose).expect("evaluate");
        self.sync = ctx.take_sync_state();
        self.scratch = ctx.finish().scratch;
    }

    fn leader_ptr(&self) -> *const u8 {
        self.sync
            .groups()
            .first()
            .and_then(|g| g.leader.as_ref())
            .expect("a leader")
            .clip
            .as_ptr()
    }

    fn leader_name(&self) -> &str {
        &self
            .sync
            .groups()
            .first()
            .and_then(|g| g.leader.as_ref())
            .expect("a leader")
            .clip
    }
}

#[test]
fn the_leader_clip_buffer_stays_when_the_same_clip_wins() {
    let _guard = hot_count::lock_for_test();
    let setup = SyncSetup {
        format_version: SYNC_FORMAT_VERSION,
        groups: vec![SyncGroupDef {
            name: "gait".into(),
            roles: Vec::new(),
        }],
        markers: Vec::new(),
    };
    let mut driver = Driver::new();
    driver.step(&setup);
    driver.step(&setup);
    assert_eq!(driver.leader_name(), "walk");
    let ptr = driver.leader_ptr();
    let before = sync_clip_strings();
    driver.step(&setup);
    let delta = sync_clip_strings() - before;
    assert_eq!(driver.leader_name(), "walk");
    assert_eq!(
        driver.leader_ptr(),
        ptr,
        "electing the same leader allocated a new clip string"
    );
    assert_eq!(delta, 0, "the step allocated {delta} sync clip strings");
}
