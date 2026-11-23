// SPDX-License-Identifier: MPL-2.0
//! Review lane anim-chain-ik-nodes-review-c: edge cases of the `.animgraph.ron` v4 `ChainIK` and
//! `BodyAim` nodes (ADR-8475). Tests named `finding_*` fail at the reviewed head 0fd207bd5 and
//! pass once the fix in the review report lands; the other tests pin edge behavior that already
//! holds (chain of one, zero-length links, non-finite and huge inputs, frame-rate independence,
//! the iteration cap).

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::chain_ik::{ChainSolver, JointLimit};
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    AnimGraph, AnimNode, BodyAimNode, ChainIKNode, ChainLimits, ClipNode, IkTarget, MAX_CHAIN_IK_ITERATIONS,
    animgraph_schema_fingerprint,
};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// The published v4 example (`docs/formats/chain_ik.animgraph.ron`).
const EXAMPLE: &str = include_str!("../../../docs/formats/chain_ik.animgraph.ron");

/// `hips`, a tail `t0..t5` (0.2 m links) and a neck `n0..n2` with a `head`.
fn rig_with(link: impl Fn(u32) -> Vec3) -> Skeleton {
    let mut names = vec!["hips".to_string()];
    let mut parents = vec![None];
    let mut binds = vec![Vec3::new(0.0, 0.5, 0.0)];
    for i in 0..6u32 {
        names.push(format!("t{i}"));
        parents.push(Some(i));
        binds.push(link(i));
    }
    for i in 0..3u32 {
        names.push(format!("n{i}"));
        parents.push(Some(if i == 0 { 0 } else { 6 + i }));
        binds.push(Vec3::new(0.0, 0.2, 0.0));
    }
    names.push("head".into());
    parents.push(Some(9));
    binds.push(Vec3::new(0.0, 0.1, -0.1));
    let binds = binds
        .into_iter()
        .map(|translation| LocalBind {
            translation,
            ..LocalBind::default()
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

fn rig() -> Skeleton {
    rig_with(|_| Vec3::new(0.0, 0.2, 0.0))
}

fn clip() -> Box<AnimNode> {
    Box::new(AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    }))
}

fn params() -> Vec<ParamDecl> {
    vec![
        ParamDecl("Tail".into(), ParamValue::Vec3(Vec3::new(0.3, 1.2, 0.3))),
        ParamDecl("Pole".into(), ParamValue::Vec3(Vec3::X)),
        ParamDecl("Look".into(), ParamValue::Vec3(Vec3::new(1.0, 1.5, -6.0))),
        ParamDecl("W".into(), ParamValue::Float(1.0)),
    ]
}

fn chain(inner: Box<AnimNode>, root: &str, tip: &str, solver: ChainSolver) -> AnimNode {
    AnimNode::ChainIK(ChainIKNode {
        inner,
        root: root.into(),
        tip: tip.into(),
        solver,
        target: IkTarget::Param("Tail".into()),
        pole: Some("Pole".into()),
        limits: Some(ChainLimits::Uniform(JointLimit::Cone {
            max_swing: 0.6,
            min_twist: -0.2,
            max_twist: 0.2,
        })),
        iterations: 24,
        tolerance: 1e-3,
        weight_param: Some("W".into()),
    })
}

fn aim(inner: Box<AnimNode>, joints: Vec<(String, f32)>) -> AnimNode {
    AnimNode::BodyAim(BodyAimNode {
        inner,
        joints,
        aim_joint: "head".into(),
        axis: Vec3::NEG_Z,
        target: IkTarget::Param("Look".into()),
        limits: None,
        passes: 4,
        weight_param: Some("W".into()),
    })
}

fn both(solver: ChainSolver) -> AnimGraph {
    AnimGraph {
        parameters: params(),
        root: aim(
            Box::new(chain(clip(), "t0", "t5", solver)),
            vec![("n0".into(), 1.0), ("n1".into(), 1.0), ("n2".into(), 2.0)],
        ),
    }
}

/// Plays a graph one step per call with kept instance and reuse buffers.
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

    fn step(&mut self, params: &ParameterSet, dt: f32) {
        let mut ctx = EvalContext::new(
            self.skeleton,
            &self.clips,
            params,
            dt,
            Entity::PLACEHOLDER,
            &self.sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_reuse(std::mem::take(&mut self.reuse));
        evaluate_graph(self.graph, &mut self.instance, &mut ctx, &mut self.pose).expect("evaluate");
        self.reuse = ctx.take_reuse();
        self.scratch = ctx.finish().scratch;
    }

    fn assert_finite(&self, what: &str) {
        for (j, q) in self.pose.rotations.iter().enumerate() {
            assert!(q.is_finite(), "{what}: joint {j} rotation {q:?} is not finite");
            assert!(
                (q.length() - 1.0).abs() < 1e-3,
                "{what}: joint {j} rotation {q:?} is not unit"
            );
        }
        for (j, t) in self.pose.translations.iter().enumerate() {
            assert!(t.is_finite(), "{what}: joint {j} translation {t:?} is not finite");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Findings (fail at 0fd207bd5)
// ---------------------------------------------------------------------------------------------

/// Finding 1 (format freeze): `JointLimit` is a struct-variant enum (`Cone { max_swing,
/// min_twist, max_twist }`, `Hinge { axis, min, max }`) that is now part of the frozen v4 wire
/// format, but the fingerprint lists only its variant names. Renaming `max_swing` or adding a
/// hinge field would change every v4 file's shape without moving the fingerprint, so
/// `cargo xtask check-formats` would not ask for a version bump (ADR-2316 added
/// `serde_variant_shapes` for exactly this).
#[test]
fn finding_fingerprint_covers_joint_limit_field_names() {
    let fingerprint = animgraph_schema_fingerprint();
    for field in ["max_swing", "min_twist", "max_twist"] {
        assert!(
            fingerprint.contains(field),
            "the animgraph fingerprint does not list JointLimit::Cone's field {field:?}: {fingerprint}"
        );
    }
}

/// Finding 2 (format freeze): a misspelled field of a v4 IK node loads silently with the
/// default. `iteration: 200` (for `iterations`) runs 24 iterations and `pases: 32` runs 8; the
/// author gets no error. The node types are new in v4 (no file in the wild yet), so refusing
/// unknown fields costs nothing now and is a format change after the freeze.
#[test]
fn finding_misspelled_v4_ik_fields_are_refused() {
    for (right, typo) in [
        ("iterations: 24", "iteration: 200"),
        ("passes: 4", "pases: 32"),
        ("tolerance: 0.001", "tolerence: 0.5"),
    ] {
        assert!(EXAMPLE.contains(right), "the example still has {right:?}");
        let text = EXAMPLE.replacen(right, typo, 1);
        match AnimGraph::from_ron_str(&text) {
            Err(_) => {}
            Ok(graph) => panic!(
                "a v4 graph with the misspelled field {typo:?} loaded (the value is silently the default): {:?}",
                summary(&graph.root)
            ),
        }
    }
}

fn summary(node: &AnimNode) -> String {
    match node {
        AnimNode::BodyAim(n) => format!("BodyAim passes {} / {}", n.passes, summary(&n.inner)),
        AnimNode::ChainIK(n) => format!("ChainIK iterations {} tolerance {}", n.iterations, n.tolerance),
        _ => "other".into(),
    }
}

/// Finding 3: a `BodyAim` joint with an empty name passes validation (the `ChainIK` root and tip
/// and every per-joint limit name are checked, the turning joints are not). The editor's "Add
/// joint" button inserts exactly such an entry, the editor's save validates and writes it, and at
/// runtime every evaluation of the graph fails with `MissingJoint("")`.
#[test]
fn finding_body_aim_with_an_empty_joint_name_is_refused() {
    let graph = AnimGraph {
        parameters: params(),
        root: aim(clip(), vec![("n0".into(), 1.0), (String::new(), 1.0)]),
    };
    let text = graph.to_ron_string().expect("write");
    assert!(
        AnimGraph::from_ron_str(&text).is_err(),
        "a body aim node with an empty joint name loaded; at runtime it fails every frame with MissingJoint(\"\")"
    );
}

/// Allocations over `frames` warm frames of a graph with `nodes` nested `ChainIK` nodes.
fn warm_allocations(nodes: usize, frames: usize) -> u64 {
    let skeleton = rig();
    let mut root = clip();
    for _ in 0..nodes {
        root = Box::new(chain(root, "t0", "t5", ChainSolver::Fabrik));
    }
    let graph = AnimGraph {
        parameters: params(),
        root: *root,
    };
    graph.validate().expect("valid");
    let mut runner = Runner::new(&skeleton, &graph);
    let params = graph.default_parameters();
    for _ in 0..4 {
        runner.step(&params, 1.0 / 60.0);
    }
    let ((), stats) = alloc_counter::measure(|| {
        for _ in 0..frames {
            runner.step(&params, 1.0 / 60.0);
        }
    });
    stats.allocs
}

/// Control for finding 4: 64 chain nodes on one rig resolve once and then allocate no more than
/// the plain clip graph.
#[test]
fn sixty_four_chain_nodes_stay_allocation_free_when_warm() {
    let plain = warm_allocations(0, 20);
    assert_eq!(warm_allocations(64, 20), plain);
}

/// Finding 4 (hazard): the rig's IK-node cache holds 64 chains (`MAX_SLOTS`) and is cleared
/// when full. With 65 chain nodes on one rig (or 64 plus stale entries after graph reloads) the
/// cache never warms: every frame clears it and resolves every chain again (a `JointChain`
/// `Vec`, cloned names and limits per node per frame), against the "a warm rig allocates
/// nothing per frame" contract of `ik_nodes.rs`.
#[test]
#[ignore = "known issue for v0.6.1 (review finding 4, ADR-8732): the IK name cache clears at 64 slots; remove the ignore with the fix"]
fn finding_sixty_five_chain_nodes_stay_allocation_free_when_warm() {
    let plain = warm_allocations(0, 20);
    let with_nodes = warm_allocations(65, 20);
    assert_eq!(
        with_nodes,
        plain,
        "65 chain IK nodes added {} allocations over 20 warm frames",
        with_nodes as i64 - plain as i64
    );
}

// ---------------------------------------------------------------------------------------------
// Edge behavior that holds at 0fd207bd5
// ---------------------------------------------------------------------------------------------

/// A chain of one (root == tip) and a tip above its root pass the pose untouched, both solvers.
#[test]
fn chain_of_one_and_reversed_chain_pass_the_pose() {
    let skeleton = rig();
    let plain = AnimGraph {
        parameters: params(),
        root: *clip(),
    };
    let mut reference = Runner::new(&skeleton, &plain);
    let p = plain.default_parameters();
    reference.step(&p, 1.0 / 60.0);
    for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
        for (root, tip) in [("t2", "t2"), ("t5", "t0")] {
            let graph = AnimGraph {
                parameters: params(),
                root: chain(clip(), root, tip, solver),
            };
            graph.validate().expect("valid");
            let mut runner = Runner::new(&skeleton, &graph);
            runner.step(&p, 1.0 / 60.0);
            assert_eq!(
                runner.pose.rotations, reference.pose.rotations,
                "{solver:?} {root}->{tip} changed the pose"
            );
        }
    }
}

/// Links of zero length (every link, or one in the middle) never put NaN in the pose.
#[test]
fn zero_length_links_keep_the_pose_finite() {
    let link = |zero: u32| {
        move |i: u32| {
            if i == zero || zero == 99 {
                Vec3::ZERO
            } else {
                Vec3::new(0.0, 0.2, 0.0)
            }
        }
    };
    for (what, skeleton) in [
        ("every link zero", rig_with(link(99))),
        ("t2 link zero", rig_with(link(2))),
        ("tip link zero", rig_with(link(5))),
        ("root link zero", rig_with(link(0))),
    ] {
        for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
            let graph = both(solver);
            let mut runner = Runner::new(&skeleton, &graph);
            let mut p = graph.default_parameters();
            for frame in 0..30 {
                let a = frame as f32 * 0.3;
                p.set_vec3("Tail", Vec3::new(a.cos(), 0.5 + a.sin(), 0.2));
                runner.step(&p, 1.0 / 60.0);
                runner.assert_finite(&format!("{what} {solver:?} frame {frame}"));
            }
        }
    }
}

/// NaN, infinite and huge targets, poles and weights (parameters set by a game or the agent)
/// never reach the pose as NaN, and the residual stays a number (or infinity), never NaN.
#[test]
fn non_finite_and_huge_inputs_never_reach_the_pose() {
    let skeleton = rig();
    let bad = [
        Vec3::splat(f32::NAN),
        Vec3::new(f32::INFINITY, 0.0, 0.0),
        Vec3::splat(f32::MAX),
        Vec3::splat(-f32::MAX),
        Vec3::new(1e30, -1e30, 1e30),
        Vec3::ZERO,
        Vec3::new(0.0, 0.5, 0.0), // on the chain root
    ];
    for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
        let graph = both(solver);
        let mut runner = Runner::new(&skeleton, &graph);
        let mut p = graph.default_parameters();
        for (i, &v) in bad.iter().enumerate() {
            for weight in [1.0, 0.5, f32::NAN, f32::INFINITY, -1.0] {
                p.set_vec3("Tail", v);
                p.set_vec3("Look", v);
                p.set_vec3("Pole", bad[(i + 1) % bad.len()]);
                p.set_float("W", weight);
                runner.step(&p, 1.0 / 60.0);
                runner.assert_finite(&format!("{solver:?} input {v:?} weight {weight}"));
                let r = runner.reuse.ik_node_residuals();
                assert!(!r.chain_error.is_nan() && !r.aim_angle.is_nan(), "{r:?}");
            }
        }
    }
}

/// NaN and infinity written in a v4 file are refused at load, never evaluated.
#[test]
fn non_finite_numbers_in_a_v4_file_are_refused() {
    for (right, wrong) in [
        ("tolerance: 0.001", "tolerance: NaN"),
        ("tolerance: 0.001", "tolerance: inf"),
        ("tolerance: 0.001", "tolerance: -0.5"),
        ("axis: (0.0, 0.0, -1.0)", "axis: (NaN, 0.0, -1.0)"),
        ("axis: (0.0, 0.0, -1.0)", "axis: (0.0, 0.0, 0.0)"),
        ("max_swing: 0.6", "max_swing: NaN"),
        ("axis: (1.0, 0.0, 0.0)", "axis: (inf, 0.0, 0.0)"),
        ("(\"neck\", 0.5)", "(\"neck\", NaN)"),
        ("(\"neck\", 0.5)", "(\"neck\", -0.5)"),
        ("iterations: 24", "iterations: 257"),
        ("passes: 4", "passes: 0"),
        ("passes: 4", "passes: 33"),
    ] {
        assert!(EXAMPLE.contains(right), "the example has {right:?}");
        let text = EXAMPLE.replacen(right, wrong, 1);
        assert!(AnimGraph::from_ron_str(&text).is_err(), "{wrong:?} loaded");
    }
    AnimGraph::from_ron_str(EXAMPLE).expect("the example itself loads");
}

/// The IK nodes keep no state between frames: the same target at the same time gives the same
/// pose whether the rig runs at 30, 60 or 144 Hz (tolerance 1e-6 per quaternion component and
/// 1e-6 m on the residual; in practice bit-identical).
#[test]
fn ik_nodes_are_frame_rate_independent() {
    let skeleton = rig();
    for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
        let graph = both(solver);
        let mut at_half = Vec::new();
        for hz in [30u32, 60, 144] {
            let mut runner = Runner::new(&skeleton, &graph);
            let mut p = graph.default_parameters();
            let mut kept = None;
            for frame in 0..=hz {
                let t = f64::from(frame) / f64::from(hz);
                let a = (t * 3.0) as f32;
                p.set_vec3("Tail", Vec3::new(0.4 * a.cos(), 1.1, 0.4 * a.sin()));
                p.set_vec3("Look", Vec3::new(5.0 * a.sin(), 1.5, -6.0));
                runner.step(&p, 1.0 / hz as f32);
                if frame * 2 == hz {
                    kept = Some((runner.pose.rotations.clone(), runner.reuse.ik_node_residuals()));
                }
            }
            at_half.push((hz, kept.expect("t = 0.5 s sampled")));
        }
        let (_, (reference, residual)) = &at_half[0];
        for (hz, (rotations, r)) in &at_half[1..] {
            for (j, (a, b)) in reference.iter().zip(rotations).enumerate() {
                let d = a
                    .to_array()
                    .iter()
                    .zip(b.to_array())
                    .map(|(x, y)| (x - y).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    d <= 1e-6,
                    "{solver:?} {hz} Hz joint {j} differs from 30 Hz by {d}"
                );
            }
            assert!(
                (r.chain_error - residual.chain_error).abs() <= 1e-6,
                "{solver:?} {hz} Hz"
            );
        }
    }
}

/// The node clamps iterations to the cap and a target the limited chain cannot reach stops at
/// the cap (bounded work per frame), reporting how many it used.
#[test]
fn unreachable_target_stops_at_the_iteration_cap() {
    let skeleton = rig();
    for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
        let mut graph = both(solver);
        if let AnimNode::BodyAim(aim) = &mut graph.root
            && let AnimNode::ChainIK(chain) = aim.inner.as_mut()
        {
            chain.iterations = MAX_CHAIN_IK_ITERATIONS;
            chain.tolerance = 0.0;
        }
        graph.validate().expect("valid at the cap");
        let mut runner = Runner::new(&skeleton, &graph);
        let mut p = graph.default_parameters();
        // Behind the root, inside the reach: the 0.6 rad cones cannot fold the tail there.
        p.set_vec3("Tail", Vec3::new(0.0, 0.2, 0.1));
        runner.step(&p, 1.0 / 60.0);
        let r = runner.reuse.ik_node_residuals();
        assert!(r.chain_iterations <= MAX_CHAIN_IK_ITERATIONS, "{solver:?} {r:?}");
        assert!(
            r.chain_error.is_finite() && r.chain_error > 1e-3,
            "{solver:?} {r:?}"
        );
        runner.assert_finite(&format!("{solver:?} at the cap"));
    }
}

/// A three-joint neck rig; the head's bind local rotation is `head_roll`.
fn neck_rig(names: [&str; 3], head_roll: regia_core::math::Quat) -> Skeleton {
    let binds = vec![
        LocalBind {
            translation: Vec3::new(0.0, 1.0, 0.0),
            ..LocalBind::default()
        },
        LocalBind {
            translation: Vec3::new(0.0, 0.5, 0.0),
            ..LocalBind::default()
        },
        LocalBind {
            translation: Vec3::new(0.0, 0.2, 0.0),
            rotation: head_roll,
            ..LocalBind::default()
        },
    ];
    Skeleton::from_local_bind(
        names.iter().map(|n| (*n).to_string()).collect(),
        vec![None, Some(0), Some(1)],
        binds,
    )
    .expect("neck rig")
}

/// Finding 5 (hazard): `retarget_graph` keeps a `BodyAim` axis (and a hinge limit's axis) as
/// written, in the SOURCE joint's local frame. A target rig whose paired joint has another bone
/// roll (a 90° turn of the head's bind frame about its bone, common between DCC exports) then
/// aims a different anatomical direction: here the face ends up 90° off the target although the
/// retargeter pairs the heads. The axis should be carried through the paired bind frames
/// (`R_target_bind⁻¹ · R_source_bind · axis`), as the retargeter already does for rotations.
#[test]
#[ignore = "known issue for v0.6.1 (review finding 5, ADR-8732): retargeted aim/hinge axes stay in the source frame, needs a decision; remove the ignore with the fix"]
fn finding_retargeted_body_aim_axis_follows_the_target_rigs_bone_roll() {
    use regia_animation::ik_retarget::retarget_graph;
    use regia_animation::retarget::{RetargetProfile, Retargeter};
    use regia_core::math::Quat;

    let roll = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    let source = neck_rig(["hips", "neck", "head"], Quat::IDENTITY);
    let target = neck_rig(["Pelvis", "Neck_T", "Head_T"], roll);
    let profile = |names: [&str; 3]| {
        RetargetProfile::default()
            .with("Hips", names[0])
            .with("Neck", names[1])
            .with("Head", names[2])
    };
    let rt = Retargeter::new(
        &source,
        &profile(["hips", "neck", "head"]),
        &target,
        &profile(["Pelvis", "Neck_T", "Head_T"]),
    )
    .expect("retargeter");
    let look = Vec3::new(5.0, 1.7, -5.0);
    let graph = AnimGraph {
        parameters: vec![
            ParamDecl("Look".into(), ParamValue::Vec3(look)),
            ParamDecl("W".into(), ParamValue::Float(1.0)),
        ],
        root: AnimNode::BodyAim(BodyAimNode {
            inner: clip(),
            joints: vec![("neck".into(), 1.0), ("head".into(), 1.0)],
            aim_joint: "head".into(),
            axis: Vec3::NEG_Z,
            target: IkTarget::Param("Look".into()),
            limits: None,
            passes: 8,
            weight_param: None,
        }),
    };
    let moved = retarget_graph(&rt, &graph);
    let AnimNode::BodyAim(node) = &moved.root else {
        panic!("body aim root");
    };
    assert_eq!(node.aim_joint, "Head_T", "the joint names map");

    // The source face (head −Z) points at the target.
    let mut on_source = Runner::new(&source, &graph);
    let p = graph.default_parameters();
    on_source.step(&p, 1.0 / 60.0);
    let g = source.globals(&on_source.pose, None)[2];
    let (_, r, t) = g.to_scale_rotation_translation();
    let source_miss = (r * Vec3::NEG_Z).angle_between(look - t);
    assert!(source_miss < 0.01, "source face misses by {source_miss} rad");

    // The same face on the target rig is −Z of the source frame, i.e. roll⁻¹ · −Z in Head_T's
    // frame; after the retarget it should point at the target too.
    let mut on_target = Runner::new(&target, &moved);
    let p = moved.default_parameters();
    on_target.step(&p, 1.0 / 60.0);
    let g = target.globals(&on_target.pose, None)[2];
    let (_, r, t) = g.to_scale_rotation_translation();
    let face = r * (roll.inverse() * Vec3::NEG_Z);
    let target_miss = face.angle_between(look - t);
    assert!(
        target_miss < 0.02,
        "the retargeted body aim turns Head_T's own -Z at the target; the face (the source's -Z) misses by {target_miss} rad"
    );
}
