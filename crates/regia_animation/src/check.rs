// SPDX-License-Identifier: MPL-2.0
//! Clip health checks for tools, cooks and the agent protocol (`anim.clip_report`, ADR-871).
//!
//! Sampling is forgiving: it clamps the sample clock to the first and last key and interpolates
//! whatever keys it is given, so broken key data does not fail loudly, it plays wrong (a sample
//! is NaN when the segment it interpolates, or the endpoint it clamps to, contains a non-finite
//! key; unsorted times play keys out of order; a notify after the clip end never fires).
//! [`check_clip`] finds those problems up front, and [`root_motion_per_lap`] reports what one
//! loop of a clip moves the character, which is what a looping locomotion clip is judged by.
//!
//! Every [`ClipIssue::code`] is a stable protocol string (lowercase ASCII and `_`), listed in
//! [`ISSUE_CODES`] and documented in `book/agents/animation-robustness.md`.

use regia_core::math::{Quat, Vec3};

use crate::clip::{AnimationClip, ClipStep, RootMotionMode, TrackCurve, filter_root_motion};
use crate::skeleton::Skeleton;

/// The clip duration is not a finite positive number of seconds.
pub const BAD_DURATION: &str = "bad_duration";
/// A key (time or value) is NaN or infinite.
pub const NON_FINITE_KEY: &str = "non_finite_key";
/// Key times are not strictly increasing.
pub const UNSORTED_TIMES: &str = "unsorted_times";
/// A curve has a different number of times and values.
pub const KEY_COUNT_MISMATCH: &str = "key_count_mismatch";
/// A key lies before 0 or after the clip end. Sampling still interpolates a time that falls
/// between that key and another key. It clamps the sample clock to the key range and does not
/// drop the key.
pub const KEY_OUTSIDE_CLIP: &str = "key_outside_clip";
/// A rotation key is far from unit length (more than 1 % off).
pub const NON_UNIT_ROTATION: &str = "non_unit_rotation";
/// A notify lies outside `0..=duration`; it does not fire unless the clip loops (a looping step
/// places it on the unwrapped clock).
pub const NOTIFY_OUTSIDE_CLIP: &str = "notify_outside_clip";
/// A track animates a joint the skeleton does not have.
pub const JOINT_OUT_OF_RANGE: &str = "joint_out_of_range";
/// Two tracks animate the same joint (the later sampleable channel wins; a later channel that
/// cannot be sampled is skipped).
pub const DUPLICATE_TRACK: &str = "duplicate_track";
/// The clip declares root motion on a joint it has no track for (it extracts nothing).
pub const ROOT_MOTION_TRACK_MISSING: &str = "root_motion_track_missing";

/// Every issue code [`check_clip`] can report.
pub const ISSUE_CODES: [&str; 10] = [
    BAD_DURATION,
    NON_FINITE_KEY,
    UNSORTED_TIMES,
    KEY_COUNT_MISMATCH,
    KEY_OUTSIDE_CLIP,
    NON_UNIT_ROTATION,
    NOTIFY_OUTSIDE_CLIP,
    JOINT_OUT_OF_RANGE,
    DUPLICATE_TRACK,
    ROOT_MOTION_TRACK_MISSING,
];

/// One problem found in a clip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipIssue {
    /// Stable code, one of [`ISSUE_CODES`].
    pub code: &'static str,
    /// Human-readable detail (which track, key or notify).
    pub detail: String,
}

impl ClipIssue {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        ClipIssue {
            code,
            detail: detail.into(),
        }
    }
}

/// How far times may sit past the clip ends before they count as outside, seconds.
const TIME_SLACK: f32 = 1e-4;

/// Check a clip's key data, notifies and root-motion setup. With a `skeleton`, tracks are also
/// checked against its joint count. Returns every issue found (empty when the clip is sound);
/// a curve reports at most one issue per code.
pub fn check_clip(clip: &AnimationClip, skeleton: Option<&Skeleton>) -> Vec<ClipIssue> {
    let mut issues = Vec::new();
    let duration = clip.duration;
    let duration_ok = duration.is_finite() && duration > 0.0;
    if !duration_ok {
        issues.push(ClipIssue::new(
            BAD_DURATION,
            format!("duration {duration} s is not a finite positive number"),
        ));
    }
    let mut seen: Vec<u32> = Vec::new();
    for track in &clip.tracks {
        let joint = track.joint;
        if seen.contains(&joint) {
            issues.push(ClipIssue::new(
                DUPLICATE_TRACK,
                format!("joint {joint} has more than one track"),
            ));
        } else {
            seen.push(joint);
        }
        if let Some(skeleton) = skeleton
            && joint as usize >= skeleton.len()
        {
            issues.push(ClipIssue::new(
                JOINT_OUT_OF_RANGE,
                format!(
                    "track for joint {joint}, but the skeleton has {} joints",
                    skeleton.len()
                ),
            ));
        }
        let label = |channel: &str| format!("joint {joint} {channel}");
        check_curve(
            &track.translations,
            &label("translation"),
            duration_ok.then_some(duration),
            &mut issues,
            |v| v.is_finite(),
        );
        check_curve(
            &track.scales,
            &label("scale"),
            duration_ok.then_some(duration),
            &mut issues,
            |v| v.is_finite(),
        );
        check_curve(
            &track.rotations,
            &label("rotation"),
            duration_ok.then_some(duration),
            &mut issues,
            |q| q.is_finite(),
        );
        let rotations: &[Quat] = match &track.rotations {
            TrackCurve::Static(q) => std::slice::from_ref(q),
            TrackCurve::Animated { values, .. } => values,
        };
        if let Some((index, q)) = rotations
            .iter()
            .enumerate()
            .find(|(_, q)| q.is_finite() && (q.length() - 1.0).abs() > 0.01)
        {
            issues.push(ClipIssue::new(
                NON_UNIT_ROTATION,
                format!("{} key {index} has length {:.4}", label("rotation"), q.length()),
            ));
        }
    }
    for track in &clip.morph_tracks {
        let label = format!("morph target {}", track.target);
        check_curve(
            &track.weights,
            &label,
            duration_ok.then_some(duration),
            &mut issues,
            |w| w.is_finite(),
        );
    }
    for curve in &clip.curves {
        let label = format!("curve {:?}", curve.name);
        check_curve(
            &curve.curve,
            &label,
            duration_ok.then_some(duration),
            &mut issues,
            |v| v.is_finite(),
        );
    }
    for track in &clip.property_tracks {
        let label = format!("property {:?}", track.path);
        check_curve(
            &track.curve,
            &label,
            duration_ok.then_some(duration),
            &mut issues,
            |v| v.is_finite(),
        );
    }
    for notify in &clip.notifies {
        let inside = notify.time.is_finite()
            && notify.time >= -TIME_SLACK
            && (!duration_ok || notify.time <= duration + TIME_SLACK);
        if !inside {
            issues.push(ClipIssue::new(
                NOTIFY_OUTSIDE_CLIP,
                format!(
                    "notify {:?} at {} s is outside 0..={duration} s",
                    notify.name, notify.time
                ),
            ));
        }
    }
    if let Some(rm) = &clip.root_motion
        && !clip.tracks.iter().any(|t| t.joint == rm.joint)
    {
        issues.push(ClipIssue::new(
            ROOT_MOTION_TRACK_MISSING,
            format!(
                "root motion is extracted from joint {}, which has no track",
                rm.joint
            ),
        ));
    }
    issues
}

fn check_curve<T: Copy>(
    curve: &TrackCurve<T>,
    label: &str,
    duration: Option<f32>,
    issues: &mut Vec<ClipIssue>,
    finite: impl Fn(&T) -> bool,
) {
    match curve {
        TrackCurve::Static(value) => {
            if !finite(value) {
                issues.push(ClipIssue::new(NON_FINITE_KEY, format!("{label}: constant value")));
            }
        }
        TrackCurve::Animated { times, values, .. } => {
            if times.len() != values.len() {
                issues.push(ClipIssue::new(
                    KEY_COUNT_MISMATCH,
                    format!("{label}: {} times, {} values", times.len(), values.len()),
                ));
            }
            if let Some(index) = times
                .iter()
                .zip(values)
                .position(|(t, v)| !t.is_finite() || !finite(v))
            {
                issues.push(ClipIssue::new(NON_FINITE_KEY, format!("{label}: key {index}")));
            }
            if let Some(index) = times.windows(2).position(|w| w[1] <= w[0]) {
                issues.push(ClipIssue::new(
                    UNSORTED_TIMES,
                    format!(
                        "{label}: key {} at {} s does not follow {} s",
                        index + 1,
                        times[index + 1],
                        times[index]
                    ),
                ));
            }
            if let Some(end) = duration
                && let Some(t) = times
                    .iter()
                    .copied()
                    .find(|t| t.is_finite() && (*t < -TIME_SLACK || *t > end + TIME_SLACK))
            {
                issues.push(ClipIssue::new(
                    KEY_OUTSIDE_CLIP,
                    format!("{label}: key at {t} s is outside 0..={end} s"),
                ));
            }
        }
    }
}

/// What one full loop of a clip adds to the character (its root motion over one lap, filtered
/// by the clip's extraction mode): `(translation, yaw in radians)`. `None` when the clip has no
/// root motion, or a duration that is not a finite positive number.
pub fn root_motion_per_lap(clip: &AnimationClip) -> Option<(Vec3, f32)> {
    let rm = clip.root_motion.as_ref()?;
    if !clip.duration.is_finite() || clip.duration <= 0.0 {
        return None;
    }
    let lap = ClipStep {
        prev: 0.0,
        time: 0.0,
        loops: 1,
    };
    let (translation, rotation) = clip.root_motion_delta_step(&lap)?;
    let (translation, rotation) = filter_root_motion(rm.mode, translation, rotation);
    let (yaw, _, _) = rotation.to_euler(regia_core::glam::EulerRot::YXZ);
    Some((translation, yaw))
}

/// How far a clip's last frame is from its first, per joint in local space: what a looping
/// clip pops by at its loop point. See [`loop_seam`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoopSeam {
    /// Largest rotation difference of any track between the clip start and end, radians.
    pub max_rotation: f32,
    /// Largest translation difference, metres.
    pub max_translation: f32,
    /// The joint with the largest seam (rotation first, then translation); `None` when every
    /// track ends where it starts.
    pub joint: Option<u32>,
}

/// The seam a clip shows when it loops. Root motion the clip extracts is not a seam: the
/// extracted axes of its root-motion joint (XZ or XYZ translation, or yaw) are left out, since
/// the character controller carries them across the loop.
pub fn loop_seam(clip: &AnimationClip) -> LoopSeam {
    let end = crate::num::finite_or(clip.duration, 0.0).max(0.0);
    let mut seam = LoopSeam::default();
    let mut worst = (0.0f32, 0.0f32);
    for track in &clip.tracks {
        let mode = clip
            .root_motion
            .as_ref()
            .filter(|rm| rm.joint == track.joint)
            .map_or(RootMotionMode::None, |rm| rm.mode);
        let mut moved = track.translations.sample(end) - track.translations.sample(0.0);
        match mode {
            RootMotionMode::XZ => {
                moved.x = 0.0;
                moved.z = 0.0;
            }
            RootMotionMode::XYZ => moved = Vec3::ZERO,
            RootMotionMode::None | RootMotionMode::Yaw => {}
        }
        let (mut first, mut last) = (track.rotations.sample(0.0), track.rotations.sample(end));
        if mode == RootMotionMode::Yaw {
            let unyaw = |q: Quat| {
                let (yaw, _, _) = q.to_euler(regia_core::glam::EulerRot::YXZ);
                Quat::from_rotation_y(-yaw) * q
            };
            first = unyaw(first);
            last = unyaw(last);
        }
        let rotation = crate::compress::rotation_error(first, last);
        let translation = moved.length();
        if !rotation.is_finite() || !translation.is_finite() {
            continue;
        }
        seam.max_rotation = seam.max_rotation.max(rotation);
        seam.max_translation = seam.max_translation.max(translation);
        if (rotation, translation) > worst && (rotation > 1e-6 || translation > 1e-6) {
            worst = (rotation, translation);
            seam.joint = Some(track.joint);
        }
    }
    seam
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::{JointTrack, Notify, RootMotionMode, RootMotionTrack};
    use crate::pose::translated;

    fn track(joint: u32, times: Vec<f32>, values: Vec<Vec3>) -> JointTrack {
        JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times,
                values,
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        }
    }

    fn codes(issues: &[ClipIssue]) -> Vec<&'static str> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn a_sound_clip_has_no_issues_and_reports_its_lap() {
        let mut clip = AnimationClip::empty("walk", 1.0);
        clip.tracks.push(track(
            0,
            vec![0.0, 1.0],
            vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -1.4)],
        ));
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        clip.notifies.push(Notify {
            time: 1.0,
            name: "step".into(),
            payload: None,
        });
        let skeleton =
            Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![translated(Vec3::ZERO)])
                .expect("valid skeleton");
        assert!(check_clip(&clip, Some(&skeleton)).is_empty());
        let (lap, yaw) = root_motion_per_lap(&clip).expect("root motion");
        assert!((lap.z + 1.4).abs() < 1e-5 && yaw.abs() < 1e-6, "{lap} {yaw}");
    }

    #[test]
    fn broken_key_data_is_reported_with_stable_codes() {
        let mut clip = AnimationClip::empty("bad", 1.0);
        clip.tracks
            .push(track(0, vec![0.0, 0.5, 0.4], vec![Vec3::ZERO; 3]));
        clip.tracks
            .push(track(0, vec![0.0, 2.0], vec![Vec3::ZERO, Vec3::splat(f32::NAN)]));
        clip.tracks.push(track(7, vec![0.0], vec![Vec3::ZERO, Vec3::ONE]));
        clip.tracks[0].rotations = TrackCurve::Static(Quat::from_xyzw(0.0, 0.0, 0.0, 2.0));
        clip.notifies.push(Notify {
            time: 3.0,
            name: "late".into(),
            payload: None,
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 4,
            mode: RootMotionMode::XZ,
        });
        let skeleton = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![None, Some(0)],
            vec![translated(Vec3::ZERO), translated(Vec3::Y)],
        )
        .expect("valid skeleton");
        let found = codes(&check_clip(&clip, Some(&skeleton)));
        for code in [
            UNSORTED_TIMES,
            DUPLICATE_TRACK,
            NON_FINITE_KEY,
            KEY_OUTSIDE_CLIP,
            KEY_COUNT_MISMATCH,
            JOINT_OUT_OF_RANGE,
            NON_UNIT_ROTATION,
            NOTIFY_OUTSIDE_CLIP,
            ROOT_MOTION_TRACK_MISSING,
        ] {
            assert!(found.contains(&code), "{code} missing from {found:?}");
        }
        let mut empty = AnimationClip::empty("zero", 0.0);
        empty.duration = f32::NAN;
        assert_eq!(codes(&check_clip(&empty, None)), [BAD_DURATION]);
        assert!(root_motion_per_lap(&empty).is_none());
    }

    #[test]
    fn issue_codes_are_protocol_strings() {
        for code in ISSUE_CODES {
            assert!(
                !code.is_empty()
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn loop_seams_ignore_extracted_root_motion() {
        // A walk whose root travels 1.4 m (extracted XZ) and whose arm ends 0.3 rad off its start.
        let mut clip = AnimationClip::empty("walk", 1.0);
        clip.tracks.push(track(
            0,
            vec![0.0, 1.0],
            vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -1.4)],
        ));
        clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::Y),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::IDENTITY, Quat::from_rotation_x(0.3)],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        let seam = loop_seam(&clip);
        assert!((seam.max_rotation - 0.3).abs() < 1e-4, "{seam:?}");
        assert!(
            seam.max_translation < 1e-6,
            "the extracted travel is not a seam: {seam:?}"
        );
        assert_eq!(seam.joint, Some(1));
        // Without extraction the root's travel is a seam (the rig snaps back every loop).
        clip.root_motion = None;
        assert!((loop_seam(&clip).max_translation - 1.4).abs() < 1e-4);
        // A clean loop has none.
        clip.tracks.truncate(1);
        clip.tracks[0] = track(0, vec![0.0, 0.5, 1.0], vec![Vec3::ZERO, Vec3::X, Vec3::ZERO]);
        assert_eq!(loop_seam(&clip), LoopSeam::default());
    }
}
