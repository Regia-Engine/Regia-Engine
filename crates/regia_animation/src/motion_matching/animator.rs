// SPDX-License-Identifier: MPL-2.0
//! Motion matching through the [`Animator`] (`docs/specs/52-motion-matching.md` § 3 set ordering;
//! ADR-2409): a motion-matching stage on top of the animator's graph.
//!
//! An entity with an [`Animator`], a [`MotionMatching`], a [`MotionMatchPlayer`] and an
//! [`AnimatorMotionMatch`] plays the player's pose through its animator: after the graph
//! evaluates, [`drive_animators_from_motion_matching`] blends the player's output over the
//! graph's pose with the stage's weight, so everything downstream of the animator (sockets, IK
//! sockets, the joint writes, the skin palette, physics blends) sees motion matching. The player
//! itself runs where [`super::MotionMatchPlugin`] puts it (before the animators); the stage only
//! routes its result, so the pose an animator shows at weight 1 is exactly the player's pose.
//!
//! The weight works like a graph state: with `states` set, the stage is on while the root state
//! machine's current state is one of them (a `Locomotion` state owns the body; a `Jump` state
//! plays its own clip), fading over `fade` seconds (linear, frame-step deterministic). An optional
//! float or bool graph parameter multiplies it (`weight_param`). Root motion: the player's
//! [`super::player::RootMotionDelta`], scaled by the weight, is added to the animator's root
//! motion through its [`crate::components::RootMotionRoute`]; set the route's graph input off
//! (`OnlyMontages` or `Ignore`) when the graph's own clips carry root motion you do not want.

use regia_ecs::prelude::*;

use super::MotionMatching;
use super::player::{MotionMatchPlayer, RootMotionDelta};
use crate::components::{Animator, RootMotionOutput};
use crate::num::det_rotation_y;

/// Plays a [`MotionMatchPlayer`] through the entity's [`Animator`] (see the module docs).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct AnimatorMotionMatch {
    /// States of the animator's root state machine in which the stage is on; empty = always on.
    pub states: Vec<String>,
    /// A float (clamped to 0..1) or bool graph parameter that multiplies the weight; empty = 1.
    pub weight_param: String,
    /// Weight at full strength, 0..1 (default 1).
    pub weight: f32,
    /// Seconds to fade in or out when the state changes (default 0.2; 0 switches at once).
    pub fade: f32,
    /// Add the player's root motion to the animator's (default true).
    pub root_motion: bool,
    /// The weight applied on the latest frame (runtime).
    #[reflect(ignore)]
    pub current: f32,
    /// Whether the stage has run once (the first frame starts at its target, without a fade).
    #[reflect(ignore)]
    pub started: bool,
}

impl Default for AnimatorMotionMatch {
    fn default() -> Self {
        Self {
            states: Vec::new(),
            weight_param: String::new(),
            weight: 1.0,
            fade: 0.2,
            root_motion: true,
            current: 0.0,
            started: false,
        }
    }
}

impl AnimatorMotionMatch {
    /// A stage that is on while the root state machine is in one of `states`.
    pub fn in_states<I, S>(states: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            states: states.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// The weight the stage moves toward this frame for `animator`.
    pub fn target(&self, animator: &Animator) -> f32 {
        let base = if self.weight.is_finite() {
            self.weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let in_state = self.states.is_empty()
            || animator
                .current_state()
                .is_some_and(|state| self.states.contains(&state));
        if !in_state {
            return 0.0;
        }
        let param = if self.weight_param.is_empty() {
            1.0
        } else {
            match animator.parameters.get(&self.weight_param) {
                Some(crate::params::ParamValue::Float(v)) if v.is_finite() => v.clamp(0.0, 1.0),
                Some(crate::params::ParamValue::Bool(b)) => f32::from(u8::from(b)),
                Some(crate::params::ParamValue::Int(i)) => (i as f32).clamp(0.0, 1.0),
                _ => 0.0,
            }
        };
        base * param
    }

    /// Advance the faded weight by `dt` toward `target` and return it.
    pub fn advance(&mut self, target: f32, dt: f32) -> f32 {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        if !self.started || !(self.fade.is_finite() && self.fade > 0.0) {
            self.started = true;
            self.current = target;
            return target;
        }
        let step = dt / self.fade;
        self.current = if self.current < target {
            (self.current + step).min(target)
        } else {
            (self.current - step).max(target)
        };
        self.current
    }
}

/// What [`drive_animators_from_motion_matching`] reads.
type MotionMatchRig = (
    &'static mut Animator,
    &'static mut MotionMatchPlayer,
    &'static mut AnimatorMotionMatch,
    Option<&'static mut RootMotionOutput>,
    Has<MotionMatching>,
);

/// The Animator hook (registered by [`super::MotionMatchPlugin`] in `Update`, after
/// [`crate::systems::evaluate_animators`]): blend each player's pose over its animator's
/// evaluated pose with the stage weight and add the weighted root motion.
#[tracing::instrument(target = "regia_animation", name = "motion_matching.animator", skip_all)]
pub fn drive_animators_from_motion_matching(
    time: Res<regia_app::Time>,
    mut rigs: Query<MotionMatchRig, Without<regia_scene::layers::LayerParked>>,
) {
    let dt = time.delta_secs();
    for (mut animator, mut player, mut stage, root_output, has_matcher) in &mut rigs {
        if !has_matcher {
            continue;
        }
        let target = stage.target(&animator);
        let weight = stage.advance(target, dt);
        let root_delta = player.take_pending_root_motion();
        apply_stage_with_root_delta(
            &mut animator,
            &player,
            weight,
            stage.root_motion,
            root_output.map(|o| o.into_inner()),
            root_delta,
        );
    }
}

/// One rig's stage at `weight` (what [`drive_animators_from_motion_matching`] does per entity;
/// public so hosts and tests can drive it without the plugin).
///
/// This applies only the root movement of the player's *latest* advance
/// ([`MotionMatchPlayer::root_motion`], one fixed substep). The plugin instead drains every
/// substep since the previous presentation call (an internal accumulation), so a host that
/// advances the player several fixed times per frame and calls this each frame under-counts
/// the root motion — and never drains that accumulation. Driving the player from a host's
/// own schedule needs the engine's drain, which is not public yet (see the lane report).
pub fn apply_stage(
    animator: &mut Animator,
    player: &MotionMatchPlayer,
    weight: f32,
    root_motion: bool,
    root_output: Option<&mut RootMotionOutput>,
) {
    apply_stage_with_root_delta(
        animator,
        player,
        weight,
        root_motion,
        root_output,
        player.root_motion(),
    );
}

/// Apply one presentation stage with the root deltas accumulated since its previous call.
fn apply_stage_with_root_delta(
    animator: &mut Animator,
    player: &MotionMatchPlayer,
    weight: f32,
    root_motion: bool,
    root_output: Option<&mut RootMotionOutput>,
    root_delta: RootMotionDelta,
) {
    if !(weight.is_finite() && weight > 0.0) {
        return;
    }
    let weight = weight.min(1.0);
    let source = player.pose();
    if source.is_empty() {
        return;
    }
    let pose = animator.evaluated_pose_mut();
    if pose.len() != source.len() {
        // The animator has not evaluated yet (or animates another skeleton): show the player.
        if pose.is_empty() && weight >= 1.0 {
            pose.clone_from(source);
        }
        return;
    }
    if weight >= 1.0 {
        pose.clone_from(source);
    } else {
        pose.blend_from(source, weight);
    }
    if !root_motion {
        return;
    }
    let translation = root_delta.translation * weight;
    let rotation = det_rotation_y(root_delta.yaw * weight);
    let (reported, applied) = animator.root_motion_route.split((translation, rotation));
    let (last_t, last_r) = animator.last_root_motion;
    animator.last_root_motion = (last_t + last_r * reported.0, (last_r * reported.1).normalize());
    if let Some(output) = root_output {
        // The graph's evaluation already counted this frame's seconds.
        output.accumulate(applied.0, applied.1, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fade_is_linear_and_starts_at_its_target() {
        let mut stage = AnimatorMotionMatch {
            fade: 0.5,
            ..AnimatorMotionMatch::default()
        };
        assert_eq!(stage.advance(1.0, 0.1), 1.0, "first frame: no fade in");
        assert!((stage.advance(0.0, 0.1) - 0.8).abs() < 1e-6);
        assert!((stage.advance(0.0, 0.25) - 0.3).abs() < 1e-6);
        assert_eq!(stage.advance(0.0, 1.0), 0.0, "clamped at the target");
        assert!(
            (stage.advance(1.0, f32::NAN) - 0.0).abs() < 1e-6,
            "a NaN step holds"
        );
    }

    #[test]
    fn the_target_follows_the_parameter() {
        let mut animator = Animator::single(crate::clip::AnimationClip::empty("idle", 1.0), true);
        let mut stage = AnimatorMotionMatch {
            weight_param: "mm".into(),
            ..AnimatorMotionMatch::default()
        };
        assert_eq!(stage.target(&animator), 0.0, "an absent parameter is off");
        animator.set("mm", 0.25f32);
        assert_eq!(stage.target(&animator), 0.25);
        stage.weight_param = "mm_on".into();
        animator.set("mm_on", true);
        assert_eq!(stage.target(&animator), 1.0);
        stage.weight = f32::NAN;
        assert_eq!(stage.target(&animator), 0.0, "a NaN weight is off");
        let state_gated = AnimatorMotionMatch::in_states(["Locomotion"]);
        assert_eq!(state_gated.target(&animator), 0.0, "a single clip has no state");
    }
}
