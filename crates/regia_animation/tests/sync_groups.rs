// SPDX-License-Identifier: MPL-2.0
//! Cross-node sync groups through the public evaluation API (spec 17 § 4b, ADR-1465): a walk
//! and a run cycle of different lengths under one `Blend` node stay in phase, the leader
//! follows the weights without a phase jump, marker sync lines up named markers, and only the
//! leader is heard. Without a `SyncSetup` the clips keep their own clocks (the old behaviour).

use std::collections::HashMap;

use regia_animation::clip::{AnimationClip, Notify};
use regia_animation::eval::{ClipLibrary, EvalContext, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, BlendNode, ClipNode};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::{Pose, translated};
use regia_animation::sync::{ClipMarkers, SyncGroupDef, SyncGroups, SyncMarker, SyncSetup};
use regia_animation::{SYNC_FORMAT_VERSION, Skeleton};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

const DT: f32 = 1.0 / 30.0;
const WALK: f32 = 1.0;
const RUN: f32 = 0.6;

fn clip_node(clip: &str) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: clip.into(),
        looping: true,
        speed: 1.0,
        sync_group: Some("gait".into()),
    })
}

fn graph() -> AnimGraph {
    AnimGraph {
        parameters: vec![ParamDecl("Alpha".into(), ParamValue::Float(0.0))],
        root: AnimNode::Blend(BlendNode {
            a: Box::new(clip_node("walk")),
            b: Box::new(clip_node("run")),
            alpha_param: "Alpha".into(),
        }),
    }
}

fn library() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    let mut walk = AnimationClip::empty("walk", WALK);
    walk.notifies.push(Notify {
        time: 0.5,
        name: "walk_step".into(),
        payload: None,
    });
    let mut run = AnimationClip::empty("run", RUN);
    run.notifies.push(Notify {
        time: 0.3,
        name: "run_step".into(),
        payload: None,
    });
    clips.insert(walk);
    clips.insert(run);
    clips
}

fn gait(markers: bool) -> SyncSetup {
    let marker = |name: &str, time: f32| SyncMarker {
        name: name.into(),
        time,
    };
    SyncSetup {
        format_version: SYNC_FORMAT_VERSION,
        groups: vec![SyncGroupDef {
            name: "gait".into(),
            roles: Vec::new(),
        }],
        markers: if markers {
            vec![
                ClipMarkers {
                    clip: "walk".into(),
                    markers: vec![marker("left", 0.0), marker("right", 0.5)],
                },
                ClipMarkers {
                    clip: "run".into(),
                    markers: vec![marker("left", 0.1), marker("right", 0.4)],
                },
            ]
        } else {
            Vec::new()
        },
    }
}

struct Driver {
    skeleton: Skeleton,
    clips: ClipLibrary,
    graph: AnimGraph,
    params: ParameterSet,
    instance: GraphInstance,
    sync: SyncGroups,
    scratch: Vec<Pose>,
    notifies: Vec<String>,
}

impl Driver {
    fn new() -> Self {
        let graph = graph();
        Driver {
            skeleton: Skeleton::from_local_bind(
                vec!["root".into()],
                vec![None],
                vec![translated(Vec3::ZERO)],
            )
            .expect("skeleton"),
            clips: library(),
            params: graph.default_parameters(),
            instance: GraphInstance::from_node(&graph.root),
            graph,
            sync: SyncGroups::default(),
            scratch: Vec::new(),
            notifies: Vec::new(),
        }
    }

    fn step(&mut self, setup: Option<&SyncSetup>) {
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
        .with_sync(setup, std::mem::take(&mut self.sync));
        evaluate_graph(&self.graph, &mut self.instance, &mut ctx, &mut pose).expect("evaluate");
        self.notifies.extend(ctx.notifies.iter().map(|n| n.name.clone()));
        self.sync = ctx.take_sync_state();
        self.scratch = ctx.finish().scratch;
    }

    /// `(walk time, run time)`.
    fn times(&self) -> (f32, f32) {
        let GraphInstance::Blend { a, b } = &self.instance else {
            return (f32::NAN, f32::NAN);
        };
        match (a.as_ref(), b.as_ref()) {
            (GraphInstance::Clip(a), GraphInstance::Clip(b)) => (a.time, b.time),
            _ => (f32::NAN, f32::NAN),
        }
    }

    fn phases(&self) -> (f32, f32) {
        let (walk, run) = self.times();
        (walk / WALK, run / RUN)
    }
}

/// Distance between two phases on the unit circle.
fn phase_gap(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(1.0);
    d.min(1.0 - d)
}

#[test]
fn followers_stay_in_phase_and_the_leader_follows_the_weights() {
    let setup = gait(false);
    let mut driver = Driver::new();
    driver.params.set_float("Alpha", 0.3);
    for _ in 0..20 {
        driver.step(Some(&setup));
    }
    let (walk, run) = driver.phases();
    assert!(phase_gap(walk, run) < 1e-4, "walk {walk} run {run}");
    let leader = |d: &Driver| {
        d.sync
            .groups()
            .first()
            .and_then(|g| g.leader.as_ref())
            .map(|l| l.clip.clone())
    };
    assert_eq!(leader(&driver).as_deref(), Some("walk"));
    // Run takes over: the shared clock now advances at the run's cadence, without a jump.
    driver.params.set_float("Alpha", 0.8);
    let mut previous = driver.phases().0;
    for _ in 0..30 {
        driver.step(Some(&setup));
        let (walk, run) = driver.phases();
        assert!(phase_gap(walk, run) < 1e-4, "walk {walk} run {run}");
        let advance = (walk - previous).rem_euclid(1.0);
        assert!(
            advance <= DT / RUN + 1e-4,
            "no phase jump at the leader change: {advance}"
        );
        previous = walk;
    }
    assert_eq!(leader(&driver).as_deref(), Some("run"));
    let advance = {
        let before = driver.phases().0;
        driver.step(Some(&setup));
        (driver.phases().0 - before).rem_euclid(1.0)
    };
    assert!((advance - DT / RUN).abs() < 1e-4, "{advance}");
}

#[test]
fn without_a_setup_clips_keep_their_own_clocks() {
    let mut driver = Driver::new();
    driver.params.set_float("Alpha", 0.3);
    for _ in 0..20 {
        driver.step(None);
    }
    let (walk, run) = driver.times();
    assert!((walk - 20.0 * DT).abs() < 1e-4, "{walk}");
    assert!((run - (20.0 * DT).rem_euclid(RUN)).abs() < 1e-4, "{run}");
    assert!(driver.sync.groups().is_empty());
}

#[test]
fn marker_sync_lines_up_named_markers() {
    let setup = gait(true);
    let mut driver = Driver::new();
    driver.params.set_float("Alpha", 0.2);
    for frame in 0..60 {
        driver.step(Some(&setup));
        if frame < 2 {
            continue; // the leader is elected after the first frame
        }
        let (walk, run) = driver.times();
        // Walk left 0.0 → right 0.5 → left 1.0; run left 0.1 → right 0.4 → left 0.7.
        let expected = if walk < 0.5 {
            0.1 + walk / 0.5 * 0.3
        } else {
            (0.4 + (walk - 0.5) / 0.5 * 0.3).rem_euclid(RUN)
        };
        assert!(
            phase_gap(run / RUN, expected / RUN) < 1e-3,
            "frame {frame}: walk {walk} run {run} expected {expected}"
        );
    }
}

#[test]
fn only_the_leader_fires_notifies() {
    let mut synced = Driver::new();
    let mut free = Driver::new();
    for driver in [&mut synced, &mut free] {
        driver.params.set_float("Alpha", 0.5);
    }
    let setup = gait(false);
    for _ in 0..60 {
        synced.step(Some(&setup));
        free.step(None);
    }
    assert!(
        free.notifies.iter().any(|n| n == "run_step"),
        "{:?}",
        free.notifies
    );
    assert!(
        synced.notifies.iter().any(|n| n == "walk_step"),
        "{:?}",
        synced.notifies
    );
    assert!(
        !synced.notifies.iter().any(|n| n == "run_step"),
        "a follower is quiet: {:?}",
        synced.notifies
    );
}

/// A clip and a synced blend space in one declared group share one phase, whichever leads.
#[test]
fn a_blend_space_joins_a_named_group_as_one_member() {
    use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
    use regia_animation::graph::BlendSpace1DNode;

    let mut driver = Driver::new();
    let mut clips = library();
    clips.insert(AnimationClip::empty("survey", 2.0));
    driver.clips = clips;
    driver.graph = AnimGraph {
        parameters: vec![
            ParamDecl("Alpha".into(), ParamValue::Float(0.0)),
            // Walk dominates the space (0.7 against 0.3).
            ParamDecl("Speed".into(), ParamValue::Float(0.3)),
        ],
        root: AnimNode::Blend(BlendNode {
            a: Box::new(clip_node("survey")),
            b: Box::new(AnimNode::BlendSpace1D(BlendSpace1DNode {
                space: BlendSpace1D {
                    param: "Speed".into(),
                    samples: vec![
                        BlendSample1D {
                            value: 0.0,
                            clip: "walk".into(),
                        },
                        BlendSample1D {
                            value: 1.0,
                            clip: "run".into(),
                        },
                    ],
                },
                sync_group: Some("gait".into()),
            })),
            alpha_param: "Alpha".into(),
        }),
    };
    driver.params = driver.graph.default_parameters();
    driver.instance = GraphInstance::from_node(&driver.graph.root);
    let phases = |d: &Driver| {
        let GraphInstance::Blend { a, b } = &d.instance else {
            return (f32::NAN, f32::NAN);
        };
        match (a.as_ref(), b.as_ref()) {
            (GraphInstance::Clip(survey), GraphInstance::BlendSpace1D(runtime)) => (
                survey.time / 2.0,
                runtime.states.last().map_or(f32::NAN, |clock| clock.time),
            ),
            _ => (f32::NAN, f32::NAN),
        }
    };
    let setup = gait(false);
    for (alpha, leader) in [(0.8, "walk"), (0.1, "survey")] {
        driver.params.set_float("Alpha", alpha);
        for _ in 0..20 {
            driver.step(Some(&setup));
        }
        let (survey, space) = phases(&driver);
        assert!(
            phase_gap(survey, space) < 1e-4,
            "alpha {alpha}: survey {survey} space {space}"
        );
        let lead = driver
            .sync
            .groups()
            .first()
            .and_then(|g| g.leader.as_ref())
            .map(|l| l.clip.clone());
        assert_eq!(lead.as_deref(), Some(leader), "alpha {alpha}");
    }
}

/// Audit v033b (Low): the side-effect-free probe of an inertialized transition's target (run at
/// delta 0) left its sync-group record behind. Under a `Blend` at alpha 0 the real evaluation also
/// records at weight 0, the election keeps the first of equal weights, and so the probe's time 0
/// led the group instead of the clip's real time. The probe's records are rewound.
#[test]
fn an_inertialized_transition_probe_does_not_lead_the_sync_group() {
    let setup = gait(false);
    let text = r#"(format_version: 2, parameters: [("Alpha", Float(0.0)), ("Go", Bool(false))],
        root: Blend((
            a: Clip((clip: "walk", looping: true, speed: 1.0, sync_group: None)),
            b: StateMachine((initial: "A",
                states: [("A", Clip((clip: "walk", looping: true, speed: 1.0, sync_group: None))),
                         ("B", Clip((clip: "run", looping: true, speed: 1.0, sync_group: Some("gait"))))],
                transitions: [
                    (from: Some("A"), to: "B", conditions: [Bool(param: "Go", value: true)],
                     duration: 0.3, curve: Linear, interruptible: true, blend: Inertialize),
                ])),
            alpha_param: "Alpha",
        )))"#;
    let graph = AnimGraph::from_ron_str(text).expect("graph");
    let mut driver = Driver::new();
    driver.params = graph.default_parameters();
    driver.instance = GraphInstance::from_node(&graph.root);
    driver.graph = graph;
    for _ in 0..3 {
        driver.step(Some(&setup));
    }
    assert!(driver.sync.groups().is_empty(), "nothing in the group plays yet");
    driver.params.set_bool("Go", true);
    driver.step(Some(&setup));
    let leader = driver
        .sync
        .groups()
        .first()
        .and_then(|g| g.leader.clone())
        .expect("the run clip leads its group");
    assert_eq!(leader.clip, "run");
    assert!(
        (leader.time - DT).abs() < 1e-5,
        "the leader is the clip's real time {DT}, not the probe's: {}",
        leader.time
    );
}
