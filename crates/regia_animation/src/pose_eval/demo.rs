// SPDX-License-Identifier: MPL-2.0
//! A networked-character demo: the plain demo humanoid with a locomotion graph that uses every
//! node kind a snapshot keeps (a state machine with cross-fades, a 1D blend space, an override
//! layer with weight easing). Tests of pose evaluation, lag compensation and remote animation
//! share it so they exercise the same graph.

use crate::blendspace::{BlendSample1D, BlendSpace1D};
use crate::demo_rig::{DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use crate::eval::ClipLibrary;
use crate::graph::{
    AnimGraph, AnimNode, BlendSpace1DNode, ClipNode, JointMaskDef, LayerEntry, LayerMode, LayerNode,
};
use crate::params::{ParamDecl, ParamValue};
use crate::skeleton::Skeleton;
use crate::state_machine::{
    CmpOp, Condition, StateMachineDef, TransitionBlend, TransitionCurve, TransitionDef,
};

/// Float parameter: ground speed in m/s (the blend space's axis).
pub const SPEED: &str = "Speed";
/// Float parameter: weight of the upper-body layer, eased over 0.2 s.
pub const AIM: &str = "Aim";
/// Trigger parameter: jump.
pub const JUMP: &str = "Jump";

fn clip(name: &str, looping: bool, speed: f32) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: name.into(),
        looping,
        speed,
        sync_group: None,
    })
}

fn transition(from: &str, to: &str, conditions: Vec<Condition>, duration: f32) -> TransitionDef {
    TransitionDef {
        from: Some(from.into()),
        to: to.into(),
        conditions,
        duration,
        curve: TransitionCurve::Smooth,
        interruptible: false,
        blend: TransitionBlend::Crossfade,
    }
}

/// The demo graph: `Locomotion` (idle / walk / run on [`SPEED`]) and `Jump` (a one-shot, left on
/// clip end), cross-faded; an upper-body override layer weighted by [`AIM`].
pub fn demo_net_graph() -> AnimGraph {
    let locomotion = AnimNode::BlendSpace1D(BlendSpace1DNode {
        space: BlendSpace1D {
            param: SPEED.into(),
            samples: [(0.0, "idle"), (1.3, "walk"), (3.2, "run")]
                .iter()
                .map(|&(value, clip)| BlendSample1D {
                    value,
                    clip: clip.into(),
                })
                .collect(),
        },
        sync_group: None,
    });
    let machine = AnimNode::StateMachine(StateMachineDef {
        initial: "Locomotion".into(),
        states: vec![
            ("Locomotion".into(), locomotion),
            ("Jump".into(), clip("run", false, 1.5)),
        ],
        transitions: vec![
            transition(
                "Locomotion",
                "Jump",
                vec![
                    Condition::Trigger { param: JUMP.into() },
                    Condition::Float {
                        param: SPEED.into(),
                        op: CmpOp::Ge,
                        value: 0.0,
                    },
                ],
                0.15,
            ),
            transition("Jump", "Locomotion", vec![Condition::ClipFinished], 0.2),
        ],
    });
    AnimGraph {
        parameters: vec![
            ParamDecl(SPEED.into(), ParamValue::Float(0.0)),
            ParamDecl(AIM.into(), ParamValue::Float(0.0)),
            ParamDecl(JUMP.into(), ParamValue::Trigger),
        ],
        root: AnimNode::Layer(LayerNode {
            base: Box::new(machine),
            layers: vec![LayerEntry {
                graph: Box::new(clip("walk", true, 0.5)),
                mask: JointMaskDef {
                    joints: vec!["chest".into()],
                    include_children: true,
                },
                mode: LayerMode::Override,
                weight_param: AIM.into(),
                weight_inertialize: 0.2,
            }],
        }),
    }
}

/// The plain 20-joint demo humanoid, its idle / walk / run clips and [`demo_net_graph`].
/// `None` only if the demo rig could not be built.
pub fn demo_net_character() -> Option<(Skeleton, AnimGraph, ClipLibrary)> {
    let skeleton = demo_humanoid(&DemoRig::default())?;
    let mut clips = ClipLibrary::new();
    for c in demo_locomotion(&skeleton, &DemoGait::standard()) {
        clips.insert(c);
    }
    Some((skeleton, demo_net_graph(), clips))
}
