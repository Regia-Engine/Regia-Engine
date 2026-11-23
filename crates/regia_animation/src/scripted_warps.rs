// SPDX-License-Identifier: MPL-2.0
//! Script-authored motion warps: bend a graph clip's root motion toward a target for a while,
//! with no montage involved (`docs/specs/34-montage-extensions.md` § 5, M3).
//!
//! A [`ScriptedWarp`] starts the frame it is added and runs for `duration` seconds of animator
//! time (the frame deltas the evaluate system passes, so it is deterministic and pauses with the
//! animator), then removes itself and records [`WarpEvent::Finished`]. The corrector is the one
//! montage windows use ([`crate::warp::warp_frame`]); it runs on the final root motion after
//! the montage stage, so a montage window is applied first and the scripted warp acts on its
//! result.
//!
//! A graph clip has no authored layout to look ahead in, so the root motion still to come in
//! the window is extrapolated from this frame's motion at the same speed. That is exact for a
//! constant-speed dash and converges for anything else, because the correction is recomputed
//! from the rig's current position every frame and the last frame (no time left) takes whatever
//! offset remains.

use regia_core::math::{Quat, Vec3, det};
use regia_ecs::prelude::*;

use crate::montage_player::RigPlacement;
use crate::warp::{
    RotationKind, WarpAnchor, WarpInput, WarpMode, WarpTarget, compose_root, invert_root, warp_frame,
    wrap_angle, yaw_of, yaw_toward,
};

/// Why a scripted warp was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WarpError {
    /// `duration` was zero, negative or not finite (`warp_bad_duration`).
    #[error("warp_bad_duration: duration must be finite and positive")]
    BadDuration,
    /// The id or the target name was empty (`warp_bad_name`).
    #[error("warp_bad_name: id and target must not be empty")]
    BadName,
    /// `rotation_fraction` was outside `(0, 1]` (`warp_bad_fraction`).
    #[error("warp_bad_fraction: rotation_fraction must be in (0, 1]")]
    BadFraction,
}

/// A scripted warp request.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptedWarp {
    /// Warp id: adding an id that exists replaces (restarts) it.
    pub id: String,
    /// Name of the warp target ([`ScriptedWarps::set_target`]).
    pub target: String,
    /// Seconds of animator time the warp runs.
    pub duration: f32,
    /// Path mapping.
    pub mode: WarpMode,
    /// Warp the horizontal translation.
    pub translation: bool,
    /// Also warp the height.
    pub vertical: bool,
    /// Turn the rig to the target.
    pub rotation: bool,
    /// Which facing the rotation turns to.
    pub rotation_kind: RotationKind,
    /// The turn completes when this fraction `(0, 1]` of the duration has elapsed.
    pub rotation_fraction: f32,
    /// What lands on the target at the end. `Bone` uses the bone's offset in the pose of the
    /// frame the warp is applied on (the supplied lookup).
    pub anchor: WarpAnchor,
}

impl ScriptedWarp {
    /// A translation warp toward `target` over `duration` seconds, every option at its default.
    pub fn new(id: impl Into<String>, target: impl Into<String>, duration: f32) -> Self {
        ScriptedWarp {
            id: id.into(),
            target: target.into(),
            duration,
            mode: WarpMode::default(),
            translation: true,
            vertical: false,
            rotation: false,
            rotation_kind: RotationKind::default(),
            rotation_fraction: 1.0,
            anchor: WarpAnchor::default(),
        }
    }

    fn validate(&self) -> Result<(), WarpError> {
        if !(self.duration.is_finite() && self.duration > 0.0) {
            return Err(WarpError::BadDuration);
        }
        if self.id.is_empty() || self.target.is_empty() {
            return Err(WarpError::BadName);
        }
        if !(self.rotation_fraction.is_finite()
            && self.rotation_fraction > 0.0
            && self.rotation_fraction <= 1.0)
        {
            return Err(WarpError::BadFraction);
        }
        Ok(())
    }
}

/// What a scripted warp reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarpEvent {
    /// The warp ran its full duration and removed itself.
    Finished {
        /// The warp's id.
        id: String,
    },
}

/// A scripted warp finishing, written as an ECS message by the evaluate system.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct ScriptedWarpEvent {
    /// The rig entity.
    pub entity: Entity,
    /// What happened.
    pub event: WarpEvent,
}

/// A listed scripted warp.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptedWarpStatus {
    /// The request.
    pub warp: ScriptedWarp,
    /// Seconds of animator time elapsed.
    pub elapsed: f32,
    /// `false` while [`ScriptedWarps::pause_all`] holds it.
    pub enabled: bool,
}

#[derive(Clone, Debug)]
struct Entry {
    warp: ScriptedWarp,
    elapsed: f32,
    enabled: bool,
    warned_no_motion: bool,
}

/// The scripted warps of one rig, and the warp targets they aim at.
#[derive(Component, Clone, Debug, Default)]
pub struct ScriptedWarps {
    entries: Vec<Entry>,
    targets: Vec<(String, WarpTarget)>,
    events: Vec<WarpEvent>,
}

impl ScriptedWarps {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start `warp` now. An existing warp with the same id is replaced and restarts with its
    /// full duration.
    ///
    /// # Errors
    ///
    /// [`WarpError`] for a bad duration, name or rotation fraction; nothing is changed.
    pub fn add(&mut self, warp: ScriptedWarp) -> Result<(), WarpError> {
        warp.validate()?;
        let entry = Entry {
            warp,
            elapsed: 0.0,
            enabled: true,
            warned_no_motion: false,
        };
        match self.entries.iter_mut().find(|e| e.warp.id == entry.warp.id) {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
        Ok(())
    }

    /// Remove warp `id`; returns whether it existed. No event is recorded.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.warp.id != id);
        before != self.entries.len()
    }

    /// Turn every warp off without deleting it (time stops too).
    pub fn pause_all(&mut self) {
        for entry in &mut self.entries {
            entry.enabled = false;
        }
    }

    /// Turn every warp back on.
    pub fn resume_all(&mut self) {
        for entry in &mut self.entries {
            entry.enabled = true;
        }
    }

    /// The warps, in the order they were added.
    #[must_use]
    pub fn list(&self) -> Vec<ScriptedWarpStatus> {
        self.entries
            .iter()
            .map(|e| ScriptedWarpStatus {
                warp: e.warp.clone(),
                elapsed: e.elapsed,
                enabled: e.enabled,
            })
            .collect()
    }

    /// Whether any warp is listed.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Set (or move) warp target `name`.
    pub fn set_target(&mut self, name: impl Into<String>, target: WarpTarget) {
        let name = name.into();
        match self.targets.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = target,
            None => self.targets.push((name, target)),
        }
    }

    /// Remove warp target `name`; returns whether it was set.
    pub fn clear_target(&mut self, name: &str) -> bool {
        let before = self.targets.len();
        self.targets.retain(|(n, _)| n != name);
        before != self.targets.len()
    }

    /// Warp target `name`, if set.
    #[must_use]
    pub fn target(&self, name: &str) -> Option<WarpTarget> {
        self.targets.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
    }

    /// Re-expresses the world-space warp targets after an origin rebase (spec 78 § 12.2):
    /// `shift` is subtracted from every target position.
    pub fn shift_targets(&mut self, shift: Vec3) {
        for (_, target) in &mut self.targets {
            target.position -= shift;
        }
    }

    /// Every warp target set on this rig, in the order they were set.
    #[must_use]
    pub fn targets(&self) -> &[(String, WarpTarget)] {
        &self.targets
    }

    /// Drain the events recorded since the last call.
    pub fn take_events(&mut self) -> Vec<WarpEvent> {
        std::mem::take(&mut self.events)
    }

    /// Apply every enabled warp to this frame's final root motion `root` (rig object space).
    ///
    /// `dt` is the animator time the frame covers. `bone_offset(name)` gives a bone's position
    /// relative to the rig origin in the frame's pose (used by `Bone` anchors; `None` falls
    /// back to the origin). Warps whose target is not set pass the motion through and still
    /// consume time. Finished warps are removed and recorded.
    #[tracing::instrument(skip_all, name = "scripted_warps")]
    pub fn apply(
        &mut self,
        dt: f32,
        rig: RigPlacement,
        root: (Vec3, Quat),
        bone_offset: &dyn Fn(&str) -> Option<Vec3>,
    ) -> (Vec3, Quat) {
        let dt = crate::num::finite_or(dt, 0.0).max(0.0);
        if dt <= 0.0 {
            return root;
        }
        let mut root = root;
        let mut entries = std::mem::take(&mut self.entries);
        for entry in &mut entries {
            if !entry.enabled {
                continue;
            }
            let frame_time = dt.min(entry.warp.duration - entry.elapsed);
            if frame_time > 0.0
                && let Some(target) = self.target(&entry.warp.target)
            {
                if !entry.warned_no_motion && entry.warp.translation && root.0.length_squared() <= 1e-12 {
                    entry.warned_no_motion = true;
                    tracing::warn!(
                        target: "regia_animation",
                        "warp_no_root_motion: scripted warp {:?} sees no root motion; the offset is spread linearly",
                        entry.warp.id
                    );
                }
                let anchor = match &entry.warp.anchor {
                    WarpAnchor::Root => Vec3::ZERO,
                    WarpAnchor::Bone(name) => bone_offset(name).unwrap_or(Vec3::ZERO),
                    WarpAnchor::Offset(v) => Vec3::from_array(*v),
                };
                root = if frame_time < dt {
                    // The warp ends inside this frame: only its share of the frame is warped
                    // and the rest passes through (ADR-2604, spec 34 § 4a). Graph root motion
                    // has no authored timeline to cut, so the frame is split at constant
                    // velocity; the two parts compose back to the whole frame.
                    let share = frame_time / dt;
                    let inside = (root.0 * share, det::quat_slerp(Quat::IDENTITY, root.1, share));
                    let after = compose_root(invert_root(inside), root);
                    let warped = warp_one(
                        &entry.warp,
                        entry.elapsed,
                        frame_time,
                        target,
                        rig,
                        inside,
                        anchor,
                    );
                    compose_root(warped, after)
                } else {
                    warp_one(&entry.warp, entry.elapsed, frame_time, target, rig, root, anchor)
                };
            }
            entry.elapsed += dt;
        }
        for entry in &entries {
            if entry.elapsed >= entry.warp.duration - 1e-6 {
                self.events.push(WarpEvent::Finished {
                    id: entry.warp.id.clone(),
                });
            }
        }
        entries.retain(|e| e.elapsed < e.warp.duration - 1e-6);
        self.entries = entries;
        root
    }
}

/// One scripted warp's correction of this frame's `root`.
fn warp_one(
    warp: &ScriptedWarp,
    elapsed: f32,
    frame_time: f32,
    target: WarpTarget,
    rig: RigPlacement,
    root: (Vec3, Quat),
    anchor: Vec3,
) -> (Vec3, Quat) {
    let rest_time = (warp.duration - elapsed - frame_time).max(0.0);
    // Constant-speed extrapolation of the motion still to come (see the module docs): the
    // rotation delta scales the same way the translation does. Scaling the axis-angle keeps
    // the constant rate for ratios above 1, where a slerp would clamp to a single frame and
    // under-count the yaw still to come (correctness audit, finding 1).
    let ratio = rest_time / frame_time;
    let scaled_axis = det::quat_to_scaled_axis(root.1) * ratio;
    let angle = scaled_axis.length();
    let rest_rot = if scaled_axis.is_finite() && angle > 1e-8 {
        det::quat_from_axis_angle(scaled_axis / angle, wrap_angle(angle))
    } else {
        Quat::IDENTITY
    };
    let rest = (root.0 * ratio, rest_rot);
    let guard = |s: f32| if s.abs() > 1e-6 { s } else { 1.0 };
    let scale = Vec3::new(guard(rig.scale.x), guard(rig.scale.y), guard(rig.scale.z));
    let rig_yaw = yaw_of(rig.rotation);
    let mut offset = target.position - rig.position;
    let desired_yaw = |offset: Vec3| -> Option<f32> {
        let facing = target.facing.map(yaw_of);
        let desired = match warp.rotation_kind {
            RotationKind::Auto => facing.or_else(|| yaw_toward(offset)),
            RotationKind::MatchTarget => facing,
            RotationKind::FaceTarget => yaw_toward(offset),
        };
        desired.map(|d| wrap_angle(d - rig_yaw))
    };
    if anchor != Vec3::ZERO {
        // Aim the origin at the target minus the anchor's offset in the final orientation; two
        // passes because the orientation depends on the aim (spec 34 § 4, rule 2).
        let authored_yaw = yaw_of(root.1 * rest.1);
        for _ in 0..2 {
            let turn = if warp.rotation {
                desired_yaw(offset).unwrap_or(authored_yaw)
            } else {
                authored_yaw
            };
            let final_rotation = rig.rotation * det::quat_from_rotation_y(turn);
            offset = target.position - rig.position - final_rotation * (scale * anchor);
        }
    }
    let to_target = (rig.rotation.inverse() * offset) / scale;
    let yaw_to_target = if warp.rotation { desired_yaw(offset) } else { None };
    let (rotation_frame_time, rotation_rest_time) = if warp.rotation_fraction >= 1.0 {
        (frame_time, rest_time)
    } else {
        let rotation_end = warp.duration * warp.rotation_fraction;
        let (from, to) = (elapsed, elapsed + frame_time);
        (
            (rotation_end.min(to) - from).max(0.0),
            (rotation_end - to).max(0.0),
        )
    };
    warp_frame(&WarpInput {
        frame: root,
        rest,
        to_target,
        yaw_to_target,
        frame_time,
        rest_time,
        horizontal: warp.translation,
        vertical: warp.vertical,
        mode: warp.mode,
        rotation_frame_time,
        rotation_rest_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_BONE: &dyn Fn(&str) -> Option<Vec3> = &|_| None;

    /// Runs a dash of `authored` metres forward (−Z) over `duration` with `steps` equal frames
    /// and returns the rig's end position; `placed` is where the rig stands (the rig moves by
    /// the warped motion each frame).
    fn dash(warps: &mut ScriptedWarps, authored: f32, duration: f32, steps: usize) -> Vec3 {
        let dt = duration / steps as f32;
        let per_frame = Vec3::new(0.0, 0.0, -authored / steps as f32);
        let mut position = Vec3::ZERO;
        for _ in 0..steps {
            let rig = RigPlacement {
                position,
                ..RigPlacement::default()
            };
            let (t, _) = warps.apply(dt, rig, (per_frame, Quat::IDENTITY), NO_BONE);
            position += t;
        }
        position
    }

    fn with_target(target: Vec3, duration: f32) -> ScriptedWarps {
        let mut warps = ScriptedWarps::new();
        warps.set_target("enemy", WarpTarget::at(target));
        warps
            .add(ScriptedWarp::new("dash", "enemy", duration))
            .expect("valid warp");
        warps
    }

    #[test]
    fn a_dash_ends_on_a_target_further_than_its_authored_motion() {
        // 3 m of authored forward motion, a target 5 m away, off to one side.
        let target = Vec3::new(0.0, 0.0, -5.0);
        let mut warps = with_target(target, 1.0);
        let end = dash(&mut warps, 3.0, 1.0, 20);
        assert!((end - target).length() < 0.01, "{end}");
        assert!(!warps.is_active(), "the warp removes itself");
        assert_eq!(
            warps.take_events(),
            vec![WarpEvent::Finished { id: "dash".into() }]
        );
    }

    #[test]
    fn the_end_position_does_not_depend_on_the_step_count() {
        let target = Vec3::new(1.0, 0.0, -4.0);
        let ends: Vec<Vec3> = [30usize, 60, 144]
            .iter()
            .map(|&steps| dash(&mut with_target(target, 1.0), 3.0, 1.0, steps))
            .collect();
        for end in &ends {
            assert!((*end - target).length() < 0.01, "{end}");
            assert!((*end - ends[0]).length() < 0.001, "{end} vs {}", ends[0]);
        }
    }

    #[test]
    fn pause_leaves_the_rest_of_the_motion_unwarped() {
        let target = Vec3::new(0.0, 0.0, -5.0);
        let mut warps = with_target(target, 1.0);
        let per_frame = Vec3::new(0.0, 0.0, -0.15);
        let rig = RigPlacement::default();
        warps.apply(0.05, rig, (per_frame, Quat::IDENTITY), NO_BONE);
        warps.pause_all();
        let (t, _) = warps.apply(0.05, rig, (per_frame, Quat::IDENTITY), NO_BONE);
        assert_eq!(t, per_frame);
        let listed = warps.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].enabled);
        assert!(
            (listed[0].elapsed - 0.05).abs() < 1e-6,
            "paused time does not run"
        );
        warps.resume_all();
        let (t, _) = warps.apply(0.05, rig, (per_frame, Quat::IDENTITY), NO_BONE);
        assert_ne!(t, per_frame);
    }

    #[test]
    fn rotation_warp_holds_the_facing_when_the_target_matches_it() {
        // Authored root motion yaws +0.2 rad per frame; the target's facing equals the rig's
        // current facing, so the warp must cancel that yaw and leave the rig's facing
        // unchanged on every frame, early or late. The extrapolation of the rotation still
        // to come used to be slerp-clamped to one frame, so the early frames under-counted
        // the authored yaw and turned the rig off its facing (correctness audit, finding 1).
        let mut warps = ScriptedWarps::new();
        warps.set_target(
            "enemy",
            WarpTarget {
                position: Vec3::ZERO,
                facing: Some(Quat::IDENTITY),
            },
        );
        let mut warp = ScriptedWarp::new("turn", "enemy", 0.5);
        warp.translation = false;
        warp.rotation = true;
        warps.add(warp).expect("valid warp");
        let authored = det::quat_from_rotation_y(0.2);
        let rig = RigPlacement::default();
        for frame in 0..5 {
            let (_, rotated) = warps.apply(0.1, rig, (Vec3::ZERO, authored), NO_BONE);
            assert!(
                yaw_of(rotated).abs() < 1e-5,
                "frame {frame}: emitted yaw {} instead of holding the facing",
                yaw_of(rotated)
            );
        }
    }

    #[test]
    fn rotation_warp_turns_at_the_constant_rate_with_authored_yaw() {
        // Authored root motion yaws +0.2 rad per frame and the target faces 1.0 rad away, so
        // the authored remaining yaw exactly covers the needed turn: every frame emits just
        // its authored 0.2 rad and the rig lands on the target facing. The slerp clamp used
        // to over-turn the early frames (frame 0 emitted 0.32 rad instead of 0.2).
        let facing = det::quat_from_rotation_y(1.0);
        let mut warps = ScriptedWarps::new();
        warps.set_target(
            "t",
            WarpTarget {
                position: Vec3::ZERO,
                facing: Some(facing),
            },
        );
        let mut warp = ScriptedWarp::new("w", "t", 0.5);
        warp.translation = false;
        warp.rotation = true;
        warps.add(warp).expect("add");
        let authored = det::quat_from_rotation_y(0.2);
        let mut rig = RigPlacement::default();
        let mut yaw = 0.0f32;
        for frame in 0..5 {
            let emitted = warps.apply(0.1, rig, (Vec3::ZERO, authored), NO_BONE).1;
            assert!(
                (yaw_of(emitted) - 0.2).abs() < 1e-5,
                "frame {frame}: emitted {} instead of the authored 0.2",
                yaw_of(emitted)
            );
            yaw += yaw_of(emitted);
            rig.rotation = det::quat_from_rotation_y(yaw);
        }
        assert!((yaw - 1.0).abs() < 1e-3, "lands on the target facing: {yaw}");
    }

    #[test]
    fn replacing_an_id_restarts_with_the_full_duration() {
        let mut warps = with_target(Vec3::new(0.0, 0.0, -5.0), 1.0);
        let rig = RigPlacement::default();
        for _ in 0..10 {
            warps.apply(0.05, rig, (Vec3::new(0.0, 0.0, -0.1), Quat::IDENTITY), NO_BONE);
        }
        assert!((warps.list()[0].elapsed - 0.5).abs() < 1e-5);
        warps
            .add(ScriptedWarp::new("dash", "enemy", 2.0))
            .expect("replace");
        let listed = warps.list();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].elapsed, listed[0].warp.duration), (0.0, 2.0));
    }

    #[test]
    fn bad_requests_are_refused_without_changes() {
        let mut warps = ScriptedWarps::new();
        for duration in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                warps.add(ScriptedWarp::new("a", "t", duration)),
                Err(WarpError::BadDuration)
            );
        }
        assert_eq!(
            warps.add(ScriptedWarp::new("", "t", 1.0)),
            Err(WarpError::BadName)
        );
        let mut fraction = ScriptedWarp::new("a", "t", 1.0);
        fraction.rotation_fraction = 0.0;
        assert_eq!(warps.add(fraction), Err(WarpError::BadFraction));
        assert!(!warps.is_active());
    }

    #[test]
    fn a_warp_without_its_target_passes_the_motion_through_and_still_finishes() {
        let mut warps = ScriptedWarps::new();
        warps
            .add(ScriptedWarp::new("dash", "nobody", 0.1))
            .expect("valid warp");
        let motion = (Vec3::new(0.0, 0.0, -0.5), Quat::IDENTITY);
        let (t, r) = warps.apply(0.1, RigPlacement::default(), motion, NO_BONE);
        assert_eq!((t, r), motion);
        assert_eq!(warps.take_events().len(), 1);
    }

    #[test]
    fn remove_and_clear_report_whether_they_found_something() {
        let mut warps = with_target(Vec3::Z, 1.0);
        assert!(warps.remove("dash"));
        assert!(!warps.remove("dash"));
        assert!(warps.clear_target("enemy"));
        assert!(!warps.clear_target("enemy"));
        assert!(warps.target("enemy").is_none());
    }

    #[test]
    fn an_offset_anchor_lands_the_offset_point_on_the_target() {
        // The hand is 0.8 m ahead of the origin (−Z): the origin must stop 0.8 m short.
        let target = Vec3::new(0.0, 0.0, -5.0);
        let mut warps = ScriptedWarps::new();
        warps.set_target("ledge", WarpTarget::at(target));
        let mut warp = ScriptedWarp::new("plant", "ledge", 1.0);
        warp.anchor = WarpAnchor::Offset([0.0, 0.0, -0.8]);
        warps.add(warp).expect("valid warp");
        let end = dash(&mut warps, 3.0, 1.0, 20);
        assert!((end - Vec3::new(0.0, 0.0, -4.2)).length() < 0.01, "{end}");
    }

    /// ADR-2604: a frame that runs past the warp's end warps only its share inside the warp;
    /// the rest of the frame is the authored motion. The old code mapped the whole frame onto
    /// the target, so the time after the end was lost (here 0.1 m).
    #[test]
    fn a_frame_past_the_warp_end_warps_only_its_share_inside() {
        let target = Vec3::new(0.0, 0.0, -2.0);
        let mut warps = with_target(target, 0.5);
        // 1 m/s forward, 0.2 s frames: 0.2 + 0.2 + 0.1 s inside, 0.1 s after the end.
        let per_frame = (Vec3::new(0.0, 0.0, -0.2), Quat::IDENTITY);
        let mut position = Vec3::ZERO;
        for _ in 0..3 {
            let rig = RigPlacement {
                position,
                ..RigPlacement::default()
            };
            let (t, _) = warps.apply(0.2, rig, per_frame, NO_BONE);
            position += t;
        }
        let expected = target + Vec3::new(0.0, 0.0, -0.1);
        assert!((position - expected).length() < 1e-5, "{position} vs {expected}");
        assert!(!warps.is_active());
        // A whole frame after the end passes through untouched.
        let rig = RigPlacement {
            position,
            ..RigPlacement::default()
        };
        assert_eq!(warps.apply(0.2, rig, per_frame, NO_BONE), per_frame);
    }

    /// The constant-velocity split composes back to the whole frame: with the target where the
    /// authored motion goes anyway, a frame past the end is unchanged (to rounding), turn included.
    #[test]
    fn splitting_a_turning_frame_at_the_warp_end_keeps_the_unwarped_motion() {
        let turn = Quat::from_rotation_y(0.3);
        let frame = (Vec3::new(0.1, 0.0, -0.4), turn);
        // The warp covers the first quarter of the frame and its target is that quarter's end.
        let quarter = (frame.0 * 0.25, Quat::IDENTITY.slerp(turn, 0.25));
        let mut warps = with_target(quarter.0, 0.05);
        let (t, r) = warps.apply(0.2, RigPlacement::default(), frame, NO_BONE);
        assert!((t - frame.0).length() < 1e-5, "{t} vs {}", frame.0);
        assert!(r.angle_between(turn) < 1e-5, "{r:?}");
    }
}
