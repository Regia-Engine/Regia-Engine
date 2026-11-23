// SPDX-License-Identifier: MPL-2.0
//! Lane anim-chain-ik-followups-verification. Proofs against fix head 6142921d4
//! (ADR-8735, review-c findings 4–6).
//!
//! The ChainIK slot cache (`ik_nodes::IkNodeCache::chain_slot`) treats a slot as live when the
//! node and skeleton addresses match and the cached root and tip still carry those names. An
//! in-place parent edit keeps both, so the slot is stamped used every frame and never rebuilt.
//! `JointChain::valid` then rejects the stale list inside `solve_chain`, the pose stays at the
//! bind, and the node does not recover. A cold `EvalReuse` on the edited skeleton does solve.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::chain_ik::{
    ChainIkScratch, ChainIkSettings, ChainSolver, JointChain, JointLimit, solve_chain,
};
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, ChainIKNode, ClipNode, IkTarget};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::Entity;

const ROOT: u32 = 0;
const MID: u32 = 1;
const SIDE: u32 = 2;
const TIP: u32 = 3;

fn bind(translation: Vec3) -> LocalBind {
    LocalBind {
        translation,
        ..LocalBind::default()
    }
}

/// `root → mid → tip`, with `side` also under `root`. `tip`'s parent is `tip_parent`.
fn branched(tip_parent: u32) -> Skeleton {
    let names = vec!["root".into(), "mid".into(), "side".into(), "tip".into()];
    let parents = vec![None, Some(ROOT), Some(ROOT), Some(tip_parent)];
    let binds = vec![
        bind(Vec3::ZERO),
        bind(Vec3::new(0.0, 1.0, 0.0)),
        bind(Vec3::new(1.0, 0.0, 0.0)),
        bind(Vec3::new(0.0, 1.0, 0.0)),
    ];
    Skeleton::from_local_bind(names, parents, binds).expect("branched skeleton")
}

/// Two roots. `tip` starts with no parent, so it is not under `root` until the caller edits it.
fn two_roots() -> Skeleton {
    let names = vec!["root".into(), "tip".into()];
    let parents = vec![None, None];
    let binds = vec![bind(Vec3::ZERO), bind(Vec3::new(0.0, 1.0, 0.0))];
    Skeleton::from_local_bind(names, parents, binds).expect("two-root skeleton")
}

fn chain_graph(target: Vec3) -> AnimGraph {
    let graph = AnimGraph {
        parameters: vec![ParamDecl("Goal".into(), ParamValue::Vec3(target))],
        root: AnimNode::ChainIK(ChainIKNode {
            inner: Box::new(AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            })),
            root: "root".into(),
            tip: "tip".into(),
            solver: ChainSolver::Ccd,
            target: IkTarget::Param("Goal".into()),
            pole: None,
            limits: None,
            iterations: 8,
            tolerance: 1e-4,
            weight_param: None,
        }),
    };
    graph.validate().expect("chain graph");
    graph
}

struct Rig {
    clips: ClipLibrary,
    reuse: EvalReuse,
    scratch: Vec<Pose>,
    sockets: HashMap<(Entity, String), Vec3>,
    instance: GraphInstance,
    params: ParameterSet,
}

impl Rig {
    fn new(graph: &AnimGraph) -> Self {
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        Rig {
            clips,
            reuse: EvalReuse::default(),
            scratch: Vec::new(),
            sockets: HashMap::new(),
            instance: GraphInstance::from_node(&graph.root),
            params: graph.default_parameters(),
        }
    }

    fn eval(&mut self, skeleton: &Skeleton, graph: &AnimGraph) -> Pose {
        let mut pose = Pose::from_identity(skeleton.len());
        let mut ctx = EvalContext::new(
            skeleton,
            &self.clips,
            &self.params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &self.sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_reuse(std::mem::take(&mut self.reuse));
        evaluate_graph(graph, &mut self.instance, &mut ctx, &mut pose).expect("evaluate");
        self.reuse = ctx.take_reuse();
        self.scratch = ctx.finish().scratch;
        pose
    }
}

fn turn_from_bind(pose: &Pose, joint: usize) -> f32 {
    pose.rotations[joint].angle_between(Quat::IDENTITY).abs()
}

fn turned(pose: &Pose, joints: &[usize]) -> bool {
    joints.iter().any(|&joint| turn_from_bind(pose, joint) > 0.05)
}

fn turns_of(pose: &Pose) -> String {
    pose.rotations
        .iter()
        .enumerate()
        .map(|(joint, rotation)| format!("{joint}:{:.4}", rotation.angle_between(Quat::IDENTITY).abs()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A cold cache on the reparented skeleton bends `root` or `side`. The same graph, warmed while
/// `tip` still hung from `mid`, then reparented in place (`parents[tip] = side`), keeps serving
/// the chain `[root, mid, tip]`. That list fails `JointChain::valid`, so the warm evaluation
/// leaves the bind pose.
#[test]
fn warm_chain_slot_resolves_again_after_an_in_place_reparent() {
    let target = Vec3::new(0.4, 1.2, 0.0);
    let graph = chain_graph(target);

    let cold_skeleton = branched(SIDE);
    let mut cold = Rig::new(&graph);
    let cold_pose = cold.eval(&cold_skeleton, &graph);
    assert!(
        turned(&cold_pose, &[ROOT as usize, SIDE as usize]),
        "control: a cold cache must solve root→side→tip, turns {}",
        turns_of(&cold_pose)
    );

    let mut skeleton = branched(MID);
    let mut warm = Rig::new(&graph);
    let first = warm.eval(&skeleton, &graph);
    assert!(
        turned(&first, &[ROOT as usize, MID as usize]),
        "the chain must bend before the reparent, turns {}",
        turns_of(&first)
    );
    let second = warm.eval(&skeleton, &graph);
    assert!(
        turned(&second, &[ROOT as usize, MID as usize]),
        "a warm valid chain must still bend, turns {}",
        turns_of(&second)
    );

    skeleton.parents[TIP as usize] = Some(SIDE);
    let after = warm.eval(&skeleton, &graph);
    assert!(
        turned(&after, &[ROOT as usize, SIDE as usize]),
        "warm cache kept [root, mid, tip] after tip was reparented onto side; turns {}",
        turns_of(&after)
    );
}

/// A chain that cannot be built is cached as an empty joint list, and an empty list matches the
/// hit predicate on its own. Parenting `tip` under `root` afterwards makes `JointChain::between`
/// succeed, but the warm slot still no-ops.
#[test]
fn warm_chain_slot_resolves_again_after_the_tip_gains_a_parent() {
    let target = Vec3::new(0.6, 0.2, 0.0);
    let graph = chain_graph(target);

    let mut cold_skeleton = two_roots();
    cold_skeleton.parents[1] = Some(0);
    let mut cold = Rig::new(&graph);
    let cold_pose = cold.eval(&cold_skeleton, &graph);
    assert!(
        turned(&cold_pose, &[0]),
        "control: a cold cache must solve the two-joint chain, turns {}",
        turns_of(&cold_pose)
    );

    let mut skeleton = two_roots();
    let mut warm = Rig::new(&graph);
    let before = warm.eval(&skeleton, &graph);
    assert!(
        !turned(&before, &[0, 1]),
        "an unlinked tip must leave the bind pose, turns {}",
        turns_of(&before)
    );
    skeleton.parents[1] = Some(0);
    let after = warm.eval(&skeleton, &graph);
    assert!(
        turned(&after, &[0]),
        "warm cache kept the empty chain after tip was parented to root; turns {}",
        turns_of(&after)
    );
}

fn rotation_of(m: Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

fn bone_axis(skeleton: &Skeleton, chain: &JointChain, index: usize) -> Vec3 {
    chain
        .joints
        .get(index + 1)
        .map(|&child| {
            skeleton.bind_pose[child as usize]
                .translation
                .normalize_or(Vec3::Y)
        })
        .unwrap_or(Vec3::Y)
}

fn limit_of(chain: &JointChain, index: usize) -> JointLimit {
    chain.limits.get(index).copied().unwrap_or(JointLimit::Free)
}

fn tip_world(skeleton: &Skeleton, pose: &Pose, chain: &JointChain) -> Vec3 {
    let globals = skeleton.globals(pose, None);
    let tip = chain.joints[chain.joints.len() - 1] as usize;
    globals[tip].w_axis.truncate()
}

/// The pre-fix CCD sweep: recompute forward kinematics before each joint, then the same
/// rotation step `ccd_sweep` applies. Ancestor globals do not depend on descendant turns, so
/// this matches the running `below` product, including non-uniform scale, when that product is
/// exact.
fn reference_ccd(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    target: Vec3,
    settings: ChainIkSettings,
) {
    let count = chain.joints.len();
    let mut error = (tip_world(skeleton, pose, chain) - target).length();
    for _ in 0..settings.iterations {
        if error <= settings.tolerance {
            break;
        }
        for index in (0..count - 1).rev() {
            let globals = skeleton.globals(pose, None);
            let joint = chain.joints[index] as usize;
            let position = globals[joint].w_axis.truncate();
            let tip = globals[chain.joints[count - 1] as usize].w_axis.truncate();
            let to_tip = (tip - position).normalize_or_zero();
            let to_target = (target - position).normalize_or_zero();
            if to_tip.length_squared() < 0.5 || to_target.length_squared() < 0.5 {
                continue;
            }
            let parent_rotation = if index == 0 {
                match skeleton.parent_of(chain.joints[0] as usize) {
                    Some(parent) => rotation_of(globals[parent]),
                    None => Quat::IDENTITY,
                }
            } else {
                rotation_of(globals[chain.joints[index - 1] as usize])
            };
            let new_global =
                (Quat::from_rotation_arc(to_tip, to_target) * rotation_of(globals[joint])).normalize();
            let local = (parent_rotation.inverse() * new_global).normalize();
            let bind = skeleton.bind_pose[joint].rotation;
            pose.rotations[joint] =
                limit_of(chain, index).constrain(local, bind, bone_axis(skeleton, chain, index));
        }
        error = (tip_world(skeleton, pose, chain) - target).length();
    }
}

fn column(joints: usize, segment: f32) -> Skeleton {
    let names = (0..joints).map(|i| format!("j{i}")).collect();
    let parents = (0..joints)
        .map(|i| i.checked_sub(1).map(|parent| parent as u32))
        .collect();
    let binds = (0..joints)
        .map(|i| {
            bind(if i == 0 {
                Vec3::ZERO
            } else {
                Vec3::new(0.0, segment, 0.0)
            })
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("column")
}

fn assert_ccd_matches(label: &str, skeleton: &Skeleton, chain: &JointChain, start: &Pose, target: Vec3) {
    let settings = ChainIkSettings {
        iterations: 4,
        tolerance: 0.0,
        weight: 1.0,
    };
    let mut solved = start.clone();
    let mut scratch = ChainIkScratch::default();
    solve_chain(
        skeleton,
        &mut solved,
        chain,
        target,
        None,
        ChainSolver::Ccd,
        settings,
        &mut scratch,
    );
    let mut reference = start.clone();
    reference_ccd(skeleton, &mut reference, chain, target, settings);
    let solved_tip = tip_world(skeleton, &solved, chain);
    let reference_tip = tip_world(skeleton, &reference, chain);
    let gap = (solved_tip - reference_tip).length();
    assert!(
        gap < 1e-4,
        "{label}: tip drifted {gap} m (solved {solved_tip:?}, full FK {reference_tip:?})"
    );
    for &joint in &chain.joints {
        let drift = solved.rotations[joint as usize]
            .angle_between(reference.rotations[joint as usize])
            .abs();
        assert!(
            drift < 1e-4,
            "{label}: joint {joint} rotation drifted {drift} rad"
        );
    }
}

/// Review-c finding 6: one CCD sweep keeps a running product instead of recomputing chain
/// globals per joint. The tip and the chain rotations stay within 1e-4 of a sweep that
/// recomputes full forward kinematics before every joint.
#[test]
fn ccd_sweep_matches_full_forward_kinematics() {
    let target = Vec3::new(0.3, 0.8, 0.2);
    let straight = column(5, 0.4);
    let chain = JointChain::between(&straight, "j0", "j4").expect("chain");
    assert_ccd_matches("straight", &straight, &chain, &straight.bind_pose_pose(), target);

    let mut pre_rotated = straight.bind_pose_pose();
    pre_rotated.rotations[1] = Quat::from_rotation_z(0.7);
    pre_rotated.rotations[2] = Quat::from_rotation_x(-0.4);
    assert_ccd_matches("pre-rotated", &straight, &chain, &pre_rotated, target);

    let mut scaled = straight.bind_pose_pose();
    scaled.scales[1] = Vec3::new(2.0, 0.5, 1.0);
    scaled.scales[2] = Vec3::new(1.0, 1.0, 3.0);
    assert_ccd_matches("non-uniform scale", &straight, &chain, &scaled, target);

    let limited = chain.clone().with_uniform_limit(JointLimit::Cone {
        max_swing: 0.35,
        min_twist: -0.2,
        max_twist: 0.2,
    });
    assert_ccd_matches("cone", &straight, &limited, &straight.bind_pose_pose(), target);

    let hinge = chain.with_uniform_limit(JointLimit::Hinge {
        axis: Vec3::Z,
        min: -0.4,
        max: 0.8,
    });
    assert_ccd_matches("hinge", &straight, &hinge, &straight.bind_pose_pose(), target);

    let spine = column(6, 0.25);
    let mut posed = spine.bind_pose_pose();
    posed.translations[0] = Vec3::new(0.2, 0.0, 0.1);
    posed.rotations[0] = Quat::from_rotation_y(0.5);
    posed.rotations[1] = Quat::from_rotation_x(0.3);
    let upper = JointChain::between(&spine, "j2", "j5").expect("upper chain");
    assert_ccd_matches("chain below a posed root", &spine, &upper, &posed, target);
}
