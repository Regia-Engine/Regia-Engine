// SPDX-License-Identifier: MPL-2.0
//! Animation systems: evaluation (Update) and application (`PostUpdateSet::AnimationApply`).
//!
//! Specification: `docs/specs/17-animation.md` § 2: evaluation in `Update` (sample → blend →
//! post-process → pose buffer), then pose → joint `Transform` writes in
//! `PostUpdateSet::AnimationApply`, before transform propagation.

use std::mem;

use regia_app::{PostUpdate, PostUpdateSet, Time, Update};
use regia_core::math::Mat4;
use regia_ecs::ecs::system::SystemParam;
use regia_ecs::prelude::*;
use regia_scene::{GlobalTransform, Transform};
use tracing::{debug, warn};

use crate::bind_pose::{InstanceBindPose, instance_palette, instance_palette_into, retarget_to_instance};
use crate::clip::Notify;
use crate::components::{
    AnimLodCamera, AnimVisible, Animator, AnimatorSource, JointGlobals, MorphWeights, RootMotionOutput,
    SkinPalette, SkinnedMesh, Socket, SocketTargets, UpdateMode,
};
use crate::driver::RigDrivers;
use crate::eval::{ClipLibrary, EvalContext, StateEvent, evaluate_graph};
use crate::events::{AnimNotify, AnimStateEvent};
use crate::foot_ik::{
    FootPlacement, FootPlacementRespawn, FootPlacementStatus, FootPlantReport, GroundProbes,
};
use crate::gaze::Gaze;
use crate::montage_player::{MontageEvent, MontageFrame, MontagePlayer, RigPlacement};
use crate::morph::{MorphLayers, MorphTargetNames};
use crate::physics_blend::PhysicsPoseBlend;
use crate::pose::Pose;
use crate::scripted_warps::{ScriptedWarpEvent, ScriptedWarps};
use crate::spring::SpringBones;

/// What [`evaluate_animators`] reads from each rig: the animator, its skeleton and transform,
/// root-motion output, and the montage player with the rig's world placement.
type EvaluateRig = (
    Entity,
    &'static mut Animator,
    &'static SkinnedMesh,
    &'static Transform,
    Option<&'static mut RootMotionOutput>,
    (
        Option<&'static mut MontagePlayer>,
        Option<&'static GlobalTransform>,
        Has<ChildOf>,
        Option<&'static mut ScriptedWarps>,
        Option<&'static mut FootPlacement>,
    ),
);

pub use crate::foot_ik::component::prepare_foot_placement;

/// Advance and evaluate every animator into its pose buffer; emit notify and state messages,
/// promote curve values to parameters and auto-reset consumed triggers.
#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub fn evaluate_animators(
    mut commands: Commands,
    time: Res<Time>,
    mut notifies: MessageWriter<AnimNotify>,
    mut state_events: MessageWriter<AnimStateEvent>,
    mut montage_events: MessageWriter<MontageEvent>,
    mut warp_events: MessageWriter<ScriptedWarpEvent>,
    socket_targets: Res<SocketTargets>,
    lod_camera: Option<Res<AnimLodCamera>>,
    visible: Query<&AnimVisible>,
    mut animators: Query<EvaluateRig, Without<regia_scene::layers::LayerParked>>,
) {
    let _span = tracing::debug_span!("evaluate_animators").entered();
    let delta = time.delta_secs();
    for (
        entity,
        mut animator,
        binding,
        transform,
        root_output,
        (mut player, global, is_child, scripted, mut foot_placement),
    ) in &mut animators
    {
        // Update gating: Always / WhenVisible / Lod.
        let rate = match animator.update_mode {
            UpdateMode::Always => Some(1.0),
            UpdateMode::WhenVisible => visible.get(entity).is_ok().then_some(1.0),
            UpdateMode::Lod { near, mid, far_rate } => match lod_camera.as_ref().map(|c| c.position) {
                Some(cam) => {
                    // World position: a parented rig's local translation says nothing about distance.
                    let world = match global.filter(|_| is_child) {
                        Some(global) => global.0.translation.into(),
                        None => transform.translation,
                    };
                    let dist = world.distance(cam);
                    Some(if dist <= near {
                        1.0
                    } else if dist <= mid {
                        0.5
                    } else {
                        far_rate
                    })
                }
                None => Some(1.0),
            },
        };
        // Secondary motion covers no time while the animator is gated off entirely.
        animator.last_eval_delta = 0.0;
        let Some(rate) = rate else {
            continue;
        };
        // LOD animators batch skipped time into larger, less frequent steps. A negative rate
        // (a sign error in game data) runs nothing, rather than piling up a debt the animator
        // would have to pay back once the rate is fixed (deep-zai-animation finding 3). NaN
        // passes through to the `finite_or` guard below, as before.
        let rate = if rate < 0.0 { 0.0 } else { rate };
        animator.rate_accumulator += delta * rate;
        if animator.rate_accumulator < delta {
            // Gated off: the pose stays where it is, so the secondary motion must not cover
            // this frame's time either (audit-v040 M4).
            animator.last_eval_delta = 0.0;
            continue;
        }
        // A NaN speed or LOD rate (game data) must not turn into NaN clip time or a NaN
        // `pending_seconds` a character controller divides by: such a frame advances nothing.
        let eval_delta =
            crate::num::finite_or(mem::take(&mut animator.rate_accumulator) * animator.speed, 0.0);
        animator.last_eval_delta = eval_delta.abs();
        // The montage command clock covers every evaluation, playing or not (spec 34 § 9): a
        // replayed command waits for the same clock the recording player had at the call.
        if let Some(player) = player.as_mut() {
            player.bypass_change_detection().advance_clock(eval_delta);
        }

        animator.ensure_instance(binding.skeleton.len());
        let Some(mut instance) = animator.instance.take() else {
            continue;
        };
        let scratch = mem::take(&mut animator.scratch);
        let mut out = mem::take(&mut animator.pose);
        if out.len() != binding.skeleton.len() {
            out = Pose::from_identity(binding.skeleton.len());
        }

        // The rig's world placement, plus root motion not yet applied by its controller.
        let placement = || {
            let (scale, rotation, position) = match global.filter(|_| is_child) {
                Some(global) => global.0.to_scale_rotation_translation(),
                None => (transform.scale, transform.rotation, transform.translation),
            };
            let (pending_t, pending_r) = root_output.as_ref().map_or(
                (regia_core::math::Vec3::ZERO, regia_core::math::Quat::IDENTITY),
                |o| (o.pending_translation, o.pending_rotation),
            );
            RigPlacement {
                position: position + rotation * (scale * pending_t),
                rotation: (rotation * pending_r).normalize(),
                scale,
            }
        };
        // A graph with `Slot` nodes plays montages inside the graph (spec 34 M9): the montage
        // stage advances first and hands the graph its per-slot poses.
        let mut player = player;
        let graph_slots = player.as_ref().is_some_and(|p| p.is_active())
            && matches!(&animator.source, AnimatorSource::Graph { graph, .. } if graph.has_slot_nodes());
        if graph_slots {
            let check = match (&animator.source, player.as_ref()) {
                (AnimatorSource::Graph { graph, .. }, Some(player)) => {
                    let key = slot_check_key(graph, &player.slots);
                    (animator.slots_checked_for != Some(key))
                        .then(|| (key, graph.check_slot_defs(&player.slots)))
                }
                _ => None,
            };
            if let Some((key, warnings)) = check {
                animator.slots_checked_for = Some(key);
                for warning in warnings {
                    tracing::warn!(target: "regia_animation", "{warning}");
                }
            }
        }
        let mut early_notifies: Vec<Notify> = Vec::new();
        let mut slot_layers = mem::take(&mut animator.slot_layers);
        slot_layers.clear();
        let mut montage_begun = false;
        if graph_slots && let Some(player) = player.as_mut() {
            let frame = MontageFrame {
                skeleton: &binding.skeleton,
                graph_clips: Some(clips_of(&animator.source)),
                delta: eval_delta,
                rig: placement(),
            };
            montage_begun = player.begin_frame(&frame, &mut early_notifies);
            if montage_begun {
                player.slot_layers(&frame, &mut slot_layers);
            }
        }

        let sync_setup = animator.sync_setup.clone();
        let sync_state = mem::take(&mut animator.sync_state);
        // Take the scratch before `clips_of` borrows `animator.source` for the context.
        let curve_sample = mem::take(&mut animator.curve_sample);
        let curves = mem::take(&mut animator.curves);
        let node_raw = mem::take(&mut animator.node_raw);
        let morph_shared_pool = mem::take(&mut animator.morph_shared_pool);
        let morph_nodes_pool = mem::take(&mut animator.morph_nodes_pool);
        let ik_globals = mem::take(&mut animator.ik_globals);
        let reuse = mem::take(&mut animator.reuse);
        let mut ctx = EvalContext::blank(
            &binding.skeleton,
            clips_of(&animator.source),
            &animator.parameters,
            eval_delta,
            entity,
            &socket_targets.0,
            scratch,
        )
        .with_eval_scratch(
            curve_sample,
            curves,
            node_raw,
            morph_shared_pool,
            morph_nodes_pool,
            ik_globals,
        );
        // After the scratch: the reused bind pose, IK buffers and weighted curve names
        // (animation2-perf) replace the per-evaluation ones; `take_reuse` hands them back.
        ctx.install_reuse(reuse);
        let mut ctx = ctx
            .with_sync(sync_setup.as_deref(), sync_state)
            .with_slots(montage_begun.then_some(&slot_layers));
        let result = match &animator.source {
            AnimatorSource::Graph { graph, .. } => evaluate_graph(graph, &mut instance, &mut ctx, &mut out),
            AnimatorSource::SingleClip { clip, looping } => {
                crate::eval::evaluate_single_clip(clip, *looping, &mut instance, &mut ctx, &mut out)
            }
        };
        if let Err(error) = result {
            warn!(target: "regia_animation", "animator on {entity:?}: {error}");
        }
        let mut fired_notifies: Vec<Notify> = mem::take(&mut ctx.notifies);
        let fired_states: Vec<StateEvent> = mem::take(&mut ctx.state_events);
        let consumed: Vec<String> = mem::take(&mut ctx.consumed_triggers);
        let consumed_slots: Vec<String> = mem::take(&mut ctx.consumed_slots);
        let route = animator.root_motion_route;
        let mut root_motion = route.graph_input(ctx.root_motion);
        let sync_state = ctx.take_sync_state();
        let reuse = ctx.take_reuse();
        // `ctx` borrows the animator. Finish it before writing the curves back.
        let finished = ctx.finish();
        for ((name, value), weight) in reuse.curves.iter().zip(&reuse.curve_weights) {
            if *weight > 0.0 {
                animator.parameters.set_float(name, *value);
            }
        }
        animator.reuse = reuse;
        animator.sync_state = sync_state;
        animator.slot_layers = slot_layers;
        // Montages play over the graph's output (spec 17 § 4b); slots a graph `Slot` node
        // consumed are not blended again (spec 34 M9).
        let mut fired_montage = Vec::new();
        if let Some(mut player) = player
            && (graph_slots || player.is_active())
        {
            let frame = MontageFrame {
                skeleton: &binding.skeleton,
                graph_clips: Some(clips_of(&animator.source)),
                delta: eval_delta,
                rig: placement(),
            };
            if graph_slots {
                fired_notifies.append(&mut early_notifies);
                if montage_begun {
                    player.finish_frame(&frame, &mut out, &mut root_motion, &consumed_slots);
                }
            } else {
                player.evaluate(&frame, &mut out, &mut root_motion, &mut fired_notifies);
            }
            fired_montage = player.take_events();
            // A blending-out montage leads its sync group next frame (spec 17 § 4b).
            if animator.sync_setup.is_some() {
                for (group, leader) in player.sync_leaders() {
                    animator.sync_state.lead(group, leader.clone());
                }
            }
        }
        // Scripted warps bend the final root motion, after the montage stage (spec 34 § 5).
        if let Some(mut warps) = scripted
            && warps.is_active()
        {
            let globals = std::cell::OnceCell::new();
            let skeleton = &binding.skeleton;
            let bone_offset = |name: &str| {
                let joint = skeleton.joint_index(name)? as usize;
                let globals = globals.get_or_init(|| skeleton.globals(&out, None));
                globals
                    .get(joint)
                    .map(|m| m.transform_point3(regia_core::math::Vec3::ZERO))
            };
            root_motion = warps.apply(eval_delta, placement(), root_motion, &bone_offset);
            for event in warps.take_events() {
                warp_events.write(ScriptedWarpEvent { entity, event });
            }
        }
        for record in fired_montage {
            montage_events.write(MontageEvent {
                entity,
                id: record.id,
                montage: record.montage,
                kind: record.kind,
            });
        }
        let (reported, applied) = route.split(root_motion);
        animator.last_root_motion = reported;
        // Root motion accumulates per evaluation (LOD-skipped frames add nothing) until a
        // character controller drains it. The seconds are animation time covered, so reverse
        // playback (negative speed) counts too; consumers skip motion that covers no time.
        let covered = eval_delta.abs();
        match root_output {
            Some(mut output) => output.accumulate(applied.0, applied.1, covered),
            None => {
                let mut output = RootMotionOutput::default();
                output.accumulate(applied.0, applied.1, covered);
                commands.entity(entity).insert(output);
            }
        }
        animator.scratch = finished.scratch;
        animator.last_morphs = finished.morphs;
        animator.last_node_morphs = finished.node_morphs;
        animator.curve_sample = finished.curve_sample;
        animator.curves = finished.curves;
        animator.node_raw = finished.node_raw;
        animator.morph_shared_pool = finished.morph_shared_pool;
        animator.morph_nodes_pool = finished.morph_nodes_pool;
        animator.ik_globals = finished.ik_globals;
        animator.pose = out;
        animator.pose_dirty = true;
        animator.instance = Some(instance);

        for notify in &fired_notifies {
            notifies.write(AnimNotify {
                entity,
                name: notify.name.clone(),
                payload: notify.payload.clone(),
            });
        }
        for event in &fired_states {
            state_events.write(AnimStateEvent {
                entity,
                from: event.from.clone(),
                to: event.to.clone(),
            });
        }
        if !fired_notifies.is_empty() || !fired_states.is_empty() {
            debug!(
                target: "regia_animation",
                "animator on {entity:?}: {} notifies, {} state events",
                fired_notifies.len(),
                fired_states.len()
            );
        }
        // Triggers consumed by fired transitions auto-reset.
        for trigger in &consumed {
            animator.parameters.consume_trigger(trigger);
        }
        // Pre-IK joint positions for next frame's graph foot placement. Pose output does not
        // read them (it plants the working pose after evaluation).
        if let Some(ref mut placement) = foot_placement {
            crate::foot_ik::component::store_animated(placement, &animator.reuse.pre_ik);
        }
    }
}

/// What the graph `Slot` node check last ran against: the content it reads — the graph's `Slot`
/// node names, in first-use order (`AnimGraph::slot_names`) — together with the player's slot
/// names (rev-61 note 1). The graph `Arc`'s address is deliberately not part of the key: an
/// allocator can hand a swapped graph the same address, and an in-place edit through the
/// animator's own `Arc` (`Arc::get_mut` / `make_mut` with no other holder) keeps the address
/// while changing exactly these names. Two combinations with equal names produce equal
/// warnings, so a spurious match cannot hide one.
fn slot_check_key(
    graph: &std::sync::Arc<crate::graph::AnimGraph>,
    slots: &[crate::montage_player::SlotDef],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let names = graph.slot_names();
    names.len().hash(&mut hasher);
    for name in &names {
        name.hash(&mut hasher);
    }
    slots.len().hash(&mut hasher);
    for slot in slots {
        slot.name.hash(&mut hasher);
    }
    hasher.finish()
}

/// The clip library of a graph source; single-clip sources use a shared empty library (the
/// clip itself is evaluated directly).
fn clips_of(source: &AnimatorSource) -> &ClipLibrary {
    static EMPTY: std::sync::OnceLock<ClipLibrary> = std::sync::OnceLock::new();
    match source {
        AnimatorSource::Graph { clips, .. } => clips,
        AnimatorSource::SingleClip { .. } => EMPTY.get_or_init(ClipLibrary::new),
    }
}

/// What [`apply_animation`] reads from each rig.
type ApplyRig = (
    Entity,
    &'static SkinnedMesh,
    &'static mut Animator,
    Option<&'static mut SkinPalette>,
    Option<&'static mut JointGlobals>,
    Option<&'static PhysicsPoseBlend>,
    Option<&'static mut SpringBones>,
    Option<&'static Transform>,
    Option<&'static mut RigDrivers>,
    (
        Option<&'static MorphLayers>,
        Option<&'static MorphTargetNames>,
        Option<&'static InstanceBindPose>,
        Option<&'static mut Gaze>,
    ),
);

/// Ragdoll matrix buffers and the foot-placement queries for [`apply_animation`].
///
/// One parameter so the system stays at seven arguments (ADR-8388). A bare tuple of these
/// params is not accepted as a system argument. The type is public because
/// `apply_animation` is named from other crates; hosts do not construct it.
#[derive(SystemParam)]
pub struct FootApply<'w, 's> {
    physics_animated: Local<'s, Vec<Mat4>>,
    physics_blended: Local<'s, Vec<Mat4>>,
    placements: Query<
        'w,
        's,
        (
            &'static mut FootPlacement,
            Option<&'static MontagePlayer>,
            Has<FootPlacementRespawn>,
        ),
    >,
    reports: Query<'w, 's, &'static mut FootPlantReport>,
    statuses: Query<'w, 's, &'static mut FootPlacementStatus>,
    probe: Option<Res<'w, GroundProbes>>,
    /// Engine-side walkers (ADR-8707): stepped by `step_gait_rigs`, posed here.
    gait_rigs: Query<
        'w,
        's,
        (
            &'static mut crate::gait_rig::GaitRig,
            Option<&'static crate::gait::GaitState>,
        ),
    >,
}

/// Write the evaluated pose to joint entities, then publish the skin palette, joint globals,
/// morph weights (the animator's, composed with the rig's [`MorphLayers`]) and root-motion output
/// on the rig.
#[tracing::instrument(skip_all)]
pub fn apply_animation(
    mut commands: Commands,
    lod_camera: Option<Res<AnimLodCamera>>,
    mut rigs: Query<ApplyRig, Without<regia_scene::layers::LayerParked>>,
    mut joints: Query<&mut Transform, Without<SkinnedMesh>>,
    world_transforms: Query<(&GlobalTransform, Has<ChildOf>)>,
    // Working copies: the effects chain's pose and its retarget onto an instance bind pose.
    (mut blended, mut retargeted): (Local<Pose>, Local<Pose>),
    mut feet: FootApply,
) {
    let _span = tracing::debug_span!("apply_animation").entered();
    for (
        entity,
        binding,
        mut animator,
        palette,
        joint_globals,
        physics,
        springs,
        rig_transform,
        rig_drivers,
        (layers, names, instance, gaze),
    ) in &mut rigs
    {
        if animator.pose.len() != binding.skeleton.len() {
            continue; // not yet evaluated (added this frame or gated off)
        }
        // The evaluator did not write a new pose (LOD or visibility gate). Leave the joints
        // and the skin palette where the last evaluation put them.
        if !animator.pose_dirty {
            continue;
        }
        // Morph weights: the clip's tracks, then the morph layers, then the rig drivers add theirs.
        let morphs = crate::morph::dense_weights(&animator.last_morphs);
        let mut morphs = match layers {
            Some(layers) => layers.compose(&morphs, names, Some(&animator.parameters)),
            None => morphs,
        };
        // A ragdoll blend, the rig drivers, spring bones, eye gaze and pose-mode foot placement
        // work on a copy; the animator keeps its own pose.
        let pose_feet = feet
            .placements
            .get(entity)
            .is_ok_and(|(placement, _, _)| placement.is_pose());
        // Graph output with a tilted foot or a bent toe turns them on the copy (ADR-10130).
        let graph_feet = feet
            .placements
            .get(entity)
            .is_ok_and(|(placement, _, _)| crate::foot_ik::component::graph_turns_feet(placement));
        let ragdolled = physics.filter(|p| p.max_weight() > 0.0);
        let springy = springs.filter(|s| s.enabled);
        let walker = feet.gait_rigs.contains(entity);
        let pose = if ragdolled.is_some()
            || springy.is_some()
            || rig_drivers.is_some()
            || gaze.is_some()
            || pose_feet
            || graph_feet
            || walker
        {
            blended.clone_from(&animator.pose);
            if let Some(physics) = ragdolled {
                crate::physics_blend::blend_physics_pose_reuse(
                    &binding.skeleton,
                    &mut blended,
                    physics,
                    &mut feet.physics_animated,
                    &mut feet.physics_blended,
                );
            }
            // Foot placement after the ragdoll blend and before eye gaze (ADR-8388), so gaze,
            // drivers and springs see the planted legs. Graph output was written before
            // evaluation; only its foot tilt and toe bend are applied here (ADR-10130).
            if graph_feet && let Ok((mut placement, _, _)) = feet.placements.get_mut(entity) {
                crate::foot_ik::component::turn_graph_feet(&mut placement, &binding.skeleton, &mut blended);
            }
            if pose_feet && let Ok((mut placement, montage, respawn)) = feet.placements.get_mut(entity) {
                let rig = rig_transform.copied().unwrap_or_default();
                let ignore = placement.character.unwrap_or(entity);
                let probe = feet.probe.as_deref();
                let mut staged = FootPlantReport::default();
                let status = if let Ok(mut report) = feet.reports.get_mut(entity) {
                    crate::foot_ik::component::plant_pose(
                        &mut placement,
                        crate::foot_ik::component::PosePlant {
                            skeleton: &binding.skeleton,
                            pose: &mut blended,
                            transform: &rig,
                            animator: &animator,
                            montage,
                            probe,
                            ignore,
                            respawn,
                            report: &mut report,
                            camera: lod_camera.as_ref().map(|c| c.position),
                            phase: entity.index_u32(),
                        },
                    )
                } else {
                    let status = crate::foot_ik::component::plant_pose(
                        &mut placement,
                        crate::foot_ik::component::PosePlant {
                            skeleton: &binding.skeleton,
                            pose: &mut blended,
                            transform: &rig,
                            animator: &animator,
                            montage,
                            probe,
                            ignore,
                            respawn,
                            report: &mut staged,
                            camera: lod_camera.as_ref().map(|c| c.position),
                            phase: entity.index_u32(),
                        },
                    );
                    commands.entity(entity).insert(staged);
                    status
                };
                if let Ok(mut slot) = feet.statuses.get_mut(entity) {
                    *slot = status;
                } else {
                    commands.entity(entity).insert(status);
                }
                if respawn {
                    commands.entity(entity).remove::<FootPlacementRespawn>();
                }
            }
            // Gait rig (ADR-8707) in the same slot, after foot placement and before gaze: the
            // body joint takes the stance height and each leg is solved to its foot target.
            if walker && let Ok((mut gait_rig, gait_state)) = feet.gait_rigs.get_mut(entity) {
                let rig = rig_transform.copied().unwrap_or_default();
                crate::gait_rig::pose_gait_rig(
                    &mut gait_rig,
                    gait_state,
                    &binding.skeleton,
                    &mut blended,
                    &rig,
                );
            }
            // Eye gaze (and head follow) right after the ragdoll blend and foot placement, so the twist, helper and
            // corrective drivers and head-rooted spring chains see the gazed head and eyes (the
            // eyes are absolute; the lids compose on the incoming lid rotation). Its eye-look
            // channels go into the morph weights; drivers add theirs after. A rig with its own
            // proportions is solved in them.
            if let Some(mut gaze) = gaze {
                let rig = rig_transform.copied().unwrap_or_default();
                // A root entity's own `Transform` is current this frame; a child's world position
                // comes from its (last propagated) `GlobalTransform`.
                let target = crate::gaze::gaze_target_world(gaze.target, |e| {
                    let global = world_transforms.get(e).ok();
                    let root = joints
                        .get(e)
                        .ok()
                        .filter(|_| !global.is_some_and(|(_, child)| child));
                    root.map(|t| t.translation)
                        .or_else(|| global.map(|(g, _)| g.0.translation.into()))
                });
                crate::gaze::apply_gaze(
                    &mut gaze,
                    &binding.skeleton,
                    instance,
                    &mut blended,
                    &mut morphs,
                    &rig,
                    target,
                    // The saccade steps by the time the clip just consumed: on a gated frame
                    // the pose is frozen and the eyes must not drift ahead of it
                    // (audit-v040 L10).
                    animator.last_eval_delta,
                );
            }
            // Twist, helpers, drivers and pose-space correctives follow the animated (ragdoll-
            // blended, gazed) pose and add to the morph weights.
            if let Some(mut drivers) = rig_drivers {
                let RigDrivers { rig, state } = &mut *drivers;
                rig.evaluate(&mut blended, &animator.parameters, &mut morphs, state);
            }
            // Secondary motion runs last: after the animation, any ragdoll blend and the rig
            // drivers, before the pose reaches the joint entities and the skin palette.
            if let Some(mut springs) = springy {
                let rig = rig_transform.copied().unwrap_or_default();
                let distance = lod_camera.as_ref().map(|c| rig.translation.distance(c.position));
                // The spring covers exactly the animation time the clip just consumed (0 on a
                // gated frame), never a different span than the pose it hangs from
                // (audit-v040 M4).
                springs.update(
                    &binding.skeleton,
                    &mut blended,
                    &rig,
                    animator.last_eval_delta,
                    distance,
                );
            }
            &*blended
        } else {
            &animator.pose
        };
        // Per-instance proportions: the pose is authored against the shared skeleton and moves
        // onto the instance's own rest pose here (a mismatched instance is ignored).
        let instance = instance.filter(|i| i.len() == binding.skeleton.len());
        let pose = match instance {
            Some(instance) => {
                retarget_to_instance(&binding.skeleton, instance, pose, &mut retargeted);
                &*retargeted
            }
            None => pose,
        };
        // Local TRS: joint entities are not parented (regia_scene has no hierarchy yet).
        // The model-space palette is what the renderer skins with.
        for (i, &joint_entity) in binding.joints.iter().enumerate() {
            let Ok(mut joint_transform) = joints.get_mut(joint_entity) else {
                continue;
            };
            if i >= pose.len() {
                break; // more joint entities than skeleton joints: ignore the extras
            }
            joint_transform.translation = pose.translations[i];
            joint_transform.rotation = pose.rotations[i].normalize();
            joint_transform.scale = pose.scales[i];
        }

        publish_skin(
            &mut commands,
            entity,
            &binding.skeleton,
            pose,
            instance,
            palette,
            joint_globals,
        );
        commands.entity(entity).insert(MorphWeights(morphs));
        animator.pose_dirty = false;
    }
}

/// Write the skin palette and joint globals into the components already on the rig.
///
/// The first frame inserts them. Later frames refill the existing vectors, so a palette that
/// has spare capacity keeps its allocation.
fn publish_skin(
    commands: &mut Commands,
    entity: Entity,
    skeleton: &crate::skeleton::Skeleton,
    pose: &Pose,
    instance: Option<&InstanceBindPose>,
    palette: Option<Mut<'_, SkinPalette>>,
    joint_globals: Option<Mut<'_, JointGlobals>>,
) {
    match (palette, joint_globals) {
        (Some(mut palette), Some(mut globals)) => {
            skeleton.globals_into(pose, None, &mut globals.globals);
            match instance {
                Some(instance) => {
                    instance_palette_into(instance, &globals.globals, &mut palette.matrices);
                }
                None => skeleton.skin_palette_into(&globals.globals, &mut palette.matrices),
            }
        }
        (Some(mut palette), None) => {
            let mut globals = Vec::new();
            skeleton.globals_into(pose, None, &mut globals);
            match instance {
                Some(instance) => instance_palette_into(instance, &globals, &mut palette.matrices),
                None => skeleton.skin_palette_into(&globals, &mut palette.matrices),
            }
            commands.entity(entity).insert(JointGlobals { globals });
        }
        (None, Some(mut globals)) => {
            skeleton.globals_into(pose, None, &mut globals.globals);
            let matrices = match instance {
                Some(instance) => instance_palette(instance, &globals.globals),
                None => skeleton.skin_palette(&globals.globals),
            };
            commands.entity(entity).insert(SkinPalette { matrices });
        }
        (None, None) => {
            let globals = skeleton.globals(pose, None);
            let matrices = match instance {
                Some(instance) => instance_palette(instance, &globals),
                None => skeleton.skin_palette(&globals),
            };
            commands
                .entity(entity)
                .insert(SkinPalette { matrices })
                .insert(JointGlobals { globals });
        }
    }
}

/// What [`resolve_sockets`] reads from each socket entity: the socket and its parent, if any.
type SocketEntity = (Entity, &'static Socket, Option<&'static ChildOf>);

/// Socket entities [`resolve_sockets`] places: those with a `Transform` that are not rigs.
type SocketFilter = (With<Transform>, Without<SkinnedMesh>);

/// An entity's placement as [`resolve_sockets`] reads (rigs, socket parents) and writes
/// (socket entities) it.
type Placement = (
    &'static mut Transform,
    Option<&'static GlobalTransform>,
    Has<ChildOf>,
);

/// The world matrix of an entity as `resolve_sockets` sees it: a root's own `Transform`
/// (current this frame), a child's last propagated `GlobalTransform` (its `Transform` is
/// relative to the parent). The same rule as the rig placement in [`evaluate_animators`].
fn world_matrix(transform: &Transform, global: Option<&GlobalTransform>, is_child: bool) -> Mat4 {
    match global.filter(|_| is_child) {
        Some(global) => Mat4::from(global.0),
        None => {
            Mat4::from_scale_rotation_translation(transform.scale, transform.rotation, transform.translation)
        }
    }
}

/// Resolve [`Socket`] entities to their joint's world transform and refresh the
/// [`SocketTargets`] map used by IK nodes.
///
/// The map holds each socket in its rig's object space (`joint_global · offset`), the space
/// the IK nodes solve in. The socket entity's `Transform` gets the world point
/// `rig_world · joint_global · offset`, expressed relative to the entity's parent when it has
/// one (deep-zai-animation finding 1).
#[tracing::instrument(skip_all)]
pub fn resolve_sockets(
    mut socket_targets: ResMut<SocketTargets>,
    rigs: Query<(&SkinnedMesh, &JointGlobals)>,
    sockets: Query<SocketEntity, SocketFilter>,
    mut transforms: Query<Placement>,
) {
    let _span = tracing::debug_span!("resolve_sockets").entered();
    socket_targets.0.clear();
    for (socket_entity, socket, socket_parent) in &sockets {
        let Ok((binding, globals)) = rigs.get(socket.rig) else {
            continue;
        };
        let Ok((rig_transform, rig_global, rig_is_child)) = transforms.get(socket.rig) else {
            continue;
        };
        let rig_matrix = world_matrix(rig_transform, rig_global, rig_is_child);
        let Some(joint) = binding.skeleton.joint_index(&socket.joint) else {
            warn!(
                target: "regia_animation",
                "socket {:?} references missing joint {:?}",
                socket.name, socket.joint
            );
            continue;
        };
        // The palette can be older than the skeleton (a gated frame, or a swapped skeleton).
        let Some(joint_global) = globals.globals.get(joint as usize) else {
            continue;
        };
        let offset = Mat4::from_scale_rotation_translation(
            socket.offset.scale,
            socket.offset.rotation,
            socket.offset.translation,
        );
        let object = *joint_global * offset;
        // IK targets live in the rig's object space, never the world's.
        socket_targets
            .0
            .insert((socket.rig, socket.name.clone()), object.w_axis.truncate());
        // The socket entity's `Transform` is local to its parent: an unparented socket takes
        // the world matrix, a child of the rig the object-space matrix as is (the rig's world
        // matrix composes once, in propagation), a child of anything else the world matrix in
        // that parent's frame. The world product keeps the order it always had, so an
        // unparented socket keeps its exact bits (the determinism traces hash it).
        let world = || rig_matrix * *joint_global * offset;
        let local = match socket_parent.map(ChildOf::parent) {
            None => world(),
            Some(parent) if parent == socket.rig => object,
            Some(parent) => {
                let parent_world = match transforms.get(parent) {
                    Ok((transform, global, is_child)) => world_matrix(transform, global, is_child),
                    // A parent without a transform composes as identity.
                    Err(_) => Mat4::IDENTITY,
                };
                if parent_world.determinant().abs() < f32::EPSILON {
                    continue; // a zero-scale parent has no frame to express the socket in
                }
                parent_world.inverse() * world()
            }
        };
        let Ok((mut transform, _, _)) = transforms.get_mut(socket_entity) else {
            continue;
        };
        let (scale, rotation, translation) = local.to_scale_rotation_translation();
        transform.translation = translation;
        transform.rotation = rotation.normalize();
        transform.scale = scale;
    }
}

/// Register the animation systems (called by [`crate::AnimationPlugin`]).
pub(crate) fn register(app: &mut regia_app::App) {
    app.register_type::<crate::live::FeedPoseTarget>()
        .register_type::<crate::foot_ik::FootPlantReport>()
        .register_type::<crate::foot_ik::FootPlantSettings>()
        .register_type::<FootPlacement>()
        .register_type::<crate::foot_ik::FootLegDesc>()
        .register_type::<crate::foot_ik::FootPlacementOutput>()
        .register_type::<FootPlacementStatus>()
        .register_type::<FootPlacementRespawn>()
        .register_type::<crate::foot_ik::FootShapeSettings>()
        .register_type::<Option<crate::foot_ik::FootShapeSettings>>()
        .add_message::<AnimNotify>()
        .add_message::<AnimStateEvent>()
        .add_message::<MontageEvent>()
        .add_message::<ScriptedWarpEvent>()
        .add_message::<crate::montage_command::MontageCommandRecorded>()
        .add_message::<crate::montage_command::ApplyMontageCommand>()
        .init_resource::<SocketTargets>()
        .add_systems(
            Update,
            (
                crate::montage_command::apply_montage_commands,
                prepare_foot_placement,
                evaluate_animators,
                crate::montage_command::publish_montage_commands,
            )
                .chain(),
        )
        .add_systems(Update, crate::morph::advance_node_morph_players)
        .add_systems(
            PostUpdate,
            (
                crate::live::apply_feed_pose_targets,
                apply_animation,
                resolve_sockets,
            )
                .chain()
                .in_set(PostUpdateSet::AnimationApply),
        )
        .add_systems(
            PostUpdate,
            crate::morph::apply_morph_layers.in_set(PostUpdateSet::AnimationApply),
        )
        .add_systems(
            PostUpdate,
            crate::morph::apply_node_morphs
                .after(apply_animation)
                .after(crate::morph::apply_morph_layers)
                .in_set(PostUpdateSet::AnimationApply),
        )
        .add_systems(
            PostUpdate,
            (
                crate::node_driver::sample_plain_node_clips,
                crate::node_driver::apply_node_drives,
            )
                .chain()
                .after(apply_animation)
                .in_set(PostUpdateSet::AnimationApply),
        );
}
