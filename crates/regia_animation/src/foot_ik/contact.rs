// SPDX-License-Identifier: MPL-2.0
//! Cooked foot-contact curves (`docs/specs/17-animation.md` § 6): the motion-matching contact
//! labels ([`label_contacts`]) computed for any clip with feet and stored on the clip as named
//! float curves `contact.<foot joint>` (1 planted, 0 swinging).
//!
//! Clips already carry named curves and the cooked clip stores them, so the curves cost no format
//! change. The graph blends named curves like poses (a 50/50 blend-space mix of a planted and a
//! swinging foot reads 0.5) and writes them into the animator's parameters, where
//! [`contact_weight`] reads them back for [`super::FootPlantLeg::contact`].
//!
//! A label comes from the foot's height over its **own lowest point in the clip** and its speed,
//! with the motion-matching hysteresis ([`ContactSettings`]). That is what makes it right where the
//! planter's fallback rule (height over the bind ankle height) is wrong: clips whose sole is not at
//! the rig origin, retargeted clips with other leg lengths, and mixes of clips.
//!
//! Speeds are measured in the clip's object space, where the stance foot of a clip that carries
//! its travel on the root joint stands still. A clip that walks in place (a treadmill clip) sets
//! [`ContactCurveSettings::treadmill`]: its feet's speeds are then measured over the ground the
//! clip walks on (its stance velocity, from the first two feet, is added back).
//!
//! Provenance: the labels are the crate's motion-matching contact labels (ADR-2858); sampling a
//! clip at its frames, the run-length key encoding and the treadmill ground speed are original
//! code (ADR-8308).

use regia_core::math::Vec3;

use crate::clip::{AnimationClip, NamedCurve, TrackCurve};
use crate::motion_matching::ContactSettings;
use crate::motion_matching::contacts::label_contacts;
use crate::params::{ParamValue, ParameterSet};
use crate::retarget::{HumanoidBone, RetargetProfile};
use crate::skeleton::Skeleton;

/// The name prefix of a contact curve: the curve of foot joint `foot_l` is `contact.foot_l`.
pub const CONTACT_CURVE_PREFIX: &str = "contact.";

/// The contact curve name of the foot joint `joint`.
pub fn contact_curve_name(joint: &str) -> String {
    format!("{CONTACT_CURVE_PREFIX}{joint}")
}

/// The contact weight `[0, 1]` the graph wrote for the foot joint `joint` into `parameters`, when
/// the playing clips carry its curve (`None` otherwise: a planter then uses its lift rule). The
/// name is built in a stack buffer, so a per-frame call allocates nothing for joint names up to
/// 96 bytes; longer names allocate once per call.
pub fn contact_weight(parameters: &ParameterSet, joint: &str) -> Option<f32> {
    let mut buffer = [0u8; 104];
    let prefix = CONTACT_CURVE_PREFIX.as_bytes();
    let total = prefix.len() + joint.len();
    let value = if total <= buffer.len() {
        buffer[..prefix.len()].copy_from_slice(prefix);
        buffer[prefix.len()..total].copy_from_slice(joint.as_bytes());
        // Both halves are valid UTF-8, so their concatenation is too.
        let name = std::str::from_utf8(&buffer[..total]).ok()?;
        parameters.get(name)
    } else {
        parameters.get(&contact_curve_name(joint))
    };
    match value {
        Some(ParamValue::Float(weight)) if weight.is_finite() => Some(weight.clamp(0.0, 1.0)),
        _ => None,
    }
}

/// Which feet get contact curves and how they are labeled. Read from a glTF source's `.meta`
/// settings as `contact_curves: (...)`; every field has a default, so `contact_curves: ()` turns
/// the curves on for the humanoid feet.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ContactCurveSettings {
    /// Foot joint names. Empty (the default) uses the humanoid foot roles guessed from the joint
    /// names ([`RetargetProfile::guess_humanoid`], left then right).
    pub feet: Vec<String>,
    /// The label thresholds and hysteresis (the motion-matching defaults). `phase` is ignored.
    pub thresholds: ContactSettings,
    /// The clip loops: the labels run one warm-up lap so frame 0 continues the end of the loop, and
    /// velocities wrap around the seam. Default `true` (locomotion cycles).
    pub looping: bool,
    /// The clip walks in place: measure foot speeds over the ground the clip walks on. Default
    /// `false`.
    pub treadmill: bool,
}

impl Default for ContactCurveSettings {
    fn default() -> Self {
        Self {
            feet: Vec::new(),
            thresholds: ContactSettings::default(),
            looping: true,
            treadmill: false,
        }
    }
}

/// Why a clip got no contact curves.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ContactCurveError {
    /// A named foot joint is not in the skeleton.
    #[error("foot joint {0:?} is not in the skeleton")]
    MissingFoot(String),
    /// No feet were named and the joint names give no humanoid foot roles.
    #[error("no foot joints: name them in `feet` (the humanoid foot roles were not found)")]
    NoFeet,
    /// The thresholds are not valid.
    #[error("bad contact thresholds: {0}")]
    BadThresholds(String),
    /// The clip has no duration or no frames to sample.
    #[error("the clip has no frames to label")]
    Empty,
}

/// The labels of one foot of a clip, frame by frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FootContacts {
    /// The foot joint.
    pub joint: u32,
    /// The foot joint's name (the curve is `contact.<name>`).
    pub name: String,
    /// Height over the foot's lowest point in the clip, per frame, meters.
    pub heights: Vec<f32>,
    /// Speed per frame, meters per second.
    pub speeds: Vec<f32>,
    /// The contact label per frame (`true` = planted).
    pub labels: Vec<bool>,
}

/// The frame times of `clip`: `0, 1/rate, …` up to and including the duration (rate =
/// [`AnimationClip::sample_rate`], 30 when it is not positive and finite).
pub fn contact_frame_times(clip: &AnimationClip) -> Vec<f32> {
    let rate = if clip.sample_rate.is_finite() && clip.sample_rate > 0.0 {
        clip.sample_rate
    } else {
        30.0
    };
    let duration = if clip.duration.is_finite() {
        clip.duration.max(0.0)
    } else {
        0.0
    };
    let frames = (duration * rate).round() as usize;
    (0..=frames.min(1 << 20))
        .map(|i| (i as f32 / rate).min(duration))
        .collect()
}

/// The foot joints `settings` names, resolved on `skeleton`.
///
/// # Errors
///
/// [`ContactCurveError::MissingFoot`] for a named joint the skeleton lacks;
/// [`ContactCurveError::NoFeet`] when none are named and none can be guessed.
pub fn resolve_feet(
    skeleton: &Skeleton,
    settings: &ContactCurveSettings,
) -> Result<Vec<(u32, String)>, ContactCurveError> {
    if !settings.feet.is_empty() {
        return settings
            .feet
            .iter()
            .map(|name| {
                skeleton
                    .joint_index(name)
                    .map(|j| (j, name.clone()))
                    .ok_or_else(|| ContactCurveError::MissingFoot(name.clone()))
            })
            .collect();
    }
    let profile = RetargetProfile::guess_humanoid(skeleton);
    let feet: Vec<(u32, String)> = [HumanoidBone::LeftFoot, HumanoidBone::RightFoot]
        .iter()
        .filter_map(|role| {
            let name = profile.roles.get(role.name())?;
            Some((skeleton.joint_index(name)?, name.clone()))
        })
        .collect();
    if feet.is_empty() {
        return Err(ContactCurveError::NoFeet);
    }
    Ok(feet)
}

/// Label every foot of `clip` (object space, [`contact_frame_times`]): heights over the foot's
/// lowest point, speeds by central differences (wrapping for a looping clip, one-sided at the ends
/// otherwise), then [`label_contacts`].
///
/// # Errors
///
/// As [`resolve_feet`], plus [`ContactCurveError::BadThresholds`] and
/// [`ContactCurveError::Empty`].
pub fn label_clip_contacts(
    clip: &AnimationClip,
    skeleton: &Skeleton,
    settings: &ContactCurveSettings,
) -> Result<Vec<FootContacts>, ContactCurveError> {
    settings
        .thresholds
        .validate()
        .map_err(|e| ContactCurveError::BadThresholds(e.to_string()))?;
    let feet = resolve_feet(skeleton, settings)?;
    let times = contact_frame_times(clip);
    if times.len() < 2 {
        return Err(ContactCurveError::Empty);
    }
    let n = times.len();
    // Object-space foot positions per frame.
    let mut pose = skeleton.bind_pose_pose();
    let mut globals = Vec::new();
    let mut positions: Vec<Vec<Vec3>> = vec![Vec::with_capacity(n); feet.len()];
    for &time in &times {
        clip.sample_pose(time, &mut pose);
        skeleton.globals_into(&pose, None, &mut globals);
        for (slot, &(joint, _)) in positions.iter_mut().zip(&feet) {
            let p = globals
                .get(joint as usize)
                .map_or(Vec3::ZERO, |m| m.w_axis.truncate());
            slot.push(if p.is_finite() { p } else { Vec3::ZERO });
        }
    }
    let dt = if n > 1 { times[1] - times[0] } else { 0.0 };
    // A looping clip's last frame repeats its first: the lap has `n - 1` distinct frames.
    let lap = n - 1;
    let velocity = |track: &[Vec3], i: usize| -> Vec3 {
        let (before, after, span) = if settings.looping {
            let before = if i == 0 { lap - 1 } else { i - 1 };
            let after = if i >= lap { 1 } else { i + 1 };
            (before, after, 2.0 * dt)
        } else {
            let before = i.saturating_sub(1);
            let after = (i + 1).min(n - 1);
            (before, after, (after - before) as f32 * dt)
        };
        if span > 0.0 {
            (track[after] - track[before]) / span
        } else {
            Vec3::ZERO
        }
    };
    let ground = if settings.treadmill && positions.len() >= 2 && lap >= 8 {
        let pairs: Vec<[Vec3; 2]> = (0..lap).map(|i| [positions[0][i], positions[1][i]]).collect();
        crate::retarget_check::stance_velocity(&pairs, dt)
    } else {
        Vec3::ZERO
    };
    Ok(feet
        .into_iter()
        .zip(positions)
        .map(|((joint, name), track)| {
            let lowest = track.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
            let heights: Vec<f32> = track.iter().map(|p| p.y - lowest).collect();
            let speeds: Vec<f32> = (0..n).map(|i| (velocity(&track, i) + ground).length()).collect();
            let labels = label_contacts(&heights, &speeds, settings.looping, &settings.thresholds);
            FootContacts {
                joint,
                name,
                heights,
                speeds,
                labels,
            }
        })
        .collect())
}

/// A 0/1 curve that reads `labels[i]` exactly at `times[i]`: a key where the value changes and one
/// on the frame before (a one-frame ramp between them), plus the first and last frame. A label that
/// never changes is a static curve.
pub fn contact_curve(times: &[f32], labels: &[bool]) -> TrackCurve<f32> {
    let value = |b: bool| if b { 1.0 } else { 0.0 };
    let n = times.len().min(labels.len());
    if n == 0 {
        return TrackCurve::Static(0.0);
    }
    if labels[..n].iter().all(|&b| b == labels[0]) {
        return TrackCurve::Static(value(labels[0]));
    }
    let mut key_times: Vec<f32> = Vec::new();
    let mut key_values: Vec<f32> = Vec::new();
    let mut push = |i: usize| {
        if key_times.last() != Some(&times[i]) {
            key_times.push(times[i]);
            key_values.push(value(labels[i]));
        }
    };
    push(0);
    for (i, pair) in labels[..n].windows(2).enumerate() {
        if pair[0] != pair[1] {
            push(i);
            push(i + 1);
        }
    }
    push(n - 1);
    TrackCurve::Animated {
        times: key_times,
        values: key_values,
        exact_rotation: false,
    }
}

/// Label `clip`'s feet ([`label_clip_contacts`]) and store one `contact.<foot>` curve per foot on
/// it, replacing curves of the same name (so a second call changes nothing). Returns the number of
/// curves written.
///
/// # Errors
///
/// As [`label_clip_contacts`]; the clip is unchanged on error.
pub fn add_contact_curves(
    clip: &mut AnimationClip,
    skeleton: &Skeleton,
    settings: &ContactCurveSettings,
) -> Result<usize, ContactCurveError> {
    let feet = label_clip_contacts(clip, skeleton, settings)?;
    let times = contact_frame_times(clip);
    for foot in &feet {
        let name = contact_curve_name(&foot.name);
        let curve = contact_curve(&times, &foot.labels);
        match clip.curves.iter_mut().find(|c| c.name == name) {
            Some(existing) => existing.curve = curve,
            None => clip.curves.push(NamedCurve { name, curve }),
        }
    }
    Ok(feet.len())
}

/// The contact curves a clip carries: `(foot joint name, curve)` for every curve named
/// `contact.<joint>`.
pub fn contact_curves(clip: &AnimationClip) -> impl Iterator<Item = (&str, &TrackCurve<f32>)> {
    clip.curves.iter().filter_map(|c| {
        c.name
            .strip_prefix(CONTACT_CURVE_PREFIX)
            .map(|joint| (joint, &c.curve))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_reads_every_label_exactly_at_its_frame() {
        let times: Vec<f32> = (0..10).map(|i| i as f32 / 30.0).collect();
        let labels = [true, true, false, false, false, true, true, true, false, true];
        let curve = contact_curve(&times, &labels);
        for (t, l) in times.iter().zip(labels) {
            assert_eq!(curve.sample(*t), if l { 1.0 } else { 0.0 }, "t {t}");
        }
        assert!(matches!(contact_curve(&times, &[true; 10]), TrackCurve::Static(v) if v == 1.0));
        assert!(matches!(contact_curve(&[], &[]), TrackCurve::Static(v) if v == 0.0));
    }

    #[test]
    fn contact_weight_reads_the_curve_parameter() {
        let mut params = ParameterSet::default();
        assert_eq!(contact_weight(&params, "foot_l"), None);
        params.set_float("contact.foot_l", 0.25);
        assert_eq!(contact_weight(&params, "foot_l"), Some(0.25));
        params.set_float("contact.foot_r", 3.0);
        assert_eq!(contact_weight(&params, "foot_r"), Some(1.0));
        let long = "j".repeat(200);
        params.set_float(&contact_curve_name(&long), 0.5);
        assert_eq!(contact_weight(&params, &long), Some(0.5));
    }
}
