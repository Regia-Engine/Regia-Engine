// SPDX-License-Identifier: MPL-2.0
//! IK retargeting: move the IK parts of authored animation between skeletons.
//!
//! [`crate::retarget::Retargeter`] transfers *poses* and bakes *clips* from a source skeleton
//! to a target one. But an animation graph (`.animgraph.ron`) and its authoring sidecar
//! (`.animedit.ron`) carry **joint names** the retargeter does not touch: two-bone IK chains,
//! look-at joints, layer masks, sockets and their IK targets. This module retargets those
//! references through the same role map, so a graph with IK nodes authored for one rig plays
//! on another rig of the same family.
//!
//! Semantics (every rule mirrors how the retargeter already maps poses):
//!
//! - Joint-name references (chains, look-at joints, mask entries, socket joints, and the
//!   v4 `ChainIK` root, tip and limit joints and `BodyAim` joints and aim joint) map through
//!   the retargeter's role map. An unmapped socket or authoring IK chain is dropped whole; an
//!   unmapped mask entry is removed; an unmapped graph chain, look-at joint, chain IK or body
//!   aim joint keeps its source name, so the graph still plays and the evaluator's existing
//!   "unknown joint" warning fires. Joint limits keep their angles: they are relative to each
//!   joint's bind rotation, which the role map pairs, and a cone's swing and twist are measured
//!   about the bone, whatever its roll.
//! - Directions written in a joint's own frame (a `BodyAim` axis, a hinge limit's axis) turn
//!   through the paired joints' rest-frame difference
//!   ([`crate::retarget::Retargeter::retarget_joint_direction`]), the same difference the pose
//!   transfer applies, so the aim still points along the same anatomical direction (the face)
//!   and a knee still bends the same way on a rig with another bone roll (review-c finding 5,
//!   ADR-8735). A joint that does not map keeps the authored direction. A uniform hinge limit
//!   whose joints need different axes on the target becomes a per-joint list.
//! - Parameter names (IK targets, poles, weights, blend-space inputs) are skeleton-independent
//!   and pass through unchanged.
//! - A socket target remaps through the two skeletons' own socket lists: a socket is matched by
//!   name first, then by the retarget of its joint, so a target rig that names the same socket
//!   differently on the same joint still resolves.
//!
//! Provenance (`docs/provenance.md`): this is the role-mapping of `crate::retarget` (Monzani,
//! Baerlocher, Boulic & Thalmann, Computer Graphics Forum 19(3), 2000; Gleicher, SIGGRAPH 1998)
//! applied to IK references — no new solve math.

use std::collections::HashMap;

use regia_core::math::Vec3;

use crate::authoring::{AnimEditDocument, IkChainDef};
use crate::chain_ik::{JointChain, JointLimit};
use crate::graph::{
    AdditiveNode, AnimGraph, AnimNode, BlendNode, BodyAimNode, ChainIKNode, ChainLimits, IkTarget,
    JointMaskDef, LayerEntry, LayerNode, LookAtNode, NamedJointLimit, RootMotionNode, TwoBoneIKNode,
};
use crate::retarget::Retargeter;
use crate::skeleton::SkeletonSocket;
use crate::state_machine::StateMachineDef;

/// Retarget one socket: the socket's **joint** is remapped; its name and local offset are kept.
/// `None` when the socket's joint does not map.
pub fn retarget_socket(rt: &Retargeter, socket: &SkeletonSocket) -> Option<SkeletonSocket> {
    Some(SkeletonSocket {
        name: socket.name.clone(),
        joint: rt.retarget_joint_name(&socket.joint)?.to_string(),
        offset: socket.offset,
    })
}

/// Retarget a list of sockets, dropping those on joints that do not map.
pub fn retarget_sockets(rt: &Retargeter, sockets: &[SkeletonSocket]) -> Vec<SkeletonSocket> {
    sockets.iter().filter_map(|s| retarget_socket(rt, s)).collect()
}

/// Retarget an IK chain definition: its joints are remapped; the name and the pole parameter
/// pass through. `None` when a joint does not map.
pub fn retarget_ik_chain(rt: &Retargeter, chain: &IkChainDef) -> Option<IkChainDef> {
    Some(IkChainDef {
        name: chain.name.clone(),
        joints: rt.retarget_chain(&chain.joints)?,
        pole_param: chain.pole_param.clone(),
    })
}

/// Retarget the sockets and IK chains of an `.animedit.ron` document. Per-clip edits (notifies,
/// curves, property tracks, root-motion mode) reference clip names and reflected property
/// paths, not joints, so they pass through unchanged.
pub fn retarget_animedit(rt: &Retargeter, document: &AnimEditDocument) -> AnimEditDocument {
    AnimEditDocument {
        format_version: document.format_version,
        sockets: retarget_sockets(rt, &document.sockets),
        ik_chains: document
            .ik_chains
            .iter()
            .filter_map(|c| retarget_ik_chain(rt, c))
            .collect(),
        clips: document.clips.clone(),
    }
}

/// Retarget an animation graph: every joint-name reference (two-bone chains, look-at joints,
/// layer masks) maps through the retargeter's role map; socket targets remap through the two
/// skeletons' socket lists; parameter names pass through unchanged.
pub fn retarget_graph(rt: &Retargeter, graph: &AnimGraph) -> AnimGraph {
    let sockets = socket_map(rt);
    AnimGraph {
        parameters: graph.parameters.clone(),
        root: retarget_node(&graph.root, rt, &sockets),
    }
}

/// Source socket name → target socket name. A socket matches by name first, then by the
/// retarget of its joint (so a socket the target rig names differently on the same joint
/// still maps). With no sockets on either side the map is empty and names pass through.
fn socket_map(rt: &Retargeter) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for source in &rt.source().sockets {
        if rt.target().sockets.iter().any(|t| t.name == source.name) {
            map.entry(source.name.clone())
                .or_insert_with(|| source.name.clone());
        }
        if let Some(joint) = rt.retarget_joint_name(&source.joint)
            && let Some(target) = rt.target().sockets.iter().find(|t| t.joint == joint)
        {
            map.entry(source.name.clone())
                .or_insert_with(|| target.name.clone());
        }
    }
    map
}

fn retarget_ik_target(target: &IkTarget, sockets: &HashMap<String, String>) -> IkTarget {
    match target {
        IkTarget::Param(_) => target.clone(),
        IkTarget::Socket(name) => {
            IkTarget::Socket(sockets.get(name).cloned().unwrap_or_else(|| name.clone()))
        }
    }
}

fn retarget_two_bone(n: &TwoBoneIKNode, rt: &Retargeter, sockets: &HashMap<String, String>) -> TwoBoneIKNode {
    TwoBoneIKNode {
        inner: Box::new(retarget_node(&n.inner, rt, sockets)),
        chain: rt.retarget_chain(&n.chain).unwrap_or_else(|| n.chain.clone()),
        target: retarget_ik_target(&n.target, sockets),
        pole: n.pole.clone(),
        weight_param: n.weight_param.clone(),
    }
}

fn retarget_look_at(n: &LookAtNode, rt: &Retargeter, sockets: &HashMap<String, String>) -> LookAtNode {
    LookAtNode {
        inner: Box::new(retarget_node(&n.inner, rt, sockets)),
        joint: rt
            .retarget_joint_name(&n.joint)
            .map(|s| s.to_string())
            .unwrap_or_else(|| n.joint.clone()),
        target: retarget_ik_target(&n.target, sockets),
        max_yaw: n.max_yaw,
        max_pitch: n.max_pitch,
        weight_param: n.weight_param.clone(),
    }
}

fn retarget_layer(n: &LayerNode, rt: &Retargeter, sockets: &HashMap<String, String>) -> LayerNode {
    LayerNode {
        base: Box::new(retarget_node(&n.base, rt, sockets)),
        layers: n
            .layers
            .iter()
            .map(|layer| LayerEntry {
                graph: Box::new(retarget_node(&layer.graph, rt, sockets)),
                mask: JointMaskDef {
                    joints: rt.retarget_mask(&layer.mask.joints),
                    include_children: layer.mask.include_children,
                },
                mode: layer.mode,
                weight_param: layer.weight_param.clone(),
                weight_inertialize: layer.weight_inertialize,
            })
            .collect(),
    }
}

fn retarget_state_machine(
    sm: &StateMachineDef,
    rt: &Retargeter,
    sockets: &HashMap<String, String>,
) -> StateMachineDef {
    StateMachineDef {
        initial: sm.initial.clone(),
        states: sm
            .states
            .iter()
            .map(|(name, node)| (name.clone(), retarget_node(node, rt, sockets)))
            .collect(),
        // Transition conditions reference parameters and state names only.
        transitions: sm.transitions.clone(),
    }
}

fn retarget_node(node: &AnimNode, rt: &Retargeter, sockets: &HashMap<String, String>) -> AnimNode {
    match node {
        // Nodes with no joint references pass through untouched.
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => node.clone(),
        AnimNode::StateMachine(sm) => AnimNode::StateMachine(retarget_state_machine(sm, rt, sockets)),
        AnimNode::Blend(n) => AnimNode::Blend(BlendNode {
            a: Box::new(retarget_node(&n.a, rt, sockets)),
            b: Box::new(retarget_node(&n.b, rt, sockets)),
            alpha_param: n.alpha_param.clone(),
        }),
        AnimNode::Additive(n) => AnimNode::Additive(AdditiveNode {
            base: Box::new(retarget_node(&n.base, rt, sockets)),
            additive: Box::new(retarget_node(&n.additive, rt, sockets)),
            weight_param: n.weight_param.clone(),
        }),
        AnimNode::Layer(n) => AnimNode::Layer(retarget_layer(n, rt, sockets)),
        AnimNode::TwoBoneIK(n) => AnimNode::TwoBoneIK(retarget_two_bone(n, rt, sockets)),
        AnimNode::LookAt(n) => AnimNode::LookAt(retarget_look_at(n, rt, sockets)),
        AnimNode::RootMotion(n) => AnimNode::RootMotion(RootMotionNode {
            inner: Box::new(retarget_node(&n.inner, rt, sockets)),
            mode: n.mode,
        }),
        AnimNode::Slot(n) => AnimNode::Slot(crate::graph::SlotNode {
            name: n.name.clone(),
            source: Box::new(retarget_node(&n.source, rt, sockets)),
            eval_idle_source: n.eval_idle_source,
        }),
        AnimNode::ChainIK(n) => AnimNode::ChainIK(ChainIKNode {
            inner: Box::new(retarget_node(&n.inner, rt, sockets)),
            root: retarget_name(rt, &n.root),
            tip: retarget_name(rt, &n.tip),
            solver: n.solver,
            target: retarget_ik_target(&n.target, sockets),
            pole: n.pole.clone(),
            limits: n
                .limits
                .as_ref()
                .map(|l| retarget_limits(rt, l, &target_chain_joints(rt, &n.root, &n.tip))),
            iterations: n.iterations,
            tolerance: n.tolerance,
            weight_param: n.weight_param.clone(),
        }),
        AnimNode::BodyAim(n) => AnimNode::BodyAim(BodyAimNode {
            inner: Box::new(retarget_node(&n.inner, rt, sockets)),
            joints: n
                .joints
                .iter()
                .map(|(name, share)| (retarget_name(rt, name), *share))
                .collect(),
            aim_joint: retarget_name(rt, &n.aim_joint),
            axis: retarget_direction(rt, &n.aim_joint, n.axis),
            target: retarget_ik_target(&n.target, sockets),
            limits: n.limits.as_ref().map(|l| {
                let joints: Vec<(String, Option<&str>)> = n
                    .joints
                    .iter()
                    .map(|(name, _)| (retarget_name(rt, name), Some(name.as_str())))
                    .collect();
                retarget_limits(rt, l, &joints)
            }),
            passes: n.passes,
            weight_param: n.weight_param.clone(),
        }),
        AnimNode::StrideWarp(n) => AnimNode::StrideWarp(crate::graph::StrideWarpNode {
            inner: Box::new(retarget_node(&n.inner, rt, sockets)),
            ground_speed_param: n.ground_speed_param.clone(),
            settings: n.settings,
            legs: n
                .legs
                .iter()
                .map(|leg| leg.each_ref().map(|name| retarget_name(rt, name)))
                .collect(),
            pelvis: retarget_name(rt, &n.pelvis),
            stride_length: n.stride_length,
        }),
        AnimNode::OrientationWarp(n) => AnimNode::OrientationWarp(crate::graph::OrientationWarpNode {
            inner: Box::new(retarget_node(&n.inner, rt, sockets)),
            heading_param: n.heading_param.clone(),
            pelvis: retarget_name(rt, &n.pelvis),
            spine: n
                .spine
                .iter()
                .map(|(name, share)| (retarget_name(rt, name), *share))
                .collect(),
            strength: n.strength,
            max_delta: n.max_delta,
        }),
    }
}

/// A graph joint name on the target rig; an unmapped name passes through (the evaluator warns).
fn retarget_name(rt: &Retargeter, name: &str) -> String {
    rt.retarget_joint_name(name)
        .map_or_else(|| name.to_string(), str::to_string)
}

/// A direction in source joint `joint`'s frame, in the paired target joint's frame; an unmapped
/// joint keeps the authored direction.
fn retarget_direction(rt: &Retargeter, joint: &str, direction: Vec3) -> Vec3 {
    rt.retarget_joint_direction(joint, direction).unwrap_or(direction)
}

/// One joint's limit on the target rig: a hinge's axis turns with the rest frame of the joint
/// (`source`, a source joint name; `None` keeps the authored axis).
fn retarget_limit(rt: &Retargeter, source: Option<&str>, limit: JointLimit) -> JointLimit {
    match (limit, source) {
        (JointLimit::Hinge { axis, min, max }, Some(joint)) => JointLimit::Hinge {
            axis: retarget_direction(rt, joint, axis),
            min,
            max,
        },
        _ => limit,
    }
}

/// The joints of the retargeted chain on the target rig, root first: each target joint name
/// with the source joint that drives it (`None` for a target joint no source joint drives).
/// Empty when the target rig has no such chain.
fn target_chain_joints<'a>(rt: &'a Retargeter, root: &str, tip: &str) -> Vec<(String, Option<&'a str>)> {
    let source_name = |t: u32| {
        rt.source_of(t)
            .and_then(|s| rt.source().joint_names.get(s as usize))
            .map(String::as_str)
    };
    let Ok(chain) = JointChain::between(rt.target(), &retarget_name(rt, root), &retarget_name(rt, tip))
    else {
        return Vec::new();
    };
    chain
        .joints
        .iter()
        .filter_map(|&t| {
            let name = rt.target().joint_names.get(t as usize)?;
            Some((name.clone(), source_name(t)))
        })
        .collect()
}

/// Largest component difference under which two retargeted hinge axes count as one.
const SAME_AXIS: f32 = 1e-5;

/// The limits of a node turning `joints` (target joint name, source joint name) on the target
/// rig. A uniform hinge whose joints' rest frames differ on the target becomes one hinge per
/// joint, each with its own axis; a uniform limit that is the same for every joint stays
/// uniform (also when `joints` is empty: the chain does not resolve on the target).
fn retarget_limits(rt: &Retargeter, limits: &ChainLimits, joints: &[(String, Option<&str>)]) -> ChainLimits {
    match limits {
        ChainLimits::Uniform(limit @ JointLimit::Hinge { .. }) => {
            let per_joint: Vec<JointLimit> = joints
                .iter()
                .map(|(_, source)| retarget_limit(rt, *source, *limit))
                .collect();
            let axis_of = |l: &JointLimit| match *l {
                JointLimit::Hinge { axis, .. } => axis,
                _ => Vec3::ZERO,
            };
            let Some(first) = per_joint.first() else {
                return ChainLimits::Uniform(*limit);
            };
            let shared = per_joint
                .iter()
                .all(|l| (axis_of(l) - axis_of(first)).abs().max_element() <= SAME_AXIS);
            if shared {
                ChainLimits::Uniform(*first)
            } else {
                ChainLimits::PerJoint(
                    joints
                        .iter()
                        .zip(per_joint)
                        .map(|((joint, _), limit)| NamedJointLimit {
                            joint: joint.clone(),
                            limit,
                        })
                        .collect(),
                )
            }
        }
        ChainLimits::Uniform(limit) => ChainLimits::Uniform(*limit),
        ChainLimits::PerJoint(list) => ChainLimits::PerJoint(
            list.iter()
                .map(|l| NamedJointLimit {
                    joint: retarget_name(rt, &l.joint),
                    limit: retarget_limit(rt, Some(&l.joint), l.limit),
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;
    use crate::retarget::RetargetProfile;
    use crate::skeleton::{LocalBind, Skeleton};

    /// A 12-joint humanoid mirroring `retarget.rs`'s test fixture: root, hips, spine, chest,
    /// neck, head, left arm (upper/fore/hand) and left leg (thigh/calf/foot).
    fn humanoid(names: &[&str; 12], size: f32) -> Skeleton {
        let parents = vec![
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(3),
            Some(6),
            Some(7),
            Some(1),
            Some(9),
            Some(10),
        ];
        let s = |v: regia_core::math::Vec3| translated(v * size);
        let binds: Vec<LocalBind> = vec![
            LocalBind::default(),
            s(regia_core::math::Vec3::new(0.0, 1.0, 0.0)),
            s(regia_core::math::Vec3::new(0.0, 0.15, 0.0)),
            s(regia_core::math::Vec3::new(0.0, 0.2, 0.0)),
            s(regia_core::math::Vec3::new(0.0, 0.25, 0.0)),
            s(regia_core::math::Vec3::new(0.0, 0.1, 0.0)),
            s(regia_core::math::Vec3::new(0.15, 0.2, 0.0)),
            s(regia_core::math::Vec3::new(0.3, 0.0, 0.0)),
            s(regia_core::math::Vec3::new(0.3, 0.0, 0.0)),
            s(regia_core::math::Vec3::new(0.1, -0.05, 0.0)),
            s(regia_core::math::Vec3::new(0.0, -0.45, 0.0)),
            s(regia_core::math::Vec3::new(0.0, -0.45, 0.0)),
        ];
        crate::skeleton::Skeleton::from_local_bind(
            names.iter().map(|n| n.to_string()).collect(),
            parents,
            binds,
        )
        .expect("valid skeleton")
    }

    const MIXAMO: [&str; 12] = [
        "Armature",
        "mixamorig:Hips",
        "mixamorig:Spine",
        "mixamorig:Spine1",
        "mixamorig:Neck",
        "mixamorig:Head",
        "mixamorig:LeftArm",
        "mixamorig:LeftForeArm",
        "mixamorig:LeftHand",
        "mixamorig:LeftUpLeg",
        "mixamorig:LeftLeg",
        "mixamorig:LeftFoot",
    ];
    const GAME: [&str; 12] = [
        "root",
        "pelvis",
        "spine_01",
        "spine_02",
        "neck_01",
        "head",
        "upperarm_l",
        "lowerarm_l",
        "hand_l",
        "thigh_l",
        "calf_l",
        "foot_l",
    ];

    fn retargeter(source: &crate::skeleton::Skeleton, target: &crate::skeleton::Skeleton) -> Retargeter {
        Retargeter::new(
            source,
            &RetargetProfile::guess_humanoid(source),
            target,
            &RetargetProfile::guess_humanoid(target),
        )
        .expect("retargeter")
    }

    #[test]
    fn retargets_the_animedit_sidecar() {
        let source = humanoid(&MIXAMO, 1.0);
        let target = humanoid(&GAME, 1.5);
        let rt = retargeter(&source, &target);

        let mut doc = AnimEditDocument::default();
        doc.sockets.push(SkeletonSocket {
            name: "grip".into(),
            joint: "mixamorig:LeftHand".into(),
            offset: LocalBind::default(),
        });
        doc.sockets.push(SkeletonSocket {
            name: "tail_tip".into(),
            joint: "tail".into(),
            offset: LocalBind::default(),
        });
        doc.ik_chains.push(IkChainDef {
            name: "LeftArm".into(),
            joints: [
                "mixamorig:LeftArm".into(),
                "mixamorig:LeftForeArm".into(),
                "mixamorig:LeftHand".into(),
            ],
            pole_param: Some("ArmPole".into()),
        });

        let out = retarget_animedit(&rt, &doc);
        // The hand socket's joint remaps; the socket on an unmapped joint is dropped.
        assert_eq!(out.sockets.len(), 1);
        assert_eq!(out.sockets[0].name, "grip");
        assert_eq!(out.sockets[0].joint, "hand_l");
        // The chain remaps joint by joint and keeps its pole parameter.
        assert_eq!(out.ik_chains.len(), 1);
        assert_eq!(out.ik_chains[0].joints, ["upperarm_l", "lowerarm_l", "hand_l"]);
        assert_eq!(out.ik_chains[0].pole_param.as_deref(), Some("ArmPole"));
    }

    #[test]
    fn retargets_a_graph_with_ik_nodes() {
        let source = humanoid(&MIXAMO, 1.0);
        let target = humanoid(&GAME, 1.5);
        let rt = retargeter(&source, &target);

        let graph = AnimGraph {
            parameters: vec![crate::params::ParamDecl(
                "Target".into(),
                crate::params::ParamValue::Vec3(regia_core::math::Vec3::ZERO),
            )],
            root: AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(AnimNode::Layer(LayerNode {
                    base: Box::new(AnimNode::LookAt(LookAtNode {
                        inner: Box::new(AnimNode::Clip(crate::graph::ClipNode {
                            clip: "Walk".into(),
                            looping: true,
                            speed: 1.0,
                            sync_group: None,
                        })),
                        joint: "mixamorig:Head".into(),
                        target: IkTarget::Param("Target".into()),
                        max_yaw: 1.0,
                        max_pitch: 1.0,
                        weight_param: None,
                    })),
                    layers: vec![LayerEntry {
                        graph: Box::new(AnimNode::Clip(crate::graph::ClipNode {
                            clip: "Wave".into(),
                            looping: true,
                            speed: 1.0,
                            sync_group: None,
                        })),
                        mask: JointMaskDef {
                            joints: vec!["mixamorig:LeftArm".into(), "unknown".into()],
                            include_children: true,
                        },
                        mode: crate::graph::LayerMode::Override,
                        weight_param: "WaveWeight".into(),
                        weight_inertialize: 0.0,
                    }],
                })),
                chain: [
                    "mixamorig:LeftArm".into(),
                    "mixamorig:LeftForeArm".into(),
                    "mixamorig:LeftHand".into(),
                ],
                target: IkTarget::Param("Target".into()),
                pole: Some("ArmPole".into()),
                weight_param: None,
            }),
        };

        let out = retarget_graph(&rt, &graph);
        let AnimNode::TwoBoneIK(two) = &out.root else {
            panic!("two-bone root");
        };
        assert_eq!(two.chain, ["upperarm_l", "lowerarm_l", "hand_l"]);
        assert_eq!(two.pole.as_deref(), Some("ArmPole"));
        let AnimNode::Layer(layer) = two.inner.as_ref() else {
            panic!("layer inner");
        };
        // The mask remaps the known joint and drops the unknown one.
        assert_eq!(layer.layers[0].mask.joints, ["upperarm_l"]);
        assert!(layer.layers[0].mask.include_children);
        let AnimNode::LookAt(look) = layer.base.as_ref() else {
            panic!("look-at base");
        };
        assert_eq!(look.joint, "head");
        assert!(matches!(&look.target, IkTarget::Param(_)));
    }

    #[test]
    fn retargeted_graph_maps_chain_ik_joints() {
        use crate::chain_ik::{ChainSolver, JointLimit};
        let source = humanoid(&MIXAMO, 1.0);
        let target = humanoid(&GAME, 1.5);
        let rt = retargeter(&source, &target);
        let cone = JointLimit::Cone {
            max_swing: 0.5,
            min_twist: -0.2,
            max_twist: 0.2,
        };
        let graph = AnimGraph {
            parameters: vec![crate::params::ParamDecl(
                "Target".into(),
                crate::params::ParamValue::Vec3(regia_core::math::Vec3::ZERO),
            )],
            root: AnimNode::BodyAim(BodyAimNode {
                inner: Box::new(AnimNode::ChainIK(ChainIKNode {
                    inner: Box::new(AnimNode::Clip(crate::graph::ClipNode {
                        clip: "Walk".into(),
                        looping: true,
                        speed: 1.0,
                        sync_group: None,
                    })),
                    root: "mixamorig:Spine".into(),
                    tip: "mixamorig:Head".into(),
                    solver: ChainSolver::Ccd,
                    target: IkTarget::Param("Target".into()),
                    pole: Some("SpinePole".into()),
                    limits: Some(ChainLimits::PerJoint(vec![
                        NamedJointLimit {
                            joint: "mixamorig:Spine1".into(),
                            limit: cone,
                        },
                        NamedJointLimit {
                            joint: "tail_03".into(),
                            limit: JointLimit::Free,
                        },
                    ])),
                    iterations: 40,
                    tolerance: 0.002,
                    weight_param: Some("ReachWeight".into()),
                })),
                joints: vec![
                    ("mixamorig:Spine".into(), 1.0),
                    ("mixamorig:Spine1".into(), 2.0),
                    ("mixamorig:Neck".into(), 0.5),
                ],
                aim_joint: "mixamorig:Head".into(),
                axis: regia_core::math::Vec3::new(0.0, 0.0, -1.0),
                target: IkTarget::Param("Target".into()),
                limits: Some(ChainLimits::Uniform(cone)),
                passes: 3,
                weight_param: None,
            }),
        };

        let out = retarget_graph(&rt, &graph);
        let AnimNode::BodyAim(aim) = &out.root else {
            panic!("body aim root");
        };
        let names: Vec<&str> = aim.joints.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["spine_01", "spine_02", "neck_01"]);
        let shares: Vec<f32> = aim.joints.iter().map(|(_, s)| *s).collect();
        assert_eq!(shares, [1.0, 2.0, 0.5], "shares pass through");
        assert_eq!(aim.aim_joint, "head");
        assert_eq!(aim.limits, Some(ChainLimits::Uniform(cone)));
        assert_eq!(aim.passes, 3);
        let AnimNode::ChainIK(chain) = aim.inner.as_ref() else {
            panic!("chain IK inner");
        };
        assert_eq!((chain.root.as_str(), chain.tip.as_str()), ("spine_01", "head"));
        assert_eq!(chain.solver, ChainSolver::Ccd);
        assert_eq!(chain.pole.as_deref(), Some("SpinePole"));
        assert_eq!(chain.weight_param.as_deref(), Some("ReachWeight"));
        assert_eq!((chain.iterations, chain.tolerance), (40, 0.002));
        // The known limit joint remaps; the unmapped one keeps its name (the evaluator warns).
        let Some(ChainLimits::PerJoint(limits)) = &chain.limits else {
            panic!("per-joint limits");
        };
        assert_eq!(limits[0].joint, "spine_02");
        assert_eq!(limits[0].limit, cone);
        assert_eq!(limits[1].joint, "tail_03");
        // The retargeted chain resolves on the target rig.
        assert!(crate::chain_ik::JointChain::between(&target, &chain.root, &chain.tip).is_ok());
    }

    #[test]
    fn socket_targets_remap_through_the_skeletons() {
        let mut source = humanoid(&MIXAMO, 1.0);
        source.sockets.push(SkeletonSocket {
            name: "grip".into(),
            joint: "mixamorig:LeftHand".into(),
            offset: LocalBind::default(),
        });
        let mut target = humanoid(&GAME, 1.5);
        target.sockets.push(SkeletonSocket {
            name: "hold".into(),
            joint: "hand_l".into(),
            offset: LocalBind::default(),
        });
        let rt = retargeter(&source, &target);

        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(AnimNode::Clip(crate::graph::ClipNode {
                    clip: "Wave".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                chain: [
                    "mixamorig:LeftArm".into(),
                    "mixamorig:LeftForeArm".into(),
                    "mixamorig:LeftHand".into(),
                ],
                target: IkTarget::Socket("grip".into()),
                pole: None,
                weight_param: None,
            }),
        };
        let out = retarget_graph(&rt, &graph);
        let AnimNode::TwoBoneIK(two) = &out.root else {
            panic!("two-bone root");
        };
        // The socket name follows the target rig's name for the same joint.
        assert_eq!(two.target, IkTarget::Socket("hold".into()));
    }

    /// `humanoid` with the target joints' bind local rotations from `rotation` (joint index).
    fn humanoid_turned(
        names: &[&str; 12],
        size: f32,
        rotation: impl Fn(usize) -> regia_core::math::Quat,
    ) -> Skeleton {
        let base = humanoid(names, size);
        let binds = base
            .bind_pose
            .iter()
            .enumerate()
            .map(|(i, b)| LocalBind {
                rotation: rotation(i),
                ..*b
            })
            .collect();
        Skeleton::from_local_bind(base.joint_names.clone(), base.parents.clone(), binds)
            .expect("valid skeleton")
    }

    fn arm_chain_graph(limits: ChainLimits) -> AnimGraph {
        AnimGraph {
            parameters: vec![crate::params::ParamDecl(
                "Target".into(),
                crate::params::ParamValue::Vec3(regia_core::math::Vec3::ZERO),
            )],
            root: AnimNode::ChainIK(ChainIKNode {
                inner: Box::new(AnimNode::Clip(crate::graph::ClipNode {
                    clip: "Walk".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                root: "mixamorig:LeftArm".into(),
                tip: "mixamorig:LeftHand".into(),
                solver: crate::chain_ik::ChainSolver::Fabrik,
                target: IkTarget::Param("Target".into()),
                pole: None,
                limits: Some(limits),
                iterations: 8,
                tolerance: 1e-3,
                weight_param: None,
            }),
        }
    }

    fn close(a: regia_core::math::Vec3, b: regia_core::math::Vec3) -> bool {
        (a - b).abs().max_element() < 1e-5
    }

    /// Finding 5 (ADR-8735): a hinge limit's axis is written in the joint's own frame. On a
    /// target rig whose forearm is rolled 90° about the bone, the retargeted axis is turned by
    /// the inverse roll, so the elbow still bends in the same anatomical plane (before the fix
    /// the axis was kept, a sideways hinge).
    #[test]
    fn retargeted_hinge_axis_follows_a_ninety_degree_bone_roll() {
        use regia_core::math::{Quat, Vec3};
        let roll = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        let source = humanoid(&MIXAMO, 1.0);
        // Joint 7 is the forearm; its bone runs along +X, so a turn about X is a pure roll.
        let target = humanoid_turned(&GAME, 1.5, |i| if i == 7 { roll } else { Quat::IDENTITY });
        let rt = retargeter(&source, &target);
        let hinge = crate::chain_ik::JointLimit::Hinge {
            axis: Vec3::Z,
            min: -2.5,
            max: 0.0,
        };
        let out = retarget_graph(
            &rt,
            &arm_chain_graph(ChainLimits::PerJoint(vec![NamedJointLimit {
                joint: "mixamorig:LeftForeArm".into(),
                limit: hinge,
            }])),
        );
        let AnimNode::ChainIK(chain) = &out.root else {
            panic!("chain IK root");
        };
        let Some(ChainLimits::PerJoint(limits)) = &chain.limits else {
            panic!("per-joint limits");
        };
        assert_eq!(limits[0].joint, "lowerarm_l");
        let crate::chain_ik::JointLimit::Hinge { axis, min, max } = limits[0].limit else {
            panic!("hinge");
        };
        assert!(close(axis, roll.inverse() * Vec3::Z), "{axis:?}");
        assert!(
            !close(axis, Vec3::Z),
            "the axis must not stay in the source frame"
        );
        assert_eq!((min, max), (-2.5, 0.0), "the angles pass through");
    }

    /// A uniform hinge on a chain whose joints need different axes on the target becomes one
    /// hinge per target joint; on a chain whose rest frames all agree it stays uniform.
    #[test]
    fn a_uniform_hinge_splits_per_joint_only_when_the_rest_frames_differ() {
        use regia_core::math::{Quat, Vec3};
        let roll = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        let source = humanoid(&MIXAMO, 1.0);
        let hinge = crate::chain_ik::JointLimit::Hinge {
            axis: Vec3::Z,
            min: -1.0,
            max: 1.0,
        };
        let graph = arm_chain_graph(ChainLimits::Uniform(hinge));

        let same = humanoid(&GAME, 1.5);
        let out = retarget_graph(&retargeter(&source, &same), &graph);
        let AnimNode::ChainIK(chain) = &out.root else {
            panic!("chain IK root");
        };
        assert_eq!(chain.limits, Some(ChainLimits::Uniform(hinge)));

        let rolled = humanoid_turned(&GAME, 1.5, |i| if i == 7 { roll } else { Quat::IDENTITY });
        let out = retarget_graph(&retargeter(&source, &rolled), &graph);
        let AnimNode::ChainIK(chain) = &out.root else {
            panic!("chain IK root");
        };
        let Some(ChainLimits::PerJoint(limits)) = &chain.limits else {
            panic!("per-joint limits, got {:?}", chain.limits);
        };
        let names: Vec<&str> = limits.iter().map(|l| l.joint.as_str()).collect();
        assert_eq!(names, ["upperarm_l", "lowerarm_l", "hand_l"]);
        let axes: Vec<Vec3> = limits
            .iter()
            .map(|l| match l.limit {
                crate::chain_ik::JointLimit::Hinge { axis, .. } => axis,
                other => panic!("{other:?}"),
            })
            .collect();
        // The hand inherits the forearm's roll in its global bind frame.
        assert!(close(axes[0], Vec3::Z), "{axes:?}");
        assert!(close(axes[1], roll.inverse() * Vec3::Z), "{axes:?}");
        assert!(close(axes[2], roll.inverse() * Vec3::Z), "{axes:?}");
    }

    /// `retarget_joint_direction` is the rest-frame difference the pose transfer applies: for a
    /// posed source rig, the source joint's direction and the retargeted direction on the
    /// retargeted target pose agree in object space, with bone rolls and an A-pose rest
    /// correction (the target's upper arm bound 40° down) on the way.
    #[test]
    fn retargeted_directions_follow_the_pose_transfer() {
        use regia_core::math::{Quat, Vec3};
        let source = humanoid(&MIXAMO, 1.0);
        let target = humanoid_turned(&GAME, 1.5, |i| match i {
            5 => Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            6 => Quat::from_rotation_z(-0.7),
            7 => Quat::from_rotation_x(1.1),
            _ => Quat::IDENTITY,
        });
        let rt = retargeter(&source, &target);
        assert!(
            rt.rest_correction_degrees(6) > 10.0,
            "the A-pose arm needs a rest correction"
        );
        let mut pose = source.bind_pose_pose();
        pose.rotations[4] = Quat::from_rotation_x(0.3);
        pose.rotations[5] = Quat::from_euler(regia_core::math::EulerRot::YXZ, 0.6, -0.2, 0.1);
        pose.rotations[6] = Quat::from_rotation_z(0.5) * Quat::from_rotation_y(0.4);
        pose.rotations[7] = Quat::from_rotation_y(-0.9);
        let mut out = target.bind_pose_pose();
        rt.retarget_pose(&pose, &mut out);
        let source_globals = source.globals(&pose, None);
        let target_globals = target.globals(&out, None);
        for (s, name) in [
            (5usize, "mixamorig:Head"),
            (6, "mixamorig:LeftArm"),
            (7, "mixamorig:LeftForeArm"),
        ] {
            let t = rt.retarget_joint(name).expect("mapped") as usize;
            for dir in [Vec3::NEG_Z, Vec3::X, Vec3::new(0.3, -0.5, 0.8).normalize()] {
                let mapped = rt.retarget_joint_direction(name, dir).expect("mapped");
                let want = crate::retarget::rotation_of(source_globals[s]) * dir;
                let got = crate::retarget::rotation_of(target_globals[t]) * mapped;
                assert!(
                    want.angle_between(got) < 1e-3,
                    "{name} {dir:?}: {want:?} vs {got:?}"
                );
            }
        }
        assert_eq!(
            rt.retarget_joint_direction("tail", Vec3::X),
            None,
            "an unmapped joint"
        );
    }
}
