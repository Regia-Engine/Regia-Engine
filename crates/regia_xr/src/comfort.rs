// SPDX-License-Identifier: MPL-2.0
//! Comfort primitives (spec 58 § 2.8): snap turn, the motion vignette, `rig_turn` and `rig_move`.
//!
//! Both work on the rig (the tracking-space origin), never on the head: snap turn rotates the rig
//! about the head's vertical axis so the head does not translate, and the vignette rises only with
//! the rig's artificial motion. The numbers (30 degrees, 0.6 and 0.3 thresholds, 8-frame cooldown)
//! are Regia's own starting points (ADR-1585), to be tuned with a headset.

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::{Hand, Pose, XrError};
use serde::{Deserialize, Serialize};

/// Which hand is dominant: `Left` swaps the default left/right bindings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum DominantHand {
    /// Left-handed.
    Left,
    /// Right-handed (default).
    #[default]
    Right,
}

impl DominantHand {
    /// The hand a default binding written for `hand` lands on.
    pub fn map(self, hand: Hand) -> Hand {
        match (self, hand) {
            (DominantHand::Right, h) => h,
            (DominantHand::Left, Hand::Left) => Hand::Right,
            (DominantHand::Left, Hand::Right) => Hand::Left,
        }
    }
}

/// Comfort options (`comfort` in `config/xr.ron`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reflect)]
#[serde(deny_unknown_fields, default)]
pub struct ComfortSettings {
    /// Degrees per snap; 0 is smooth turning.
    pub snap_turn_deg: f32,
    /// Stick deflection that fires a snap.
    pub snap_threshold: f32,
    /// Stick deflection below which the snap re-arms.
    pub snap_rearm: f32,
    /// Fixed steps after a snap before the next may fire.
    pub snap_cooldown_frames: u32,
    /// Smooth-turn speed when `snap_turn_deg` is 0, degrees per second at full deflection.
    pub smooth_turn_deg_per_s: f32,
    /// Peak vignette darkening, 0 to 1 (0 disables it).
    pub vignette_strength: f32,
    /// Radius (radians of field of view from the view center) where darkening starts.
    pub vignette_inner: f32,
    /// Radius where darkening is full.
    pub vignette_outer: f32,
    /// Easing time constant in fixed steps (how many steps to close the gap to the target).
    pub vignette_ease_frames: u32,
    /// Eye height for an emulated floor (`Local` fallback), meters.
    pub eye_height_m: f32,
    /// Dominant hand.
    pub dominant_hand: DominantHand,
    /// Seated mode: the `Local` space plus the eye-height offset.
    pub seated: bool,
}

impl Default for ComfortSettings {
    fn default() -> Self {
        ComfortSettings {
            snap_turn_deg: 30.0,
            snap_threshold: 0.6,
            snap_rearm: 0.3,
            snap_cooldown_frames: 8,
            smooth_turn_deg_per_s: 90.0,
            vignette_strength: 0.5,
            vignette_inner: 0.6,
            vignette_outer: 1.0,
            vignette_ease_frames: 6,
            eye_height_m: 1.65,
            dominant_hand: DominantHand::Right,
            seated: false,
        }
    }
}

fn invalid(key: &str, reason: &str) -> XrError {
    XrError::InvalidValue {
        key: key.to_string(),
        reason: reason.to_string(),
    }
}

impl ComfortSettings {
    /// Checks the ranges: `snap_turn_deg` 0-180, `0 < snap_rearm < snap_threshold <= 1`,
    /// `vignette_strength` 0-1, `0 <= vignette_inner < vignette_outer`, `eye_height_m` 0.5-2.5,
    /// smooth-turn speed 0-720.
    pub fn validate(&self) -> Result<(), XrError> {
        let finite = |v: f32| v.is_finite();
        if !(finite(self.snap_turn_deg) && (0.0..=180.0).contains(&self.snap_turn_deg)) {
            return Err(invalid("snap_turn_deg", "must be 0..=180"));
        }
        if !(finite(self.snap_threshold)
            && finite(self.snap_rearm)
            && self.snap_rearm > 0.0
            && self.snap_rearm < self.snap_threshold
            && self.snap_threshold <= 1.0)
        {
            return Err(invalid(
                "snap_threshold",
                "needs 0 < snap_rearm < snap_threshold <= 1",
            ));
        }
        if !(finite(self.smooth_turn_deg_per_s) && (0.0..=720.0).contains(&self.smooth_turn_deg_per_s)) {
            return Err(invalid("smooth_turn_deg_per_s", "must be 0..=720"));
        }
        if !(finite(self.vignette_strength) && (0.0..=1.0).contains(&self.vignette_strength)) {
            return Err(invalid("vignette_strength", "must be 0..=1"));
        }
        if !(finite(self.vignette_inner)
            && finite(self.vignette_outer)
            && self.vignette_inner >= 0.0
            && self.vignette_inner < self.vignette_outer)
        {
            return Err(invalid(
                "vignette_inner",
                "needs 0 <= vignette_inner < vignette_outer",
            ));
        }
        if !(finite(self.eye_height_m) && (0.5..=2.5).contains(&self.eye_height_m)) {
            return Err(invalid("eye_height_m", "must be 0.5..=2.5"));
        }
        Ok(())
    }
}

/// The snap-turn trigger: one snap per deflection past the threshold, re-armed below the re-arm
/// level, with a cooldown in fixed steps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapTurn {
    armed_off: bool,
    cooldown: u32,
    snaps: u64,
}

impl SnapTurn {
    /// Snaps fired so far.
    pub fn snaps(&self) -> u64 {
        self.snaps
    }

    /// One fixed step with the turn stick's X deflection `stick_x` (−1 left to 1 right). Returns
    /// the rig yaw change in radians (positive turns left, about +Y), or 0. With `snap_turn_deg` 0
    /// it is smooth turning: `-(smooth_turn_deg_per_s.to_radians() * stick_x * step_secs)`. A
    /// positive stick (right) returns a negative yaw, which is a right turn.
    pub fn step(&mut self, settings: &ComfortSettings, stick_x: f32, step_secs: f32) -> f32 {
        let x = if stick_x.is_finite() {
            stick_x.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        if settings.snap_turn_deg == 0.0 {
            return -(settings.smooth_turn_deg_per_s.to_radians() * x * step_secs);
        }
        self.cooldown = self.cooldown.saturating_sub(1);
        if self.armed_off {
            if x.abs() < settings.snap_rearm {
                self.armed_off = false;
            }
            return 0.0;
        }
        if x.abs() >= settings.snap_threshold && self.cooldown == 0 {
            self.armed_off = true;
            self.cooldown = settings.snap_cooldown_frames;
            self.snaps += 1;
            return -x.signum() * settings.snap_turn_deg.to_radians();
        }
        0.0
    }
}

/// Rotates the rig (world pose `rig`) by `yaw` radians about the vertical axis through the head
/// (`head_local`, the head in rig-local space), so the head's world position does not change.
pub fn rig_turn(rig: &Pose, head_local: &Pose, yaw: f32) -> Pose {
    let head_world = rig.compose(head_local).position;
    let turn = Quat::from_rotation_y(yaw);
    Pose {
        position: head_world + turn * (rig.position - head_world),
        orientation: (turn * rig.orientation).normalize(),
    }
}

/// Moves the rig by `delta` world meters (locomotion is moving the rig).
pub fn rig_move(rig: &Pose, delta: Vec3) -> Pose {
    Pose {
        position: rig.position + delta,
        orientation: rig.orientation,
    }
}

/// Linear speed (m/s) at which the vignette target reaches full strength.
pub const VIGNETTE_FULL_LINEAR_MPS: f32 = 3.0;
/// Angular speed (rad/s) at which the vignette target reaches full strength.
pub const VIGNETTE_FULL_ANGULAR_RPS: f32 = 1.5;

/// The motion vignette's eased strength. Driven only by rig motion: the user's own head motion
/// never enters it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vignette {
    strength: f32,
}

impl Vignette {
    /// The current darkening, 0 to `vignette_strength`.
    pub fn strength(&self) -> f32 {
        self.strength
    }

    /// One fixed step with the rig's artificial linear speed (m/s) and angular speed (rad/s).
    /// The target is proportional to speed and capped at `vignette_strength`; the value closes
    /// `1 / vignette_ease_frames` of the gap per step (f32, fixed operation order).
    pub fn step(&mut self, settings: &ComfortSettings, linear_mps: f32, angular_rps: f32) -> f32 {
        let lin = if linear_mps.is_finite() {
            linear_mps.abs()
        } else {
            0.0
        };
        let ang = if angular_rps.is_finite() {
            angular_rps.abs()
        } else {
            0.0
        };
        let drive = (lin / VIGNETTE_FULL_LINEAR_MPS + ang / VIGNETTE_FULL_ANGULAR_RPS).min(1.0);
        let target = drive * settings.vignette_strength;
        let ease = settings.vignette_ease_frames.max(1) as f32;
        self.strength += (target - self.strength) / ease;
        if self.strength < 1e-6 {
            self.strength = 0.0;
        }
        self.strength
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snap_fires_once_per_deflection_and_rearms_below_the_threshold() {
        let s = ComfortSettings::default();
        let mut snap = SnapTurn::default();
        let dt = 1.0 / 60.0;
        let first = snap.step(&s, 0.9, dt);
        assert!(
            (first + 30f32.to_radians()).abs() < 1e-6,
            "right turns clockwise: {first}"
        );
        for _ in 0..20 {
            assert_eq!(snap.step(&s, 0.9, dt), 0.0, "held stick does not repeat");
        }
        assert_eq!(snap.step(&s, 0.45, dt), 0.0, "between rearm and threshold");
        assert_eq!(snap.step(&s, 0.9, dt), 0.0, "not re-armed yet");
        assert_eq!(snap.step(&s, 0.1, dt), 0.0, "re-arms");
        let left = snap.step(&s, -0.7, dt);
        assert!((left - 30f32.to_radians()).abs() < 1e-6);
        assert_eq!(snap.snaps(), 2);
    }

    #[test]
    fn the_cooldown_blocks_a_quick_second_snap() {
        let s = ComfortSettings::default();
        let mut snap = SnapTurn::default();
        assert_ne!(snap.step(&s, 1.0, 0.0), 0.0);
        assert_eq!(snap.step(&s, 0.0, 0.0), 0.0, "re-armed, cooling down");
        let mut fired_at = None;
        for i in 0..20 {
            if snap.step(&s, 1.0, 0.0) != 0.0 {
                fired_at = Some(i);
                break;
            }
        }
        // Cooldown 8 set on the snap; one step spent re-arming, so 6 more blocked steps.
        assert_eq!(fired_at, Some(6));
    }

    #[test]
    fn smooth_turn_scales_with_the_stick_and_the_step() {
        let s = ComfortSettings {
            snap_turn_deg: 0.0,
            ..ComfortSettings::default()
        };
        let mut snap = SnapTurn::default();
        let yaw = snap.step(&s, 0.5, 0.5);
        assert!((yaw + 22.5f32.to_radians()).abs() < 1e-6, "{yaw}");
    }

    #[test]
    fn rig_turn_keeps_the_head_in_place() {
        let rig = Pose::new(Vec3::new(3.0, 0.0, -2.0), Quat::from_rotation_y(0.4));
        let head = Pose::new(Vec3::new(0.3, 1.7, -0.5), Quat::from_rotation_y(-0.2));
        let before = rig.compose(&head);
        let turned = rig_turn(&rig, &head, 30f32.to_radians());
        let after = turned.compose(&head);
        assert!((after.position - before.position).length() < 1e-5);
        let yaw_change = crate::space::yaw_of(after.orientation) - crate::space::yaw_of(before.orientation);
        assert!((yaw_change - 30f32.to_radians()).abs() < 1e-5, "{yaw_change}");
        let moved = rig_move(&rig, Vec3::X);
        assert_eq!(moved.position, rig.position + Vec3::X);
    }

    #[test]
    fn the_vignette_follows_rig_speed_and_ignores_none() {
        let s = ComfortSettings::default();
        let mut v = Vignette::default();
        for _ in 0..30 {
            v.step(&s, 0.0, 0.0);
        }
        assert_eq!(v.strength(), 0.0, "no rig motion, no vignette");
        let mut last = 0.0;
        for _ in 0..10 {
            let now = v.step(&s, 3.0, 0.0);
            assert!(now > last, "rises with rig speed");
            last = now;
        }
        assert!(last <= s.vignette_strength);
        for _ in 0..200 {
            v.step(&s, 0.0, 0.0);
        }
        assert_eq!(v.strength(), 0.0, "decays back");
    }

    #[test]
    fn dominant_hand_swaps_defaults() {
        assert_eq!(DominantHand::Left.map(Hand::Left), Hand::Right);
        assert_eq!(DominantHand::Right.map(Hand::Left), Hand::Left);
    }

    #[test]
    fn validation_names_the_field() {
        let bad = ComfortSettings {
            snap_rearm: 0.7,
            ..ComfortSettings::default()
        };
        assert!(matches!(bad.validate(), Err(XrError::InvalidValue { key, .. }) if key == "snap_threshold"));
        let bad = ComfortSettings {
            eye_height_m: 9.0,
            ..ComfortSettings::default()
        };
        assert!(matches!(bad.validate(), Err(XrError::InvalidValue { key, .. }) if key == "eye_height_m"));
        assert!(ComfortSettings::default().validate().is_ok());
    }
}
