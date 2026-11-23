// SPDX-License-Identifier: MPL-2.0
//! Warping of the matched pose (`docs/specs/52-motion-matching.md` § 2.10): three independent,
//! opt-in corrections applied after the matched clip was sampled and inertialized. None of them
//! changes what a search returns.
//!
//! - **Stride warping** ([`StrideWarp`], [`apply_stride_pose`]): the ratio of the character's
//!   actual ground speed to the clip's speed, clamped and smoothed, either speeds playback up
//!   or down ([`StrideMode::Rate`]) or lengthens and shortens the stride with leg IK
//!   ([`StrideMode::Pose`]).
//! - **Orientation warping** ([`orientation_delta`], [`apply_orientation_warp`]): the lower body
//!   turns toward the desired movement heading while a spine chain turns the upper body back.
//! - **Distance matching** ([`ArcLengthTable`], [`DistanceMatcher`]): a stop (or pivot, or
//!   landing) clip is played by the distance left to a target instead of by time.
//!
//! The stride and orientation parts are shared with the animation graph's `StrideWarp` and
//! `OrientationWarp` nodes (ADR-8540): they live in a crate-private module and this module is
//! their public home, unchanged. The graph nodes use the kept-buffer variants
//! [`apply_stride_pose_scratch`] and [`apply_orientation_warp_with_globals`], which give the
//! same bits as [`apply_stride_pose`] and [`apply_orientation_warp`].

pub(crate) use crate::locomotion_warp::apply_stride_pose_with_globals;
pub use crate::locomotion_warp::{
    OrientationWarpSettings, StrideMode, StrideRequest, StrideWarp, StrideWarpScratch, StrideWarpSettings,
    WarpError, apply_orientation_warp, apply_orientation_warp_with_globals, apply_stride_pose,
    apply_stride_pose_scratch, orientation_delta,
};
use regia_core::math::Vec3;

/// The root path of a distance-matched clip at its database frames, before the stop cut
/// (`docs/specs/52-motion-matching.md` § 4): what a cooked database stores so the player builds
/// its [`ArcLengthTable`] without sampling the clip (ADR-7817). Both lists have one entry per
/// frame.
#[derive(Clone, Debug, PartialEq)]
pub struct ArcLengthArrays {
    /// Cumulative ground distance (XZ plane) from the first frame, meters: starts at 0 and never
    /// decreases.
    pub distances: Vec<f32>,
    /// Ground speed at each frame from its neighbors' positions, meters per second (the stop
    /// detection's input; 0 for a single frame).
    pub speeds: Vec<f32>,
}

impl ArcLengthArrays {
    /// The arrays of the root's object-space positions at `frame_times`. `None` for empty or
    /// unequal lists, non-finite values, or times that do not strictly increase.
    pub fn from_root_positions(positions: &[Vec3], frame_times: &[f32]) -> Option<Self> {
        let n = positions.len();
        if n == 0
            || frame_times.len() != n
            || positions.iter().any(|p| !p.is_finite())
            || frame_times.iter().any(|t| !t.is_finite())
            || frame_times.windows(2).any(|w| w[1] <= w[0])
        {
            return None;
        }
        let flat = |p: Vec3| Vec3::new(p.x, 0.0, p.z);
        let speeds = (0..n)
            .map(|i| {
                let (lo, hi) = (i.saturating_sub(1), (i + 1).min(n - 1));
                if hi == lo {
                    0.0
                } else {
                    flat(positions[hi] - positions[lo]).length() / (frame_times[hi] - frame_times[lo])
                }
            })
            .collect();
        let mut distances = Vec::with_capacity(n);
        let mut total = 0.0f32;
        distances.push(0.0);
        for pair in positions.windows(2) {
            total += flat(pair[1] - pair[0]).length();
            distances.push(total);
        }
        Some(Self { distances, speeds })
    }

    /// True when the arrays describe `frames` frames: both lists that long, every value finite
    /// and not negative, distances starting at 0 and never decreasing (a cooked load checks
    /// this).
    pub fn is_valid(&self, frames: usize) -> bool {
        frames > 0
            && self.distances.len() == frames
            && self.speeds.len() == frames
            && self.distances.first() == Some(&0.0)
            && self.distances.windows(2).all(|w| w[1] >= w[0])
            && self
                .distances
                .iter()
                .chain(&self.speeds)
                .all(|v| v.is_finite() && *v >= 0.0)
    }
}

/// The cumulative ground distance of a root path, one entry per frame, for distance matching.
#[derive(Clone, Debug, PartialEq)]
pub struct ArcLengthTable {
    times: Vec<f32>,
    distances: Vec<f32>,
}

impl ArcLengthTable {
    /// Build the table from the root's object-space positions at `frame_times` (ground distance
    /// on the XZ plane). The table ends at the first stopped frame after the root started
    /// moving (ground speed below `stop_speed_threshold`, from neighbor differences) when
    /// `stop_at_end` is false, at the last frame when it is true. `None` for empty or unequal
    /// lists, non-finite values, or times that do not strictly increase.
    pub fn from_root_positions(
        positions: &[Vec3],
        frame_times: &[f32],
        stop_speed_threshold: f32,
        stop_at_end: bool,
    ) -> Option<Self> {
        let arrays = ArcLengthArrays::from_root_positions(positions, frame_times)?;
        Self::from_arrays(&arrays, frame_times, stop_speed_threshold, stop_at_end)
    }

    /// The table from stored [`ArcLengthArrays`] at `frame_times`, cut like
    /// [`ArcLengthTable::from_root_positions`]: the same table, bit for bit, as building it
    /// from the positions the arrays came from. `None` for empty or unequal lists, a non-finite
    /// threshold, or times that are not finite or do not strictly increase.
    pub fn from_arrays(
        arrays: &ArcLengthArrays,
        frame_times: &[f32],
        stop_speed_threshold: f32,
        stop_at_end: bool,
    ) -> Option<Self> {
        let n = frame_times.len();
        if n == 0
            || arrays.distances.len() != n
            || arrays.speeds.len() != n
            || !stop_speed_threshold.is_finite()
            || frame_times.iter().any(|t| !t.is_finite())
            || frame_times.windows(2).any(|w| w[1] <= w[0])
        {
            return None;
        }
        let speed = |i: usize| arrays.speeds[i];
        let end = if stop_at_end {
            n - 1
        } else {
            let first_moving = (0..n).find(|&i| speed(i) >= stop_speed_threshold);
            first_moving
                .and_then(|m| (m.max(1)..n).find(|&i| speed(i) < stop_speed_threshold))
                .unwrap_or(n - 1)
        };
        Some(Self {
            times: frame_times[..=end].to_vec(),
            distances: arrays.distances[..=end].to_vec(),
        })
    }

    /// The whole path length, meters.
    pub fn total(&self) -> f32 {
        self.distances.last().copied().unwrap_or(0.0)
    }

    /// The clip time of the last frame in the table.
    pub fn end_time(&self) -> f32 {
        self.times.last().copied().unwrap_or(0.0)
    }

    /// The clip time at which `remaining` meters of the path are left (clamped to
    /// `[0, total]`; a non-finite value counts as the whole path): binary search on the table
    /// and linear interpolation inside the segment. `total` gives the first frame's time, 0 the
    /// time the path is first complete.
    pub fn time_for_remaining(&self, remaining: f32) -> f32 {
        let total = self.total();
        let remaining = if remaining.is_finite() {
            remaining.clamp(0.0, total)
        } else {
            total
        };
        let traveled = total - remaining;
        let index = self.distances.partition_point(|&d| d < traveled);
        if index == 0 {
            return self.times.first().copied().unwrap_or(0.0);
        }
        if index >= self.distances.len() {
            return self.end_time();
        }
        let (d0, d1) = (self.distances[index - 1], self.distances[index]);
        let (t0, t1) = (self.times[index - 1], self.times[index]);
        let f = if d1 > d0 { (traveled - d0) / (d1 - d0) } else { 1.0 };
        t0 + (t1 - t0) * f
    }

    /// The path length left at clip time `time` (linear between frames, clamped to the table).
    pub fn remaining_at(&self, time: f32) -> f32 {
        let total = self.total();
        if !time.is_finite() {
            return total;
        }
        let index = self.times.partition_point(|&t| t < time);
        let traveled = if index == 0 {
            0.0
        } else if index >= self.times.len() {
            total
        } else {
            let (t0, t1) = (self.times[index - 1], self.times[index]);
            let (d0, d1) = (self.distances[index - 1], self.distances[index]);
            d0 + (d1 - d0) * ((time - t0) / (t1 - t0))
        };
        total - traveled
    }
}

/// Plays a distance-matched clip: the clip time follows the distance left to the target.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DistanceMatcher {
    last_remaining: Option<f32>,
    time: f32,
}

impl DistanceMatcher {
    /// Set the clip time from `distance_to_target` and return it: the time whose remaining path
    /// length equals the distance ([`ArcLengthTable::time_for_remaining`]). Playback never goes
    /// back: when the distance grows (the target moved away) or is not finite, the time holds.
    pub fn update(&mut self, table: &ArcLengthTable, distance_to_target: f32) -> f32 {
        if !distance_to_target.is_finite() {
            return self.time;
        }
        let distance = distance_to_target.clamp(0.0, table.total());
        match self.last_remaining {
            Some(previous) if distance > previous => {}
            Some(_) => {
                self.time = self.time.max(table.time_for_remaining(distance));
                self.last_remaining = Some(distance);
            }
            None => {
                self.time = table.time_for_remaining(distance);
                self.last_remaining = Some(distance);
            }
        }
        self.time
    }

    /// The current clip time.
    pub fn time(&self) -> f32 {
        self.time
    }

    /// Forget the target; the next update places the time afresh.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foot_ik::FootIkLeg;
    use crate::ik::{IkChain, joint_position};
    use crate::locomotion_warp::{hip_drop, rotation_of};
    use crate::pose::Pose;
    use crate::pose::translated;
    use crate::skeleton::Skeleton;
    use regia_core::math::Quat;

    fn close(a: f32, b: f32, tolerance: f32, what: &str) {
        assert!(
            (a - b).abs() <= tolerance,
            "{what}: {a} vs {b} (tolerance {tolerance})"
        );
    }

    fn stride(strength: f32) -> StrideWarpSettings {
        StrideWarpSettings {
            strength,
            ..StrideWarpSettings::default()
        }
    }

    #[test]
    fn stride_ratio_is_clamped_and_off_below_the_minimum_speed() {
        let on = stride(1.0);
        assert_eq!(on.target_ratio(3.0, 1.0), 1.25, "ratio 3 is limited to 1.25");
        assert_eq!(on.target_ratio(0.1, 1.0), 0.8);
        assert_eq!(on.target_ratio(0.05, 1.0), 1.0, "actual speed below min_speed");
        assert_eq!(on.target_ratio(1.0, 0.05), 1.0, "clip speed below min_speed");
        assert_eq!(stride(0.0).target_ratio(3.0, 1.0), 1.0, "strength 0 is off");
        close(stride(0.5).target_ratio(3.0, 1.0), 1.125, 1e-6, "half strength");
        assert_eq!(on.target_ratio(f32::NAN, 1.0), 1.0);

        let mut warp = StrideWarp::default();
        for _ in 0..240 {
            warp.update(&on, 3.0, 1.0, 1.0 / 60.0);
        }
        close(warp.ratio(), 1.25, 1e-4, "settled ratio");
        let mut idle = StrideWarp::default();
        for _ in 0..60 {
            assert_eq!(idle.update(&on, 0.05, 1.0, 1.0 / 60.0), 1.0);
        }
    }

    #[test]
    fn stride_spring_approaches_without_overshoot_and_repeats_exactly() {
        let on = stride(1.0);
        let run = || {
            let mut warp = StrideWarp::default();
            (0..120)
                .map(|_| warp.update(&on, 3.0, 1.0, 1.0 / 60.0))
                .collect::<Vec<_>>()
        };
        let a = run();
        assert_eq!(a, run(), "same steps, same ratios");
        let mut previous = 1.0;
        for &ratio in &a {
            assert!(
                ratio >= previous && ratio <= 1.25 + 1e-6,
                "monotone, no overshoot: {ratio}"
            );
            previous = ratio;
        }
        let mut warp = StrideWarp::default();
        warp.update(&on, 3.0, 1.0, f32::NAN);
        assert_eq!(warp.ratio(), 1.0, "a non-finite step advances nothing");
        assert_eq!(warp.rate_multiplier(StrideMode::Pose), 1.0);
        assert_eq!(warp.stride_ratio(StrideMode::Rate), 1.0);
    }

    /// ADR-4477 / audit animation1-determinism H4: the closed-form response uses the portable
    /// exponential, including its final f32 rounding, so playback time cannot fork by one ULP.
    #[test]
    fn stride_decay_uses_the_portable_exp_oracle() {
        let response = f32::from_bits(0x413c_feab);
        let settings = StrideWarpSettings {
            strength: 1.0,
            response,
            mode: StrideMode::Rate,
            ..StrideWarpSettings::default()
        };
        let target = settings.target_ratio(2.0, 1.0);
        let y = 1.0 - target;
        let j = response * y;
        let decay = crate::num::det_exp(-response);
        let expected = target + (y + j) * decay;
        let mut warp = StrideWarp::default();
        let actual = warp.update(&settings, 2.0, 1.0, 1.0);
        assert_eq!(actual.to_bits(), expected.to_bits());
        assert_eq!(
            warp.rate_multiplier(StrideMode::Rate).to_bits(),
            expected.to_bits()
        );
    }

    #[test]
    fn stride_settings_are_validated() {
        assert!(StrideWarpSettings::default().validate().is_ok());
        let bad = |s: StrideWarpSettings| s.validate().is_err();
        assert!(bad(stride(1.5)));
        assert!(bad(StrideWarpSettings {
            ratio_min: 1.3,
            ..stride(1.0)
        }));
        assert!(bad(StrideWarpSettings {
            response: 0.0,
            ..stride(1.0)
        }));
        assert!(bad(StrideWarpSettings {
            min_speed: -1.0,
            ..stride(1.0)
        }));
    }

    /// Pelvis at (0, 1, 0) with two 0.9 m legs and a spine of two joints and a head.
    fn body() -> Skeleton {
        let names: Vec<String> = [
            "pelvis", "l_hip", "l_knee", "l_foot", "r_hip", "r_knee", "r_foot", "spine1", "spine2", "head",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let parents = vec![
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(0),
            Some(4),
            Some(5),
            Some(0),
            Some(7),
            Some(8),
        ];
        let bind = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(-0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, 0.2, 0.0)),
            translated(Vec3::new(0.0, 0.25, 0.0)),
            translated(Vec3::new(0.0, 0.3, 0.0)),
        ];
        Skeleton::from_local_bind(names, parents, bind).expect("valid skeleton")
    }

    fn legs() -> Vec<FootIkLeg> {
        vec![
            FootIkLeg::new(IkChain {
                root: 1,
                mid: 2,
                tip: 3,
            }),
            FootIkLeg::new(IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            }),
        ]
    }

    /// Mid-stride: the left leg 0.3 rad forward, the right one back.
    fn striding(skeleton: &Skeleton) -> Pose {
        let mut pose = skeleton.bind_pose_pose();
        pose.rotations[1] = Quat::from_rotation_x(-0.3);
        pose.rotations[4] = Quat::from_rotation_x(0.3);
        pose
    }

    #[test]
    fn stride_pose_scales_the_foot_spacing_and_only_lowers_the_pelvis() {
        let skeleton = body();
        let start = striding(&skeleton);
        let feet = |pose: &Pose| {
            (
                joint_position(&skeleton, pose, 3),
                joint_position(&skeleton, pose, 6),
            )
        };
        let (left, right) = feet(&start);
        assert!(left.z > 0.2 && right.z < -0.2, "striding: {left} {right}");

        let mut same = start.clone();
        assert_eq!(
            apply_stride_pose(&skeleton, &mut same, &legs(), 0, Vec3::Z, 1.0, 1.0),
            0.0
        );
        assert_eq!(same, start, "ratio 1 is a no-op");

        // Longer stride: straight legs cannot reach, so the pelvis drops and the feet land.
        let mut long = start.clone();
        let drop = apply_stride_pose(&skeleton, &mut long, &legs(), 0, Vec3::Z, 1.2, 1.0);
        assert!(drop > 0.0, "the pelvis lowers: {drop}");
        assert!(joint_position(&skeleton, &long, 0).y < 1.0, "never raised");
        let (l2, r2) = feet(&long);
        let want_l = left + Vec3::Z * (0.2 * left.z);
        let want_r = right + Vec3::Z * (0.2 * right.z);
        assert!((l2 - want_l).length() < 1e-3, "left foot {l2} vs {want_l}");
        assert!((r2 - want_r).length() < 1e-3, "right foot {r2} vs {want_r}");

        // Shorter stride: the feet come in, the knees bend, the pelvis stays.
        let mut short = start.clone();
        assert_eq!(
            apply_stride_pose(&skeleton, &mut short, &legs(), 0, Vec3::Z, 0.8, 1.0),
            0.0
        );
        assert_eq!(short.translations[0], start.translations[0]);
        let (l3, _) = feet(&short);
        let want = left - Vec3::Z * (0.2 * left.z);
        assert!((l3 - want).length() < 1e-3, "left foot {l3} vs {want}");

        // The stride length limits the offset that is scaled.
        let mut limited = start.clone();
        apply_stride_pose(&skeleton, &mut limited, &legs(), 0, Vec3::Z, 0.8, 0.1);
        let (l4, _) = feet(&limited);
        assert!((l4 - (left - Vec3::Z * 0.02)).length() < 1e-3, "limited {l4}");
    }

    fn yaw_of(skeleton: &Skeleton, pose: &Pose, joint: u32) -> f32 {
        let forward = rotation_of(skeleton.globals(pose, None)[joint as usize]) * Vec3::Z;
        forward.x.atan2(forward.z)
    }

    fn orientation(strength: f32) -> OrientationWarpSettings {
        OrientationWarpSettings {
            strength,
            spine_joints: vec![7, 8],
            spine_weights: vec![0.4, 0.6],
            ..OrientationWarpSettings::default()
        }
    }

    #[test]
    fn orientation_delta_wraps_and_clips() {
        let deg = f32::to_radians;
        let s = orientation(1.0);
        close(
            orientation_delta(&s, deg(150.0), 0.0),
            deg(90.0),
            1e-6,
            "clipped to the limit",
        );
        close(
            orientation_delta(&s, deg(-170.0), deg(20.0)),
            deg(90.0),
            1e-6,
            "-190 wraps to +170, clipped",
        );
        close(
            orientation_delta(&s, deg(170.0), deg(-170.0)),
            deg(-20.0),
            1e-5,
            "340 wraps to -20",
        );
        close(
            orientation_delta(&s, deg(30.0), 0.0),
            deg(30.0),
            1e-6,
            "inside the limit",
        );
        close(
            orientation_delta(&orientation(0.5), deg(30.0), 0.0),
            deg(15.0),
            1e-6,
            "strength",
        );
        let off = OrientationWarpSettings {
            max_delta: 0.0,
            ..orientation(1.0)
        };
        assert_eq!(
            orientation_delta(&off, deg(30.0), 0.0),
            0.0,
            "max_delta 0 disables"
        );
        assert_eq!(orientation_delta(&orientation(0.0), deg(30.0), 0.0), 0.0);
        assert_eq!(orientation_delta(&s, f32::NAN, 0.0), 0.0);
    }

    #[test]
    fn spine_takes_back_the_pelvis_turn_so_the_head_keeps_facing() {
        let skeleton = body();
        // A rotated pelvis and a tilted spine: the turn must still be about object-space +Y.
        let mut pose = striding(&skeleton);
        pose.rotations[0] = Quat::from_rotation_y(0.3);
        pose.rotations[7] = Quat::from_rotation_x(0.2);
        let head_before = rotation_of(skeleton.globals(&pose, None)[9]);
        let head_yaw_before = yaw_of(&skeleton, &pose, 9);
        let pelvis_before = yaw_of(&skeleton, &pose, 0);
        let delta = 40f32.to_radians();
        apply_orientation_warp(&skeleton, &mut pose, 0, &orientation(1.0), delta);
        close(
            yaw_of(&skeleton, &pose, 0),
            pelvis_before + delta,
            1e-4,
            "pelvis yaw",
        );
        close(yaw_of(&skeleton, &pose, 9), head_yaw_before, 1e-4, "head yaw");
        let head_after = rotation_of(skeleton.globals(&pose, None)[9]);
        assert!(
            head_before.angle_between(head_after) < 1e-4,
            "head orientation kept: {head_before} vs {head_after}"
        );
    }

    #[test]
    fn orientation_settings_are_validated_and_bad_ones_change_nothing() {
        let skeleton = body();
        assert!(orientation(1.0).validate_for(&skeleton).is_ok());
        let mismatch = OrientationWarpSettings {
            spine_weights: vec![1.0],
            ..orientation(1.0)
        };
        assert_eq!(
            mismatch.validate(),
            Err(WarpError::SpineLengthMismatch {
                joints: 2,
                weights: 1
            })
        );
        let sum = OrientationWarpSettings {
            spine_weights: vec![0.5, 0.6],
            ..orientation(1.0)
        };
        assert!(matches!(sum.validate(), Err(WarpError::SpineWeightSum(_))));
        let negative = OrientationWarpSettings {
            spine_weights: vec![-0.5, 1.5],
            ..orientation(1.0)
        };
        assert!(matches!(
            negative.validate(),
            Err(WarpError::BadSpineWeight { index: 0, .. })
        ));
        let far = OrientationWarpSettings {
            spine_joints: vec![7, 99],
            ..orientation(1.0)
        };
        assert_eq!(
            far.validate_for(&skeleton),
            Err(WarpError::JointOutOfRange {
                joint: 99,
                joints: 10
            })
        );
        let start = striding(&skeleton);
        let mut pose = start.clone();
        apply_orientation_warp(&skeleton, &mut pose, 0, &sum, 0.5);
        assert_eq!(pose, start, "invalid settings change nothing");
    }

    /// A straight stop: the root decelerates uniformly from 2 m/s to rest at 1 s, then stands
    /// for half a second. Sampled at 30 Hz.
    fn stop_path() -> (Vec<Vec3>, Vec<f32>) {
        let times: Vec<f32> = (0..=45).map(|k| k as f32 / 30.0).collect();
        let positions = times
            .iter()
            .map(|&t| {
                let t = t.min(1.0);
                Vec3::new(0.0, 0.9, 2.0 * t - t * t)
            })
            .collect();
        (positions, times)
    }

    #[test]
    fn distance_matching_places_the_time_by_the_remaining_path() {
        let (positions, times) = stop_path();
        let table = ArcLengthTable::from_root_positions(&positions, &times, 0.05, false).expect("table");
        assert!(
            table.end_time() <= 1.0 + 1e-6,
            "ends at the first stopped frame: {}",
            table.end_time()
        );
        let total = table.total();
        close(total, 1.0, 1e-4, "arc length of the stop");
        for d in [total, total / 2.0, 0.0] {
            let time = table.time_for_remaining(d);
            close(table.remaining_at(time), d, 1e-4, "remaining at the chosen time");
        }
        close(table.time_for_remaining(total), 0.0, 1e-6, "d = L is the start");
        let whole = ArcLengthTable::from_root_positions(&positions, &times, 0.05, true).expect("table");
        close(
            whole.end_time(),
            1.5,
            1e-6,
            "stop_at_end keeps the standing frames",
        );
        close(whole.total(), total, 1e-5, "standing adds no distance");
        assert!(ArcLengthTable::from_root_positions(&positions, &times[1..], 0.05, false).is_none());
        assert!(ArcLengthTable::from_root_positions(&[], &[], 0.05, false).is_none());
    }

    #[test]
    fn distance_matching_holds_when_the_target_moves_away() {
        let (positions, times) = stop_path();
        let table = ArcLengthTable::from_root_positions(&positions, &times, 0.05, false).expect("table");
        let mut matcher = DistanceMatcher::default();
        let first = matcher.update(&table, 0.8);
        let second = matcher.update(&table, 0.5);
        assert!(second > first, "closer target plays forward");
        assert_eq!(matcher.update(&table, 0.7), second, "target moved away: hold");
        assert_eq!(matcher.update(&table, f32::NAN), second, "non-finite: hold");
        assert_eq!(
            matcher.update(&table, 0.6),
            second,
            "still farther than before: hold"
        );
        let last = matcher.update(&table, 0.0);
        close(last, table.time_for_remaining(0.0), 1e-6, "arrives at the end");
        matcher.reset();
        assert_eq!(matcher.update(&table, table.total()), 0.0);
    }

    #[test]
    fn hip_drop_is_zero_when_the_target_is_horizontally_out_of_reach() {
        assert_eq!(hip_drop(-0.85, -0.3), 0.0);
        assert!((hip_drop(-1.0, 0.25) - 0.5).abs() < 1.0e-6);
    }
}
