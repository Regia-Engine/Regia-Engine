// SPDX-License-Identifier: MIT-0
//! The Fox rig: the locomotion graph (idle/walk/run + air) driven by `CharacterState`, foot
//! IK on slopes and stairs via raycasts, and the skin-palette → renderer bridge.

use std::sync::Arc;

use regia::prelude::*;
use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::components::{Animator, JointGlobals, spawn_skeleton};
use regia_animation::eval::ClipLibrary;
use regia_animation::foot_ik::{
    FootGround, FootLegDesc, FootPlacement, FootPlacementOutput, FootPlantReport,
};
use regia_animation::graph::{AnimGraph, AnimNode, BlendSpace1DNode, ClipNode, IkTarget, TwoBoneIKNode};
use regia_animation::ik::IkChain;
use regia_animation::state_machine::{CmpOp, Condition, StateMachineDef, TransitionCurve, TransitionDef};
use regia_core::math::Vec3;
use regia_framework::character::CharacterState;
use regia_physics::queries::{QueryFilter, WorldQueries};
use regia_render::{MeshRenderer, SkinJoints};
use regia_scene::Transform;

use crate::assets::FoxAssets;
use crate::character::{FOX_SCALE, fox_to_regia_rotation};

/// The four foot chains of the Fox (joint names from the cooked skeleton).
pub const FEET: [&str; 4] = [
    "b_LeftFoot01_017",  // left hind ankle
    "b_RightFoot01_021", // right hind ankle
    "b_LeftHand_011",    // left front paw
    "b_RightHand_08",    // right front paw
];
const CHAINS: [(&str, &str, &str); 4] = [
    ("b_LeftLeg01_015", "b_LeftLeg02_016", "b_LeftFoot01_017"),
    ("b_RightLeg01_019", "b_RightLeg02_020", "b_RightFoot01_021"),
    ("b_LeftUpperArm_09", "b_LeftForeArm_010", "b_LeftHand_011"),
    ("b_RightUpperArm_06", "b_RightForeArm_07", "b_RightHand_08"),
];

/// Builds the locomotion graph: Idle(Survey) ↔ WalkRun(blend) ↔ Air, with a two-bone IK
/// wrapper per foot whose target comes from the `Foot<i>Target` vec3 parameter (written by the
/// rig's `FootPlacement`, graph output) and whose weight comes from `Foot<i>Weight`.
pub fn locomotion_graph() -> AnimGraph {
    let machine = StateMachineDef {
        initial: "Locomote".into(),
        states: vec![
            (
                "Idle".into(),
                AnimNode::Clip(ClipNode {
                    clip: "Survey".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: Some("gait".into()),
                }),
            ),
            (
                "Locomote".into(),
                AnimNode::BlendSpace1D(BlendSpace1DNode {
                    space: BlendSpace1D {
                        param: "Speed".into(),
                        samples: vec![
                            BlendSample1D {
                                value: 0.0,
                                clip: "Walk".into(),
                            },
                            BlendSample1D {
                                value: 8.0,
                                clip: "Run".into(),
                            },
                        ],
                    },
                    sync_group: Some("gait".into()),
                }),
            ),
            (
                "Air".into(),
                AnimNode::Clip(ClipNode {
                    clip: "Run".into(),
                    looping: true,
                    speed: 0.6,
                    sync_group: Some("gait".into()),
                }),
            ),
        ],
        transitions: vec![
            TransitionDef {
                from: Some("Idle".into()),
                to: "Locomote".into(),
                conditions: vec![Condition::Float {
                    param: "Speed".into(),
                    op: CmpOp::Gt,
                    value: 0.5,
                }],
                duration: 0.25,
                curve: TransitionCurve::Smooth,
                interruptible: true,
                blend: Default::default(),
            },
            TransitionDef {
                from: Some("Locomote".into()),
                to: "Idle".into(),
                conditions: vec![Condition::Float {
                    param: "Speed".into(),
                    op: CmpOp::Lt,
                    value: 0.2,
                }],
                duration: 0.3,
                curve: TransitionCurve::Smooth,
                interruptible: true,
                blend: Default::default(),
            },
            TransitionDef {
                from: None,
                to: "Air".into(),
                conditions: vec![Condition::Bool {
                    param: "Grounded".into(),
                    value: false,
                }],
                duration: 0.15,
                curve: TransitionCurve::Smooth,
                interruptible: true,
                blend: Default::default(),
            },
            TransitionDef {
                from: Some("Air".into()),
                to: "Locomote".into(),
                conditions: vec![
                    Condition::Bool {
                        param: "Grounded".into(),
                        value: true,
                    },
                    Condition::Float {
                        param: "Speed".into(),
                        op: CmpOp::Gt,
                        value: 0.5,
                    },
                ],
                duration: 0.2,
                curve: TransitionCurve::Smooth,
                interruptible: true,
                blend: Default::default(),
            },
            TransitionDef {
                from: Some("Air".into()),
                to: "Idle".into(),
                conditions: vec![Condition::Bool {
                    param: "Grounded".into(),
                    value: true,
                }],
                duration: 0.2,
                curve: TransitionCurve::Smooth,
                interruptible: true,
                blend: Default::default(),
            },
        ],
    };

    // Two-bone IK wrappers around the machine, one per foot. Targets and weights come from
    // parameters the foot-IK planner writes each frame; poles keep knees bending naturally
    // (hind knees forward, front elbows back, in the Fox's +X-forward space).
    let mut root = AnimNode::StateMachine(machine);
    for (i, (hip, knee, foot)) in CHAINS.iter().enumerate() {
        root = AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(root),
            chain: [(*hip).into(), (*knee).into(), (*foot).into()],
            target: IkTarget::Param(format!("Foot{i}Target")),
            pole: Some(format!("Foot{i}Pole")),
            weight_param: Some(format!("Foot{i}Weight")),
        });
    }
    AnimGraph {
        root,
        parameters: Vec::new(),
    }
}

/// Attaches the animated Fox rig to `character`: spawns the skeleton joints, the skinned
/// mesh, and the animator playing the locomotion graph.
pub fn attach_rig(world: &mut World, character: Entity, player: u8, fox: &FoxAssets) {
    let binding = spawn_skeleton(world, fox.skeleton.clone());
    let Some(feet) = FoxFeet::resolve(&fox.skeleton) else {
        tracing::warn!(target: "third_person", "Fox skeleton is missing expected leg joints");
        return;
    };

    let mut clips = ClipLibrary::default();
    for clip in &fox.clips {
        clips.insert(clip.clone());
    }
    let animator = Animator::graph(locomotion_graph(), clips);

    let rig_transform = Transform {
        scale: Vec3::splat(FOX_SCALE),
        rotation: fox_to_regia_rotation(),
        ..Default::default()
    };
    let mut placement = FootPlacement::default();
    placement.output = FootPlacementOutput::GraphParams {
        target: "Foot{i}Target".into(),
        weight: "Foot{i}Weight".into(),
        pole: "Foot{i}Pole".into(),
    };
    placement.character = Some(character);
    placement.probe_height = 0.6;
    placement.probe_length = 1.2;
    for (i, (hip, knee, ankle)) in CHAINS.iter().enumerate() {
        let bend = if i < 2 { 1.0 } else { -1.0 };
        placement.legs.push(FootLegDesc {
            hip: (*hip).into(),
            knee: (*knee).into(),
            ankle: (*ankle).into(),
            toe: None,
            pole_offset: Vec3::new(bend * 40.0, 0.0, 0.0),
        });
    }

    let _ = world
        .spawn((
            rig_transform,
            binding,
            animator,
            placement,
            SolvedFoot::default(),
            MeshRenderer::new(
                fox.mesh.clone(),
                Arc::new(regia_render::CpuMaterial {
                    base_color: fox_colour(player),
                    metallic: 0.0,
                    roughness: 0.85,
                    ..Default::default()
                }),
            ),
            SkinJoints::default(),
            regia_scene::components::Name::new(format!("fox-rig-{player}")),
            // The agent resolver reads the ECS `Name`, not the scene-name component.
            Name::new(format!("fox-rig-{player}")),
            VisualDrop(fox.rig_drop),
            crate::character::HasRig,
            feet,
            RigOf { character },
        ))
        .id();
}

/// Player-tinted fur.
fn fox_colour(player: u8) -> [f32; 4] {
    match player {
        0 => [0.85, 0.5, 0.2, 1.0],
        // Magenta: distinguishable from the orange fox, the blue sky clear colour and
        // the teal ground in the numeric screenshot checks.
        1 => [0.8, 0.3, 0.72, 1.0],
        _ => [0.7, 0.7, 0.7, 1.0],
    }
}

/// Metres the rig sits below the capsule centre so the Fox stands on the ground.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct VisualDrop(pub f32);

/// Links a rig to the character entity it visualises.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RigOf {
    /// The character entity.
    pub character: Entity,
}

/// Per-frame: follow the character and drive the animator from `CharacterState`.
///
/// Runs before [`regia_animation::systems::prepare_foot_placement`]. The rig is placed at the
/// character's yaw and position with the standing drop and no pelvis offset: foot placement
/// adds this frame's pelvis after this system, on top of that pose. Putting last frame's
/// pelvis in here would stack it.
#[tracing::instrument(skip_all, target = "third_person", name = "drive_rig")]
pub fn drive_rig(world: &mut World) {
    let rig_ids: Vec<Entity> = world
        .query_filtered::<Entity, With<RigOf>>()
        .iter(world)
        .collect();
    for rig in rig_ids {
        let Some(rig_of) = world.get::<RigOf>(rig).copied() else {
            continue;
        };
        let character = rig_of.character;
        let Some(character_transform) = world.get::<Transform>(character).copied() else {
            continue;
        };
        let state = world
            .get::<CharacterState>(character)
            .cloned()
            .unwrap_or_default();

        // Locomotion parameters land even on the first frame, before a pose exists.
        let speed = Vec3::new(state.velocity.x, 0.0, state.velocity.z).length();
        if let Some(mut animator) = world.get_mut::<Animator>(rig) {
            animator.set("Speed", speed);
            animator.set("Grounded", state.grounded);
        }

        let stand = world.get::<VisualDrop>(rig).map(|d| d.0).unwrap_or(0.0);
        place_rig(world, rig, character_transform, -stand);
    }
}

/// Publish [`SolvedFoot`] from this frame's solved joints.
///
/// Runs after `apply_animation`, so [`JointGlobals`] are the pose the graph's `TwoBoneIK`
/// nodes wrote and the rig transform already includes this frame's pelvis. Measuring the tips
/// before that pelvis move leaves every foot high by the offset, and the 8 cm plant test
/// fails. A rig without [`FootPlacement`], or a graph with the `TwoBoneIK` nodes removed,
/// leaves the joints at the clip pose and the span collapses.
#[tracing::instrument(skip_all, target = "third_person", name = "publish_solved_feet")]
pub fn publish_solved_feet(world: &mut World) {
    let rig_ids: Vec<Entity> = world
        .query_filtered::<Entity, With<FoxFeet>>()
        .iter(world)
        .collect();
    for rig in rig_ids {
        let Some(feet) = world.get::<FoxFeet>(rig).copied() else {
            continue;
        };
        let Some(globals) = world.get::<JointGlobals>(rig) else {
            continue;
        };
        let Some(rig_transform) = world.get::<Transform>(rig).copied() else {
            continue;
        };
        let Some(character) = world.get::<RigOf>(rig).map(|rig_of| rig_of.character) else {
            continue;
        };
        if globals.globals.len() <= feet.tips.iter().copied().max().unwrap_or(0) as usize {
            world.entity_mut(rig).insert(SolvedFoot::default());
            continue;
        }
        let pelvis = world
            .get::<FootPlantReport>(rig)
            .map(|report| report.pelvis_offset)
            .unwrap_or(0.0);
        let globals = globals.globals.clone();
        let mut posed_feet = [Vec3::ZERO; 4];
        let mut ground = [FootGround::None; 4];
        for i in 0..4 {
            let object = globals[feet.tips[i] as usize].w_axis.truncate();
            let foot_world = object_to_world(&rig_transform, object);
            posed_feet[i] = foot_world;
            if let Some(hit) = foot_probe(world, foot_world, character) {
                ground[i] = FootGround::Contact(hit);
            }
        }
        let (low_y, high_y, span) = planted_span(&posed_feet, &ground);
        world.entity_mut(rig).insert(SolvedFoot {
            low_y,
            high_y,
            span,
            pelvis_offset: pelvis,
        });
    }
}

/// What the solved pose actually did with the feet, read from [`JointGlobals`] (the
/// post-IK joint matrices), not from the planner's parameters.
///
/// `span` is `high_y - low_y` over feet whose tip joint sits within 8 cm of the ground
/// the probe found. On stairs that span is a step (~0.2 m) only when two-bone IK has
/// planted those tips; the animated pose keeps every foot at the rig's height, so the
/// feet that pass the 8 cm test all share one step and `span` stays ~0. Removing
/// [`FootPlacement`] or the `TwoBoneIK` nodes leaves the weight parameters at 0, the
/// solver skips, and the same thing happens.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SolvedFoot {
    /// Lowest planted foot's ground Y (world metres). `0` when fewer than two feet planted.
    pub low_y: f32,
    /// Highest planted foot's ground Y (world metres).
    pub high_y: f32,
    /// `high_y - low_y` for the planted feet.
    pub span: f32,
    /// Pelvis Y offset the planner applied this frame (negative sinks the rig).
    pub pelvis_offset: f32,
}

/// Planted-foot ground span from the solved joint positions.
///
/// A foot counts as planted when its world tip is within 8 cm of the probe hit. The
/// span of those hits is zero unless two planted feet stand on different ground.
fn planted_span(feet_world: &[Vec3; 4], ground: &[FootGround; 4]) -> (f32, f32, f32) {
    let mut planted = Vec::new();
    for i in 0..4 {
        if let FootGround::Contact(hit) = ground[i]
            && (feet_world[i].y - hit.y).abs() <= 0.08
        {
            planted.push(hit.y);
        }
    }
    if planted.len() < 2 {
        return (0.0, 0.0, 0.0);
    }
    let low = planted.iter().copied().fold(f32::MAX, f32::min);
    let high = planted.iter().copied().fold(f32::MIN, f32::max);
    (low, high, high - low)
}

/// Pre-resolved joint ids of the Fox's four legs (attach-time name lookup).
#[derive(Component, Debug, Clone, Copy)]
pub struct FoxFeet {
    /// Hip/knee-root joint ids, one per leg.
    pub roots: [u32; 4],
    /// Knee/elbow joint ids.
    pub mids: [u32; 4],
    /// Ankle/paw tip joint ids.
    pub tips: [u32; 4],
}

impl FoxFeet {
    /// The IK chains in leg order.
    pub fn chains(&self) -> Vec<IkChain> {
        (0..4)
            .map(|i| IkChain {
                root: self.roots[i],
                mid: self.mids[i],
                tip: self.tips[i],
            })
            .collect()
    }

    /// Resolves against `skeleton`, or `None` when any name is missing.
    pub fn resolve(skeleton: &regia_animation::Skeleton) -> Option<Self> {
        let resolve3 = |names: [&str; 4]| -> Option<[u32; 4]> {
            let mut out = [0; 4];
            for (slot, name) in names.iter().enumerate() {
                out[slot] = skeleton.joint_index(name)?;
            }
            Some(out)
        };
        Some(Self {
            roots: resolve3([
                "b_LeftLeg01_015",
                "b_RightLeg01_019",
                "b_LeftUpperArm_09",
                "b_RightUpperArm_06",
            ])?,
            mids: resolve3([
                "b_LeftLeg02_016",
                "b_RightLeg02_020",
                "b_LeftForeArm_010",
                "b_RightForeArm_07",
            ])?,
            tips: resolve3([
                "b_LeftFoot01_017",
                "b_RightFoot01_021",
                "b_LeftHand_011",
                "b_RightHand_08",
            ])?,
        })
    }
}

/// Object-space → world-space under a rig transform (scale, then rotate, then translate).
fn object_to_world(rig: &Transform, object: Vec3) -> Vec3 {
    rig.translation + rig.rotation * (object * rig.scale)
}

/// Positions the rig from the character transform plus the (visual) pelvis offset.
fn place_rig(world: &mut World, rig: Entity, character: Transform, pelvis_offset: f32) {
    if let Some(mut transform) = world.get_mut::<Transform>(rig) {
        transform.translation = character.translation + Vec3::Y * pelvis_offset;
        transform.rotation = character.rotation * fox_to_regia_rotation();
        transform.scale = Vec3::splat(FOX_SCALE);
    }
}

/// Raycasts down from just above `foot` for solid ground.
///
/// Sensors are skipped, and so is `ignore` (the character capsule). Without that exclusion
/// the probe plants the paw on the character's own body.
fn foot_probe(world: &World, foot: Vec3, ignore: Entity) -> Option<Vec3> {
    let physics = world.get_resource::<regia_physics::PhysicsWorld>()?;
    let queries = WorldQueries {
        world: physics,
        layers: None,
    };
    let excluded = [ignore];
    queries
        .raycast(
            foot + Vec3::Y * 0.6,
            Vec3::NEG_Y,
            1.2,
            QueryFilter::new().exclude_sensors().exclude_entities(&excluded),
        )
        .map(|hit| hit.point)
}
