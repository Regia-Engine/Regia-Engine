// SPDX-License-Identifier: MPL-2.0
//! Skeletal animation: clips, blend trees, state machines, IK, root motion, property tracks.
//!
//! Specification: `docs/specs/17-animation.md`.
//!
//! - [`Skeleton`] and [`AnimationClip`] are the data assets (cooked via
//!   [`compress::CompressedClip`]).
//! - [`Animator`] drives a rig through an [`AnimGraph`] (`.animgraph.ron`, frozen v4; v1 to v3 files still load) or a
//!   single clip; evaluation happens in `Update`, application in `PostUpdateSet::AnimationApply`.
//!   [`PropertyAnimator`] (property tracks: doors, lights, UI) is simulation: it advances and
//!   applies in `FixedUpdate` (`regia_app::FixedUpdateSet::Animation`), before the physics step
//!   consumes kinematic transforms (ADR-4653).
//! - [`SkinnedMesh`] binds a mesh to a [`Skeleton`] and its joint entities; [`SkinPalette`] is
//!   the skinning output the renderer draws with (`regia_framework::skinning`).
//! - [`MorphWeights`] come from clips and graphs, composed with [`MorphLayers`] (expression
//!   layers, clamps, viseme sets; [`morph`]); the renderer deforms the mesh with them.
//! - Sockets ([`Socket`]) attach entities to joints; notifies fire `AnimNotify` messages;
//!   root motion surfaces as [`RootMotionOutput`].
//!
//! - Editor support: [`authoring`] (the `.animedit.ron` sidecar: notifies, curves, property
//!   tracks, sockets, IK chains), [`curve`] (editable Hermite curves) and [`debug`] (PIE
//!   snapshots of a running graph).
//!
//! - Robustness tooling: [`check`] (clip health checks, the root motion of one lap) and
//!   [`compress::measure_compression_error`] (the measured error of a cooked clip).
//!
//! - Secondary motion: [`SpringBones`] simulates hair, tails and cloth strips after the animation
//!   and before skinning ([`spring`], `.spring.ron`).
//!
//! - Action playback: [`Montage`] (`.montage.ron`) timelines played by a [`MontagePlayer`] over
//!   the graph, cross-node sync groups ([`SyncSetup`], `.sync.ron`) and motion warping toward a
//!   [`WarpTarget`] (spec 17 § 4b).
//!
//! Add [`AnimationPlugin`] to an app to register the systems.
//!
//! # Getting started
//!
//! A [`Skeleton`] and an [`AnimationClip`] are plain data, so a clip can be sampled without an app.
//! This builds the procedural demo humanoid and its walk cycle, poses the rig at a time in the
//! clip and runs forward kinematics:
//!
//! ```
//! use regia_animation::demo_rig::{DemoGait, DemoRig, demo_humanoid, demo_locomotion};
//!
//! let skeleton = demo_humanoid(&DemoRig::default()).expect("a default rig builds");
//! assert_eq!(skeleton.len(), 20);
//! assert!(skeleton.validate().is_ok());
//!
//! let clips = demo_locomotion(&skeleton, &DemoGait::standard());
//! let walk = clips.iter().find(|c| c.name == "walk").expect("the standard gaits include a walk");
//! assert!((walk.duration - 1.0).abs() < 1e-3);
//!
//! // Start from the bind pose, overwrite what the clip animates, then compose to object space.
//! let mut pose = skeleton.bind_pose_pose();
//! walk.sample_pose(0.25, &mut pose);
//! let globals = skeleton.globals(&pose, None);
//! let palette = skeleton.skin_palette(&globals);
//! assert_eq!(palette.len(), skeleton.len());
//! ```
//!
//! In a game the [`Animator`] component does this every frame; see `book/agents/animation.md`.

pub mod aim_ik;
pub mod authoring;
pub mod bind_pose;
pub mod blendspace;
pub mod chain_ik;
pub mod check;
pub mod clip;
pub mod clip_compress;
pub mod components;
pub mod compress;
pub mod cooked;
pub mod corrective;
pub mod crowd;
pub mod curve;
pub mod debug;
pub mod demo_rig;
pub mod driver;
pub mod eval;
pub mod events;
pub mod foot_ik;
pub mod gaze;
pub mod graph;
pub mod ik;
pub mod ik_retarget;
pub mod inertialize;
pub mod live;
pub mod montage;
pub mod montage_edit;
pub mod montage_playback;
pub mod montage_player;
pub mod montage_variant;
pub mod morph;
pub mod motion_matching;
mod num;
pub mod params;
pub mod physics_blend;
pub mod pose;
pub mod pose_search;
pub mod profile;
pub mod property;
pub mod retarget;
pub mod retarget_check;
pub mod retarget_setup;
// The retargeting editor's read helpers: body chains, rest-offset angles, alignment (ADR-7801).
pub mod retarget_chain;
pub mod sample;
pub mod scripted_warps;
pub mod skeleton;
pub mod skeleton_slots;
pub mod spring;
pub mod state_machine;
pub mod sync;
pub mod systems;
pub mod twist;
pub mod warp;
// CPU linear blend skinning of cloth sim vertices (spec 55 § 2.2, ADR-1672).
pub mod cpu_skin;
// Side-effect-free pose evaluation from a captured playback state (ADR-2551).
pub mod pose_eval;
// Process-global allocation counters for the animation hot-path tests.
#[doc(hidden)]
pub mod hot_count;
// The montage replication hook: runtime changes as serializable commands (spec 34 § 9, ADR-7216).
pub mod montage_command;
/// Procedural hinges, spins and slews on plain nodes (ADR-8410).
pub mod node_driver;
/// Plain-node TRS channels and hierarchy sampling (ADR-8157).
pub mod node_tracks;
/// IK goals at retarget bake: planted feet and hands on a prop keep their place (ADR-8476).
pub mod retarget_goals;
// The graph's ChainIK and BodyAim nodes (animgraph v4, ADR-8475).
pub mod ik_nodes;
// Stride and orientation warping shared by motion matching and the graph's warp nodes (ADR-8540);
// public at `motion_matching::warp`.
mod locomotion_warp;
// The graph's StrideWarp and OrientationWarp nodes (animgraph v4, ADR-8540).
pub mod warp_nodes;

pub mod gait;
// The engine-side walker: steps the gait and solves the legs of a rig (ADR-8707).
pub mod gait_rig;
// `.gait.ron` gait settings files, their store and hot reload (gap G7).
pub mod gait_asset;
// IK level of detail for FootPlacement and GaitRig (animation-11).
pub mod ik_lod;

use regia_app::prelude::*;

pub use authoring::{
    ANIMEDIT_FORMAT_VERSION, AnimEditDocument, AnimEditError, animedit_migration_sources,
    animedit_schema_fingerprint,
};
pub use bind_pose::{BindPoseError, InstanceBindPose};
pub use clip::{AnimationClip, MorphNode, Notify, RootMotionMode, RootMotionTrack};
#[allow(deprecated)]
pub use components::SkeletonBinding;
pub use components::{
    AnimLodCamera, AnimVisible, Animator, AnimatorSource, JointGlobals, MorphTargetNames, MorphWeights,
    RootMotionOutput, RootMotionRoute, SkinPalette, SkinnedMesh, Socket, SocketTargets, UpdateMode,
};
pub use crowd::{
    CrowdAgentDesc, CrowdAgentId, CrowdAgentState, CrowdAnimation, CrowdAnimationPlugin, CrowdAnimator,
    CrowdArchetype, CrowdError, CrowdGait, CrowdLod, CrowdMember, CrowdSettings, CrowdStats,
};
pub use driver::{
    CompiledRig, DRIVERS_FORMAT_VERSION, Driver, DriverError, DriverSet, MorphNames, NamedMorphs, RigDrivers,
    RigState, drivers_migration_sources, drivers_schema_fingerprint,
};
pub use eval::{ClipLibrary, EvalContext, GraphInstance};
pub use gaze::{Gaze, GazeMorphSink, GazeRig, GazeTarget};
pub use graph::{
    ANIMGRAPH_FORMAT_VERSION, AnimGraph, AnimGraphError, AnimNode, IkTarget, animgraph_migration_sources,
    animgraph_schema_fingerprint,
};
pub use inertialize::{DecayMethod, Inertializer};
pub use live::{FeedPose, FeedPoseBuffer, FeedPoseTarget};
pub use montage::{
    MONTAGE_FORMAT_VERSION, Montage, MontageError, PastEndItem, montage_migration_sources,
    montage_schema_fingerprint,
};
pub use montage_edit::{EditReport, MontageEdit, MontageEditError, VariantEdit};
pub use montage_playback::{MontageCursor, MontagePlayback};
pub use montage_player::{
    BranchAction, MontageEvent, MontageEventKind, MontageId, MontagePhase, MontagePlayer, MontageStatus,
    PlayError, PlayOptions, SlotDef,
};
pub use montage_variant::{
    MONTAGE_VARIANT_FORMAT_VERSION, MontageLibrary, MontageVariant, SegmentOverride, VariantError,
    montage_variant_migration_sources, montage_variant_schema_fingerprint,
};
pub use morph::{
    MorphLayer, MorphLayerMode, MorphLayers, MorphSet, MorphTarget, NodeMorphPlayer, SourceNode,
};
pub use params::{ParamValue, ParameterSet};
pub use physics_blend::PhysicsPoseBlend;
pub use pose::Pose;
pub use profile::{
    ProfileError, ProfileReport, RestPoseRule, SKELPROFILE_FORMAT_VERSION, SkeletonProfile,
    skelprofile_migration_sources, skelprofile_schema_fingerprint,
};
pub use property::PropertyAnimator;
pub use retarget::{
    IkGoalMode, IkGoalSpec, RetargetOptions, RoleSettings, RootMotionScale, ScaleRule, TranslationMode,
};
pub use retarget_check::{ClipCheck, GoalCheck, MappedRole, RetargetIssue, RetargetReport};
pub use retarget_setup::{
    RETARGET_FORMAT_VERSION, RetargetSetup, RetargetSetupError, RigRoles, RoleSource, migrate_retarget,
    retarget_migration_sources, retarget_schema_fingerprint,
};
pub use sample::{SampleMode, sample_into};
pub use scripted_warps::{
    ScriptedWarp, ScriptedWarpEvent, ScriptedWarpStatus, ScriptedWarps, WarpError, WarpEvent,
};
pub use skeleton::{Skeleton, SkeletonError, SkeletonSocket};
pub use skeleton_slots::{
    MontageIssue, SLOTS_FORMAT_VERSION, SkeletonSlots, SlotEntry, SlotsError, slots_migration_sources,
    slots_schema_fingerprint,
};
pub use spring::{
    ColliderShape, SPRING_FORMAT_VERSION, SpringBoneSetup, SpringBones, SpringChain, SpringCollider,
    SpringError, SpringLod, spring_migration_sources, spring_schema_fingerprint,
};
pub use state_machine::TransitionBlend;
pub use sync::{SYNC_FORMAT_VERSION, SyncError, SyncSetup, sync_migration_sources, sync_schema_fingerprint};
pub use warp::{RotationKind, WarpAnchor, WarpMode, WarpTarget};

/// The animation plugin: registers messages, resources and the evaluate/apply systems.
#[derive(Default)]
pub struct AnimationPlugin;

impl Plugin for AnimationPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<gait::GaitState>()
            .register_type::<gait::GaitParams>()
            .register_type::<gait::FootTarget>()
            .register_type::<gait::GaitPattern>()
            .register_type::<gait::GaitPatternSet>()
            .register_type::<gait::GaitPosture>()
            .register_type::<node_driver::DriveKind>()
            .register_type::<node_driver::SlewLimits>()
            .register_type::<node_driver::NodeDrive>()
            .register_type::<node_driver::NodeDriver>()
            .register_type::<node_driver::DriveParam>()
            .register_type::<node_driver::NodeDriveParams>()
            .register_type::<node_driver::NodeDriveAngles>()
            .register_type::<node_driver::NodeDriveState>()
            .register_type::<node_driver::PlainNodeXform>()
            .register_type::<node_driver::PlainNodePose>();
        systems::register(app);
        property::register(app);
        gait_rig::register(app);
        gait_asset::register(app);
        ik_lod::register(app);
        // After the fixed-step rebase and before any animation work reads the stored targets.
        // FixedUpdate always precedes the presentation animator stage (ADR-2396, ADR-4477).
        app.add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                shift_world_space_targets.after(regia_scene::streaming::StreamSet::Rebase),
            );
    }
}

/// The world positions animation keeps outside the ECS transforms (spec 78 § 12.2): montage and
/// scripted warp targets, gaze points and the motion matcher's ground history.
type WorldSpaceHolders = (
    Option<&'static mut montage_player::MontagePlayer>,
    Option<&'static mut ScriptedWarps>,
    Option<&'static mut gaze::Gaze>,
    Option<&'static mut motion_matching::MotionMatching>,
);

/// Entities holding at least one of [`WorldSpaceHolders`].
type HoldsWorldSpace = Or<(
    With<montage_player::MontagePlayer>,
    With<ScriptedWarps>,
    With<gaze::Gaze>,
    With<motion_matching::MotionMatching>,
)>;

/// Re-expresses every stored world position after an origin rebase.
fn shift_world_space_targets(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    mut holders: Query<WorldSpaceHolders, HoldsWorldSpace>,
) {
    for message in shifted.read() {
        let shift = regia_core::math::Vec3::from_array(message.rebase.shift);
        for (player, warps, gaze, matching) in &mut holders {
            if let Some(mut player) = player
                && !player.warp_targets().is_empty()
            {
                player.shift_warp_targets(shift);
            }
            if let Some(mut warps) = warps
                && !warps.targets().is_empty()
            {
                warps.shift_targets(shift);
            }
            if let Some(mut gaze) = gaze
                && let gaze::GazeTarget::Point(point) = gaze.target
            {
                gaze.target = gaze::GazeTarget::Point(point - shift);
            }
            if let Some(mut matching) = matching
                && !matching.matcher.ground_track().is_empty()
            {
                matching.matcher.shift_ground_track(shift);
            }
        }
    }
}

/// Common imports for code using animation.
pub mod prelude {
    pub use crate::AnimationPlugin;
    pub use crate::clip::{AnimationClip, Notify, RootMotionMode, RootMotionTrack};
    #[allow(deprecated)]
    pub use crate::components::SkeletonBinding;
    pub use crate::components::{
        AnimLodCamera, AnimVisible, Animator, AnimatorSource, JointGlobals, MorphTargetNames, MorphWeights,
        RootMotionOutput, SkinPalette, SkinnedMesh, Socket, SocketTargets, UpdateMode,
    };
    pub use crate::compress::{CompressedClip, CompressionSettings, compress_clip};
    pub use crate::eval::{ClipLibrary, GraphInstance};
    pub use crate::events::{AnimNotify, AnimStateEvent};
    pub use crate::graph::{ANIMGRAPH_FORMAT_VERSION, AnimGraph, AnimGraphError, AnimNode, IkTarget};
    pub use crate::montage::Montage;
    pub use crate::montage_player::{MontageEvent, MontagePlayer, PlayOptions, SlotDef};
    pub use crate::morph::{
        MorphLayer, MorphLayerMode, MorphLayers, MorphSet, MorphTarget, NodeMorphPlayer, SourceNode,
    };
    pub use crate::params::{ParamValue, ParameterSet};
    pub use crate::pose::Pose;
    pub use crate::property::PropertyAnimator;
    pub use crate::skeleton::{Skeleton, SkeletonError, SkeletonSocket};
    pub use crate::spring::{SpringBoneSetup, SpringBones};
    pub use crate::warp::WarpTarget;
}

#[cfg(test)]
mod tests {
    use super::prelude::*;
    use regia_app::prelude::*;
    use regia_core::math::Vec3;
    use regia_scene::Transform;

    /// End-to-end: a rig with an animator plays a clip, the apply system writes joint
    /// transforms and produces a skin palette and socket resolution.
    #[test]
    fn plugin_plays_clip_end_to_end() {
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "arm".into()],
            vec![None, Some(0)],
            vec![
                crate::skeleton::LocalBind::default(),
                crate::pose::translated(Vec3::new(0.0, 1.0, 0.0)),
            ],
        )
        .expect("valid skeleton");

        // A clip that lifts the arm joint's local translation over one second.
        let mut clip = AnimationClip::empty("lift", 1.0);
        clip.tracks.push(crate::clip::JointTrack {
            joint: 1,
            translations: crate::clip::TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 3.0, 0.0)],
                exact_rotation: false,
            },
            rotations: crate::clip::TrackCurve::Static(regia_core::math::Quat::IDENTITY),
            scales: crate::clip::TrackCurve::Static(Vec3::ONE),
        });

        let skeleton = std::sync::Arc::new(skeleton);
        let rig = app.world_mut().spawn(Transform::default()).id();
        let binding = crate::components::spawn_skeleton(app.world_mut(), skeleton.clone());
        let arm = binding.joints[1];
        app.world_mut().entity_mut(rig).insert(binding);
        app.world_mut()
            .entity_mut(rig)
            .insert(Animator::single(clip, false));
        // A socket on the arm joint.
        let socket_entity = app
            .world_mut()
            .spawn(Transform::default())
            .insert(Socket {
                rig,
                name: "hand".into(),
                joint: "arm".into(),
                offset: Transform::default(),
            })
            .id();

        // Run frames until the clip lifts the arm.
        for _ in 0..64 {
            app.update();
        }
        let arm_transform = app.world().get::<Transform>(arm).copied().unwrap_or_default();
        assert!(
            (arm_transform.translation.y - 3.0).abs() < 1e-3,
            "arm should end on the last key (the clip is 1 s, 64 x 16 ms elapsed), got {}",
            arm_transform.translation.y
        );
        let palette = app.world().get::<SkinPalette>(rig);
        assert!(palette.is_some_and(|p| p.matrices.len() == 2));
        let socket_transform = app
            .world()
            .get::<Transform>(socket_entity)
            .copied()
            .unwrap_or_default();
        assert!(
            (socket_transform.translation.y - arm_transform.translation.y).abs() < 1e-4,
            "socket tracks the joint"
        );
        let sockets = app.world().get_resource::<SocketTargets>();
        assert!(sockets.is_some_and(|s| s.0.contains_key(&(rig, "hand".to_string()))));
    }
}
