// SPDX-License-Identifier: MPL-2.0
//! The graph's iterative-IK nodes (`.animgraph.ron` v4, ADR-8475): `ChainIK` (FABRIK or CCD over
//! any chain) and `BodyAim` (a turn spread over a joint stack), authored in data.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::chain_ik::{ChainSolver, JointLimit, swing_twist};
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    ANIMGRAPH_FORMAT_VERSION, AnimGraph, AnimGraphError, AnimNode, BodyAimNode, ChainIKNode, ChainLimits,
    ClipNode, IkTarget, NamedJointLimit,
};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::Entity;

/// `hips` at 0.5 m, then a tail `t0..t5` hanging along +Y in 0.2 m segments (1 m of reach).
fn tail_rig() -> Skeleton {
    let mut names = vec!["hips".to_string()];
    let mut parents = vec![None];
    let mut binds = vec![LocalBind {
        translation: Vec3::new(0.0, 0.5, 0.0),
        ..LocalBind::default()
    }];
    for i in 0..6 {
        names.push(format!("t{i}"));
        parents.push(Some(i as u32));
        binds.push(LocalBind {
            translation: if i == 0 {
                Vec3::new(0.0, 0.1, 0.0)
            } else {
                Vec3::new(0.0, 0.2, 0.0)
            },
            ..LocalBind::default()
        });
    }
    Skeleton::from_local_bind(names, parents, binds).expect("tail skeleton")
}

fn idle() -> Box<AnimNode> {
    Box::new(AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    }))
}

fn chain_node(solver: ChainSolver, limits: Option<ChainLimits>) -> AnimGraph {
    AnimGraph {
        parameters: vec![ParamDecl("Target".into(), ParamValue::Vec3(Vec3::ZERO))],
        root: AnimNode::ChainIK(ChainIKNode {
            inner: idle(),
            root: "t0".into(),
            tip: "t5".into(),
            solver,
            target: IkTarget::Param("Target".into()),
            pole: None,
            limits,
            iterations: 128,
            tolerance: 1e-4,
            weight_param: None,
        }),
    }
}

/// Plays `graph` on `skeleton` one fixed step per call, keeping instance and reuse buffers.
struct Runner<'a> {
    skeleton: &'a Skeleton,
    graph: &'a AnimGraph,
    clips: ClipLibrary,
    instance: GraphInstance,
    reuse: EvalReuse,
    scratch: Vec<Pose>,
    sockets: HashMap<(Entity, String), Vec3>,
    pose: Pose,
}

impl<'a> Runner<'a> {
    fn new(skeleton: &'a Skeleton, graph: &'a AnimGraph) -> Self {
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        Runner {
            skeleton,
            graph,
            clips,
            instance: GraphInstance::from_node(&graph.root),
            reuse: EvalReuse::default(),
            scratch: Vec::new(),
            sockets: HashMap::new(),
            pose: Pose::from_identity(skeleton.len()),
        }
    }

    fn step(&mut self, params: &ParameterSet) {
        let mut ctx = EvalContext::new(
            self.skeleton,
            &self.clips,
            params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &self.sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_reuse(std::mem::take(&mut self.reuse));
        evaluate_graph(self.graph, &mut self.instance, &mut ctx, &mut self.pose).expect("evaluate");
        self.reuse = ctx.take_reuse();
        self.scratch = ctx.finish().scratch;
    }

    fn global(&self, joint: &str) -> Mat4 {
        let index = self.skeleton.joint_index(joint).expect("joint") as usize;
        self.skeleton.globals(&self.pose, None)[index]
    }
}

/// A target moving on a tilted circle inside the tail's reach, off the tail's rest line.
fn moving_target(frame: usize) -> Vec3 {
    let a = frame as f32 * 0.05;
    Vec3::new(0.35 * a.cos(), 0.95 + 0.15 * (a * 0.7).sin(), 0.35 * a.sin())
}

#[test]
fn chain_ik_node_reaches_a_moving_target_within_1mm() {
    let skeleton = tail_rig();
    for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
        let graph = chain_node(solver, None);
        graph.validate().expect("valid graph");
        let mut runner = Runner::new(&skeleton, &graph);
        let mut params = graph.default_parameters();
        let mut worst = 0.0f32;
        let mut bits = Vec::new();
        for frame in 0..200 {
            let target = moving_target(frame);
            params.set_vec3("Target", target);
            runner.step(&params);
            let tip = runner.global("t5").w_axis.truncate();
            worst = worst.max(tip.distance(target));
            let residual = runner.reuse.ik_node_residuals().chain_error;
            assert!(
                (residual - tip.distance(target)).abs() < 1e-4,
                "{solver:?}: reported residual {residual} vs measured {}",
                tip.distance(target)
            );
            bits.extend(
                runner
                    .pose
                    .rotations
                    .iter()
                    .map(|q| q.to_array().map(f32::to_bits)),
            );
        }
        assert!(
            worst <= 1e-3,
            "{solver:?}: worst tip error {worst} m over 200 frames"
        );
        // The same inputs give the same bits (presentation code, one target).
        let mut again = Runner::new(&skeleton, &graph);
        let mut replay = Vec::new();
        for frame in 0..200 {
            params.set_vec3("Target", moving_target(frame));
            again.step(&params);
            replay.extend(
                again
                    .pose
                    .rotations
                    .iter()
                    .map(|q| q.to_array().map(f32::to_bits)),
            );
        }
        assert_eq!(bits, replay, "{solver:?}: a replay differs");
    }
}

/// Swing and twist of `joint`'s local rotation away from its bind rotation, about its bone.
fn swing_and_twist(skeleton: &Skeleton, pose: &Pose, joint: usize) -> (f32, f32) {
    let child = skeleton
        .parents
        .iter()
        .position(|p| *p == Some(joint as u32))
        .expect("a child");
    let axis = skeleton.bind_pose[child].translation.normalize();
    let offset = (skeleton.bind_pose[joint].rotation.inverse() * pose.rotations[joint]).normalize();
    let (swing, twist) = swing_twist(offset, axis);
    let swing = if swing.w < 0.0 { -swing } else { swing };
    let swing_angle = 2.0 * swing.w.clamp(-1.0, 1.0).acos();
    let twist_sign = Vec3::new(twist.x, twist.y, twist.z).dot(axis);
    let twist_angle = 2.0 * twist_sign.atan2(twist.w);
    let twist_angle = if twist_angle > std::f32::consts::PI {
        twist_angle - std::f32::consts::TAU
    } else if twist_angle < -std::f32::consts::PI {
        twist_angle + std::f32::consts::TAU
    } else {
        twist_angle
    };
    (swing_angle, twist_angle)
}

/// A limit set, with the swing bound it puts on each joint name.
type LimitCase = (&'static str, ChainLimits, fn(&str) -> f32);

#[test]
fn limited_chain_respects_its_cones() {
    let skeleton = tail_rig();
    let wide = JointLimit::Cone {
        max_swing: 0.35,
        min_twist: -0.1,
        max_twist: 0.1,
    };
    let tight = JointLimit::Cone {
        max_swing: 0.12,
        min_twist: -0.05,
        max_twist: 0.05,
    };
    let cases: [LimitCase; 2] = [
        ("uniform", ChainLimits::Uniform(wide), |_| 0.35),
        (
            "per joint",
            ChainLimits::PerJoint(vec![
                NamedJointLimit {
                    joint: "t0".into(),
                    limit: wide,
                },
                NamedJointLimit {
                    joint: "t2".into(),
                    limit: tight,
                },
                NamedJointLimit {
                    joint: "t3".into(),
                    limit: wide,
                },
            ]),
            |name| match name {
                "t0" | "t3" => 0.35,
                "t2" => 0.12,
                _ => f32::INFINITY,
            },
        ),
    ];
    for (label, limits, max_swing_of) in cases {
        for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
            let graph = chain_node(solver, Some(limits.clone()));
            let mut runner = Runner::new(&skeleton, &graph);
            let mut params = graph.default_parameters();
            let mut moved = 0.0f32;
            for frame in 0..200 {
                // Targets far to the side: the free solve would bend past every cone.
                let a = frame as f32 * 0.07;
                params.set_vec3("Target", Vec3::new(0.9 * a.cos(), 0.6, 0.9 * a.sin()));
                runner.step(&params);
                for i in 0..5 {
                    let name = format!("t{i}");
                    let joint = skeleton.joint_index(&name).expect("joint") as usize;
                    let (swing, twist) = swing_and_twist(&skeleton, &runner.pose, joint);
                    moved = moved.max(swing);
                    let limit = max_swing_of(&name);
                    assert!(
                        swing <= limit + 2e-3,
                        "{label} {solver:?} frame {frame}: {name} swings {swing} rad past {limit}"
                    );
                    if limit.is_finite() {
                        let twist_limit = if limit < 0.2 { 0.05 } else { 0.1 };
                        assert!(
                            twist.abs() <= twist_limit + 2e-3,
                            "{label} {solver:?} frame {frame}: {name} twists {twist} rad"
                        );
                    }
                }
            }
            assert!(moved > 0.1, "{label} {solver:?}: the chain never moved");
        }
    }
}

/// A turret: `base`, a yaw ring 0.5 m up, a pitch cradle 0.3 m above it, and a barrel muzzle
/// 1.2 m forward of the cradle and offset 0.3 m right and 0.1 m up (the parallax case).
fn turret_rig() -> Skeleton {
    let names = ["base", "yaw", "pitch", "muzzle"].map(String::from).to_vec();
    let parents = vec![None, Some(0), Some(1), Some(2)];
    let binds = [
        Vec3::ZERO,
        Vec3::new(0.0, 0.5, 0.0),
        Vec3::new(0.0, 0.3, 0.0),
        Vec3::new(0.3, 0.1, -1.2),
    ]
    .map(|translation| LocalBind {
        translation,
        ..LocalBind::default()
    })
    .to_vec();
    Skeleton::from_local_bind(names, parents, binds).expect("turret skeleton")
}

fn aim_graph(passes: u32) -> AnimGraph {
    AnimGraph {
        parameters: vec![ParamDecl("Aim".into(), ParamValue::Vec3(Vec3::ZERO))],
        root: AnimNode::BodyAim(BodyAimNode {
            inner: idle(),
            joints: vec![("yaw".into(), 1.0), ("pitch".into(), 1.0)],
            aim_joint: "muzzle".into(),
            axis: Vec3::NEG_Z,
            target: IkTarget::Param("Aim".into()),
            limits: None,
            passes,
            weight_param: None,
        }),
    }
}

/// Distance from `target` to the ray from `origin` along `dir` (unit).
fn miss(origin: Vec3, dir: Vec3, target: Vec3) -> f32 {
    let to = target - origin;
    (to - dir * to.dot(dir)).length()
}

#[test]
fn body_aim_node_points_its_axis_within_1mm_at_10m() {
    let skeleton = turret_rig();
    let graph = aim_graph(8);
    graph.validate().expect("valid graph");
    let mut runner = Runner::new(&skeleton, &graph);
    let mut params = graph.default_parameters();
    let mut worst = 0.0f32;
    for k in 0..24 {
        let yaw = k as f32 * 0.26 - 3.0;
        let elevation = ((k % 7) as f32 - 3.0) * 0.18;
        let dir = Vec3::new(
            elevation.cos() * yaw.sin(),
            elevation.sin(),
            -elevation.cos() * yaw.cos(),
        );
        let muzzle_rest = Vec3::new(0.3, 0.9, -1.2);
        let target = muzzle_rest + dir * 10.0;
        params.set_vec3("Aim", target);
        runner.step(&params);
        let muzzle = runner.global("muzzle");
        let (_, rotation, origin) = muzzle.to_scale_rotation_translation();
        let axis = (rotation * Vec3::NEG_Z).normalize();
        let off = miss(origin, axis, target);
        assert!(axis.dot(target - origin) > 0.0, "target {k} is behind the muzzle");
        worst = worst.max(off);
    }
    assert!(worst <= 1e-3, "the aim ray misses a 10 m target by {worst} m");
}

#[test]
fn body_aim_with_one_pass_keeps_the_parallax_more_passes_remove() {
    // The pass count is data: one pass leaves the offset muzzle's parallax, the default eight remove it.
    let skeleton = turret_rig();
    let target = Vec3::new(6.0, 2.0, -8.0);
    let mut misses = Vec::new();
    for passes in [1, 8] {
        let graph = aim_graph(passes);
        let mut runner = Runner::new(&skeleton, &graph);
        let mut params = graph.default_parameters();
        params.set_vec3("Aim", target);
        runner.step(&params);
        let (_, rotation, origin) = runner.global("muzzle").to_scale_rotation_translation();
        misses.push(miss(origin, (rotation * Vec3::NEG_Z).normalize(), target));
    }
    assert!(misses[0] > 1e-2, "one pass left only {} m", misses[0]);
    assert!(misses[1] < 1e-3, "eight passes left {} m", misses[1]);
}

#[test]
fn chain_ik_pole_turns_the_bend_and_keeps_the_tip() {
    let skeleton = tail_rig();
    let target = Vec3::new(0.0, 1.1, 0.25);
    let mut bends = Vec::new();
    for pole in [Vec3::new(2.0, 1.0, 0.0), Vec3::new(-2.0, 1.0, 0.0)] {
        let mut graph = chain_node(ChainSolver::Fabrik, None);
        graph
            .parameters
            .push(ParamDecl("Pole".into(), ParamValue::Vec3(Vec3::ZERO)));
        if let AnimNode::ChainIK(node) = &mut graph.root {
            node.pole = Some("Pole".into());
        }
        graph.validate().expect("valid graph");
        let mut runner = Runner::new(&skeleton, &graph);
        let mut params = graph.default_parameters();
        params.set_vec3("Target", target);
        params.set_vec3("Pole", pole);
        runner.step(&params);
        let tip = runner.global("t5").w_axis.truncate();
        assert!(
            tip.distance(target) <= 1e-3,
            "pole {pole}: tip off by {}",
            tip.distance(target)
        );
        bends.push(runner.global("t2").w_axis.x);
    }
    assert!(
        bends[0] > 0.02 && bends[1] < -0.02,
        "the bend did not follow the pole: {bends:?}"
    );
}

#[test]
fn chain_ik_node_weight_zero_returns_the_input_bits() {
    let skeleton = tail_rig();
    let mut graph = chain_node(ChainSolver::Ccd, None);
    graph
        .parameters
        .push(ParamDecl("W".into(), ParamValue::Float(0.0)));
    if let AnimNode::ChainIK(node) = &mut graph.root {
        node.weight_param = Some("W".into());
    }
    let mut runner = Runner::new(&skeleton, &graph);
    let mut params = graph.default_parameters();
    params.set_vec3("Target", Vec3::new(0.4, 0.9, 0.1));
    runner.step(&params);
    let bind = skeleton.bind_pose_pose();
    assert_eq!(runner.pose.rotations, bind.rotations);
}

#[test]
fn unknown_chain_joint_is_a_missing_joint_error_like_two_bone() {
    let skeleton = tail_rig();
    let mut graph = chain_node(ChainSolver::Fabrik, None);
    if let AnimNode::ChainIK(node) = &mut graph.root {
        node.tip = "nope".into();
    }
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let params = graph.default_parameters();
    let sockets = HashMap::new();
    let mut ctx = EvalContext::new(
        &skeleton,
        &clips,
        &params,
        1.0 / 60.0,
        Entity::PLACEHOLDER,
        &sockets,
        Vec::new(),
    );
    let mut instance = GraphInstance::from_node(&graph.root);
    let mut pose = Pose::from_identity(skeleton.len());
    let error = evaluate_graph(&graph, &mut instance, &mut ctx, &mut pose).expect_err("unknown joint");
    assert!(error.to_string().contains("nope"), "{error}");
}

#[test]
fn a_reversed_chain_leaves_the_pose_alone() {
    // `t5` is not above `t0`: the node warns once and passes the pose through.
    let skeleton = tail_rig();
    let mut graph = chain_node(ChainSolver::Fabrik, None);
    if let AnimNode::ChainIK(node) = &mut graph.root {
        node.root = "t5".into();
        node.tip = "t0".into();
    }
    let mut runner = Runner::new(&skeleton, &graph);
    let mut params = graph.default_parameters();
    params.set_vec3("Target", Vec3::new(0.4, 0.9, 0.1));
    runner.step(&params);
    runner.step(&params);
    assert_eq!(runner.pose.rotations, skeleton.bind_pose_pose().rotations);
}

/// Every `.animgraph.ron` in the repository written at v3 (or older).
fn old_graphs() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let files = [
        "crates/regia_agent/tests/fixtures/format-corpus/animgraph/v1.animgraph.ron",
        "crates/regia_agent/tests/fixtures/format-corpus/animgraph/v2.animgraph.ron",
        "crates/regia_agent/tests/fixtures/format-corpus/animgraph/v3.ron",
        "crates/regia_agent/src/fuzz_targets/fixtures/locomotion.animgraph.ron",
        "crates/regia_animation/tests/fixtures/locomotion.animgraph.ron",
        "docs/formats/locomotion.animgraph.ron",
        "samples/anim_lab/assets/reach.animgraph.ron",
        "samples/anim_lab/assets/turret.animgraph.ron",
    ];
    files
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(root.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
            (f.to_string(), text)
        })
        .collect()
}

#[test]
fn v3_graph_loads_unchanged_as_v4() {
    assert_eq!(ANIMGRAPH_FORMAT_VERSION, 4);
    let mut v3 = 0;
    for (name, text) in old_graphs() {
        let version = regia_core::version::peek_ron_version(&text).expect("version");
        assert!(
            version <= 3,
            "{name} is at v{version}; this test covers old files"
        );
        if version == 3 {
            v3 += 1;
        }
        let graph = AnimGraph::from_ron_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        // The body is read as written: the v3 document and the loaded v4 graph hold the same tree.
        let document: regia_animation::graph::AnimGraphDocument =
            ron::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let expected = AnimGraph {
            parameters: document.parameters,
            root: document.root,
        }
        .triangulated();
        assert_eq!(graph, expected, "{name}: the migration changed the tree");
        let written = graph.to_ron_string().expect("write");
        assert!(
            written.contains("format_version: 4"),
            "{name}: written at {written:.40}"
        );
        assert_eq!(
            AnimGraph::from_ron_str(&written).expect("reload"),
            graph,
            "{name}: the v4 file does not round-trip"
        );
    }
    assert!(v3 >= 1, "no v3 fixture was checked");
}

#[test]
fn v5_graph_is_refused() {
    let (name, text) = old_graphs()
        .into_iter()
        .find(|(_, t)| regia_core::version::peek_ron_version(t).ok() == Some(3))
        .expect("a v3 fixture");
    let future = text.replacen("format_version: 3", "format_version: 5", 1);
    match AnimGraph::from_ron_str(&future) {
        Err(AnimGraphError::UnknownVersion(5)) => {}
        other => panic!("{name} patched to v5: {other:?}"),
    }
    // v4 with the new nodes loads and round-trips.
    let graph = chain_node(ChainSolver::Ccd, Some(ChainLimits::Uniform(JointLimit::Free)));
    let text = graph.to_ron_string().expect("write");
    assert_eq!(AnimGraph::from_ron_str(&text).expect("load v4"), graph);
}

/// A label and an edit that makes a valid graph invalid.
type Spoil = (&'static str, fn(&mut AnimNode));

#[test]
fn v4_validation_refuses_bad_ik_numbers() {
    let cases: [Spoil; 6] = [
        ("iterations", |n| {
            if let AnimNode::ChainIK(c) = n {
                c.iterations = 100_000;
            }
        }),
        ("tolerance", |n| {
            if let AnimNode::ChainIK(c) = n {
                c.tolerance = f32::NAN;
            }
        }),
        ("cone", |n| {
            if let AnimNode::ChainIK(c) = n {
                c.limits = Some(ChainLimits::Uniform(JointLimit::Cone {
                    max_swing: f32::INFINITY,
                    min_twist: 0.0,
                    max_twist: 0.0,
                }));
            }
        }),
        ("aim axis", |n| {
            if let AnimNode::BodyAim(a) = n {
                a.axis = Vec3::ZERO;
            }
        }),
        ("share", |n| {
            if let AnimNode::BodyAim(a) = n {
                a.joints[0].1 = -1.0;
            }
        }),
        ("passes", |n| {
            if let AnimNode::BodyAim(a) = n {
                a.passes = 0;
            }
        }),
    ];
    for (label, spoil) in cases {
        let mut graph = if label.starts_with("aim") || label == "share" || label == "passes" {
            aim_graph(4)
        } else {
            chain_node(ChainSolver::Fabrik, None)
        };
        graph.validate().expect("the unspoiled graph is valid");
        spoil(&mut graph.root);
        assert!(graph.validate().is_err(), "{label}: a bad value was accepted");
    }
}

#[test]
fn chain_ik_rotations_stay_unit_quaternions() {
    let skeleton = tail_rig();
    let graph = chain_node(ChainSolver::Fabrik, None);
    let mut runner = Runner::new(&skeleton, &graph);
    let mut params = graph.default_parameters();
    params.set_vec3("Target", Vec3::new(5.0, 5.0, 5.0));
    runner.step(&params);
    for q in &runner.pose.rotations {
        assert!(q.is_finite() && (q.length() - 1.0).abs() < 1e-4, "{q:?}");
    }
}

#[test]
fn published_chain_ik_example_loads_and_round_trips() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats/chain_ik.animgraph.ron");
    let text = std::fs::read_to_string(&path).expect("example");
    let graph = AnimGraph::from_ron_str(&text).expect("the v4 example loads");
    let AnimNode::BodyAim(aim) = &graph.root else {
        panic!("body aim root");
    };
    assert_eq!(aim.joints.len(), 4);
    let AnimNode::ChainIK(chain) = aim.inner.as_ref() else {
        panic!("chain IK inner");
    };
    assert!(matches!(chain.limits, Some(ChainLimits::PerJoint(ref l)) if l.len() == 2));
    let written = graph.to_ron_string().expect("write");
    assert_eq!(AnimGraph::from_ron_str(&written).expect("reload"), graph);
}
