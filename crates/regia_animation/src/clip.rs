// SPDX-License-Identifier: MIT
// Portions ported from glam (https://github.com/bitshifter/glam-rs,
// commit e7d680ed34c051cfa8741f76821628a925161280), used under the MIT licence.
// Copyright 2020 Cameron Hart.
// DirectXMath attribution retained from glam's ATTRIBUTION.md:
// Copyright (c) 2011-2020 Microsoft Corp.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Animation clips: per-joint TRS tracks, morph weight tracks, notifies, named curves,
//! property tracks and root motion.
//!
//! Specification: `docs/specs/17-animation.md` § 1. Sampling is linear for translations and
//! scales and normalised-lerp (or slerp, opt-in per track) for rotations. Constant tracks
//! collapse to a single stored value.

use regia_core::math::{Quat, Vec3};
use regia_core::math::{det_atan2, det_cos, det_sin};
use serde::{Deserialize, Serialize};

use crate::pose::Pose;

/// One keyed curve of a track: either a constant value (collapsed) or timed keys.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum TrackCurve<T> {
    /// The track is constant: every sample returns this value (constant-track collapsing).
    Static(T),
    /// Timed keys, strictly increasing times, first key at the clip-local time given.
    Animated {
        /// Key times in seconds, strictly increasing.
        times: Vec<f32>,
        /// Key values, same length as `times`.
        values: Vec<T>,
        /// Interpolate with slerp instead of nlerp ("low key density" option).
        exact_rotation: bool,
    },
}

impl<T: Copy> TrackCurve<T> {
    /// True when the curve can be sampled: static, or animated with at least one key and as
    /// many values as times. A malformed (hand-built or corrupt) curve is skipped by
    /// [`AnimationClip::sample_pose`] so the joint keeps its previous value.
    pub fn is_sampleable(&self) -> bool {
        match self {
            TrackCurve::Static(_) => true,
            TrackCurve::Animated { times, values, .. } => !times.is_empty() && times.len() == values.len(),
        }
    }

    /// Number of stored keys (1 for a static curve).
    pub fn key_count(&self) -> usize {
        match self {
            TrackCurve::Static(_) => 1,
            TrackCurve::Animated { times, .. } => times.len(),
        }
    }

    /// First key time (0.0 for a static curve).
    pub fn start_time(&self) -> f32 {
        match self {
            TrackCurve::Static(_) => 0.0,
            TrackCurve::Animated { times, .. } => times.first().copied().unwrap_or(0.0),
        }
    }

    /// Last key time (0.0 for a static curve).
    pub fn end_time(&self) -> f32 {
        match self {
            TrackCurve::Static(_) => 0.0,
            TrackCurve::Animated { times, .. } => times.last().copied().unwrap_or(0.0),
        }
    }
}

impl TrackCurve<Vec3> {
    /// Sample the curve at `time` (clamped to the key range; linear between keys).
    pub fn sample(&self, time: f32) -> Vec3 {
        match self {
            TrackCurve::Static(v) => *v,
            TrackCurve::Animated { times, values, .. } => {
                sample_linear(times, values, time, lerp_vec3).unwrap_or(Vec3::ONE)
            }
        }
    }
}

impl TrackCurve<Quat> {
    /// Sample the curve at `time` (clamped; nlerp between keys, slerp when the track asks for
    /// exact rotation).
    pub fn sample(&self, time: f32) -> Quat {
        match self {
            TrackCurve::Static(v) => *v,
            TrackCurve::Animated {
                times,
                values,
                exact_rotation,
            } => sample_linear(times, values, time, |a, b, t| {
                if *exact_rotation {
                    regia_core::math::det_slerp(a, b, t)
                } else {
                    a.lerp(b, t)
                }
            })
            .unwrap_or(Quat::IDENTITY),
        }
    }
}

impl TrackCurve<f32> {
    /// Sample the float curve at `time` (clamped; linear between keys).
    pub fn sample(&self, time: f32) -> f32 {
        match self {
            TrackCurve::Static(v) => *v,
            TrackCurve::Animated { times, values, .. } => {
                sample_linear(times, values, time, |a, b, t| a + (b - a) * t).unwrap_or(0.0)
            }
        }
    }
}

/// Binary search for the segment containing `time` and interpolate. Values outside the key
/// range clamp to the first/last value. Returns `None` for empty curves.
fn sample_linear<T: Copy>(
    times: &[f32],
    values: &[T],
    time: f32,
    interp: impl Fn(T, T, f32) -> T,
) -> Option<T> {
    // A malformed track with unequal lengths samples the keys that have both a time and a value.
    let n = times.len().min(values.len());
    let (times, values) = (&times[..n], &values[..n]);
    if n == 0 {
        return None;
    }
    // A NaN clock would fail every comparison below and interpolate with a NaN weight.
    if time.is_nan() || time <= times[0] {
        return Some(values[0]);
    }
    let last = times.len() - 1;
    if time >= times[last] {
        return Some(values[last]);
    }
    // Invariant: times[i] < time <= times[j], j = i + 1.
    let mut lo = 0usize;
    let mut hi = last;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if times[mid] <= time {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let span = times[hi] - times[lo];
    let t = if span > 0.0 {
        (time - times[lo]) / span
    } else {
        0.0
    };
    Some(interp(values[lo], values[hi], t))
}

fn lerp_vec3(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    a.lerp(b, t)
}

/// TRS tracks for a single joint. Tracks left at `Static(bind)` are constant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JointTrack {
    /// Target joint id in the clip's skeleton.
    pub joint: u32,
    /// Translation track (metres); defaults to bind when absent at import.
    pub translations: TrackCurve<Vec3>,
    /// Rotation track; unit quaternions.
    pub rotations: TrackCurve<Quat>,
    /// Scale track; defaults to one.
    pub scales: TrackCurve<Vec3>,
}

/// The glTF node a [`MorphTrack`] targets: the node whose mesh owns the morph targets
/// (ADR-297). A track without a node (old clips, `regia_animation` authored tracks) applies to
/// every morph mesh the clip plays on, as before.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct MorphNode {
    /// Index of the node in the source file's node list. This is what the runtime matches against
    /// `SourceNode::index`, and it survives cooking and instancing (a prefab copy keeps the
    /// indices of the file it was spawned from).
    pub index: u32,
    /// Name of the node in the source file (empty when it had none). Informational: tools and
    /// the agent protocol report it; the runtime matches on `index`.
    pub name: String,
}

/// Morph weight (blend shape) track: one float curve per morph target.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MorphTrack {
    /// Morph target index in the target mesh.
    pub target: u32,
    /// Weight curve, 0..=1.
    pub weights: TrackCurve<f32>,
    /// The node whose mesh owns the target, or `None` for a track that applies to every morph
    /// mesh (clip format v1 behavior).
    pub node: Option<MorphNode>,
}

impl MorphTrack {
    /// A track that applies to every morph mesh the clip plays on.
    pub fn new(target: u32, weights: TrackCurve<f32>) -> Self {
        Self {
            target,
            weights,
            node: None,
        }
    }

    /// A track for morph target `target` of the mesh on glTF node `node`.
    pub fn for_node(node: MorphNode, target: u32, weights: TrackCurve<f32>) -> Self {
        Self {
            target,
            weights,
            node: Some(node),
        }
    }
}

/// A named float curve usable as a graph parameter (for example `"FootPlant_L"`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NamedCurve {
    /// Curve name; also the parameter name it feeds.
    pub name: String,
    /// Float keys.
    pub curve: TrackCurve<f32>,
}

/// A notify ("anim event"): fires when playback crosses `time`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Notify {
    /// Fire time in seconds from clip start.
    pub time: f32,
    /// Notify name (for example `"Footstep_L"`).
    pub name: String,
    /// Optional payload: a RON value string authored with the clip.
    pub payload: Option<String>,
}

/// A property track: animates a reflected field by path (for example `"translation.y"` on a
/// `Transform`, or `"intensity"` on a light component).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PropertyTrack {
    /// Reflection path of the target field.
    pub path: String,
    /// Float keys.
    pub curve: TrackCurve<f32>,
}

/// How root motion was extracted at import (`docs/specs/17-animation.md` § 1).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RootMotionMode {
    /// The clip has no root motion; the character moves the root itself.
    #[default]
    None,
    /// Horizontal translation only (typical locomotion).
    XZ,
    /// Full translation.
    XYZ,
    /// Only the yaw component of rotation (strafing characters, turrets).
    Yaw,
}

/// Root motion source: the joint whose object-space motion is extracted (usually the root
/// joint), and the extraction mode.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RootMotionTrack {
    /// Joint whose tracks carry the root motion.
    pub joint: u32,
    /// Extraction mode chosen at import.
    pub mode: RootMotionMode,
}

/// An animation clip asset.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnimationClip {
    /// Clip name (used by graphs to reference it).
    pub name: String,
    /// Duration in seconds (max key time across tracks).
    pub duration: f32,
    /// Nominal sample rate at author time (informational).
    pub sample_rate: f32,
    /// Per-joint TRS tracks, sorted by joint id.
    pub tracks: Vec<JointTrack>,
    /// Morph weight tracks.
    pub morph_tracks: Vec<MorphTrack>,
    /// Notifies, sorted by time.
    pub notifies: Vec<Notify>,
    /// Named float curves.
    pub curves: Vec<NamedCurve>,
    /// Property tracks (non-skeletal animation: doors, lights, cinematics).
    pub property_tracks: Vec<PropertyTrack>,
    /// Root motion source, when extracted.
    pub root_motion: Option<RootMotionTrack>,
    /// Source-node TRS hierarchy for rigid parts; independent of skeletal joint indices.
    #[serde(default)]
    pub node_animation: Option<crate::node_tracks::NodeAnimation>,
}

impl AnimationClip {
    /// An empty clip with the given name and duration.
    ///
    /// ```
    /// use regia_animation::AnimationClip;
    ///
    /// let clip = AnimationClip::empty("wave", 1.5);
    /// assert_eq!(clip.name, "wave");
    /// assert!(clip.tracks.is_empty());
    /// ```
    pub fn empty(name: impl Into<String>, duration: f32) -> Self {
        Self {
            name: name.into(),
            duration,
            sample_rate: 30.0,
            tracks: Vec::new(),
            morph_tracks: Vec::new(),
            notifies: Vec::new(),
            curves: Vec::new(),
            property_tracks: Vec::new(),
            root_motion: None,
            node_animation: None,
        }
    }

    /// Sample all joint tracks into `pose` at `time`. Joints without tracks keep their current
    /// value; start from the bind pose to get bind defaults.
    pub fn sample_pose(&self, time: f32, pose: &mut Pose) {
        for track in &self.tracks {
            let i = track.joint as usize;
            if i >= pose.len() {
                continue;
            }
            if track.translations.is_sampleable() {
                pose.translations[i] = track.translations.sample(time);
            }
            if track.rotations.is_sampleable() {
                pose.rotations[i] = track.rotations.sample(time);
            }
            if track.scales.is_sampleable() {
                pose.scales[i] = track.scales.sample(time);
            }
        }
    }

    /// Sample the morph weights that apply to every morph mesh (tracks without a node) at `time`
    /// into `(target index, weight)` pairs. Per-node tracks are sampled by
    /// [`AnimationClip::sample_node_morphs`].
    pub fn sample_morphs(&self, time: f32, out: &mut Vec<(u32, f32)>) {
        out.clear();
        for track in self.morph_tracks.iter().filter(|t| t.node.is_none()) {
            out.push((track.target, track.weights.sample(time)));
        }
    }

    /// Sample the per-node morph tracks at `time` into `(node index, target index, weight)`.
    pub fn sample_node_morphs(&self, time: f32, out: &mut Vec<(u32, u32, f32)>) {
        out.clear();
        for track in &self.morph_tracks {
            if let Some(node) = &track.node {
                out.push((node.index, track.target, track.weights.sample(time)));
            }
        }
    }

    /// True when any morph track names its node.
    pub fn has_node_morphs(&self) -> bool {
        self.morph_tracks.iter().any(|t| t.node.is_some())
    }

    /// Sample every named curve at `time` into `out`.
    ///
    /// Existing name strings are overwritten in place so a caller that keeps `out` across
    /// frames does not allocate a new `String` per curve. Extra slots are dropped when the
    /// clip has fewer curves than the buffer.
    pub fn sample_curves(&self, time: f32, out: &mut Vec<(String, f32)>) {
        if out.len() > self.curves.len() {
            out.truncate(self.curves.len());
        }
        for (index, curve) in self.curves.iter().enumerate() {
            let value = curve.curve.sample(time);
            if let Some(slot) = out.get_mut(index) {
                slot.0.clear();
                slot.0.push_str(&curve.name);
                slot.1 = value;
            } else {
                crate::hot_count::count_curve_name_clone();
                out.push((curve.name.clone(), value));
            }
        }
    }

    /// Root-motion delta between two sample times, accounting for a loop wrap: the delta from
    /// `prev` to the clip end plus from 0 to `time`, filtered by the track's extraction mode.
    /// Translation is in the clip's object space.
    ///
    /// Returns `None` when the clip has no root-motion track. See
    /// [`AnimationClip::root_motion_delta_full`] for the unfiltered variant and
    /// [`AnimationClip::root_motion_delta_step`] for reverse playback and several laps per step.
    pub fn root_motion_delta(&self, prev: f32, time: f32, wrapped: bool) -> Option<(Vec3, Quat)> {
        let rm = self.root_motion.as_ref()?;
        let (t, r) = self.root_motion_delta_full(prev, time, wrapped)?;
        Some(filter_root_motion(rm.mode, t, r))
    }

    /// Unfiltered root-motion delta (full XYZ translation and full rotation) between two
    /// sample times; the caller applies the extraction mode. `wrapped` means playback ran
    /// forward past the clip end once.
    ///
    /// Returns `None` when the clip has no root-motion track.
    pub fn root_motion_delta_full(&self, prev: f32, time: f32, wrapped: bool) -> Option<(Vec3, Quat)> {
        self.root_motion_delta_step(&ClipStep {
            prev,
            time,
            loops: i32::from(wrapped),
        })
    }

    /// Unfiltered root-motion delta over one playback step (full XYZ translation and full
    /// rotation), for any direction and any number of laps.
    ///
    /// The clip is treated as one lap of an unbounded motion: each forward lap adds the clip's
    /// end-to-start displacement and rotation once, each reverse lap removes it once. Both
    /// parts are in the clip's object space (the root joint's parent space): the rotation is
    /// `q(time) · q(prev)⁻¹`, so its twist about +Y is the turn [`RootMotionMode::Yaw`]
    /// extracts, whatever constant pitch the root carries (a Z-up import rotates it −90° about
    /// X; ADR-2604). Returns `None` when the clip has no root-motion track.
    pub fn root_motion_delta_step(&self, step: &ClipStep) -> Option<(Vec3, Quat)> {
        let rm = self.root_motion.as_ref()?;
        let track = self.tracks.iter().find(|t| t.joint == rm.joint)?;
        let p_at = |t: f32| track.translations.sample(t);
        let q_at = |t: f32| track.rotations.sample(t);
        let laps = step.loops;
        let mut translation = p_at(step.time) - p_at(step.prev);
        let start = q_at(step.prev);
        let end = q_at(step.time);
        let rotation = if laps == 0 {
            (end * start.inverse()).normalize()
        } else {
            translation += (p_at(self.duration) - p_at(0.0)) * laps as f32;
            // Unwrapped, the clock at `k` laps plus `t` has rotation `cycle^k · q(t)`, with
            // `cycle = q(end) · q(0)⁻¹` the rotation one lap adds.
            let cycle = (q_at(self.duration) * q_at(0.0).inverse()).normalize();
            (quat_pow(cycle, laps) * end * start.inverse()).normalize()
        };
        Some((translation, rotation))
    }

    /// One playback step of this clip from `prev` by `delta` seconds (see [`ClipStep`]).
    pub fn step(&self, prev: f32, delta: f32, looping: bool) -> ClipStep {
        ClipStep::advance(prev, delta, self.duration, looping)
    }

    /// Remove the extracted root-motion component from `pose` so the rig stays put while a
    /// character controller consumes [`AnimationClip::root_motion_delta`].
    ///
    /// Translation modes pin the extracted axes to the clip's time-zero sample. Yaw mode pins
    /// the root joint's twist about object-space +Y to its time-zero twist and leaves the swing
    /// (pitch and roll) animated, so the pinned rotation turned by the yaw the clip reports
    /// gives back the clip's rotation.
    pub fn pin_root_in_place(&self, pose: &mut Pose, mode: RootMotionMode) {
        if mode == RootMotionMode::None {
            return;
        }
        let Some(rm) = &self.root_motion else {
            return;
        };
        let joint = rm.joint as usize;
        if joint >= pose.len() {
            return;
        }
        let Some(track) = self.tracks.iter().find(|t| t.joint as usize == joint) else {
            return;
        };
        // A malformed curve samples as a placeholder (`Vec3::ONE`), not the clip's time-zero
        // pose; pinning to it moved the root to (1, 1, 1) (audit corr-animation). Sampling
        // skips such a track, so pinning does too.
        let sampleable = match mode {
            RootMotionMode::Yaw => track.rotations.is_sampleable(),
            _ => track.translations.is_sampleable(),
        };
        if !sampleable {
            return;
        }
        match mode {
            RootMotionMode::None => {}
            RootMotionMode::XZ => {
                let origin = track.translations.sample(0.0);
                pose.translations[joint].x = origin.x;
                pose.translations[joint].z = origin.z;
            }
            RootMotionMode::XYZ => {
                pose.translations[joint] = track.translations.sample(0.0);
            }
            RootMotionMode::Yaw => {
                // Remove the twist about object-space +Y gained since time zero (swing-twist,
                // ADR-2604). An Euler YXZ split is in gimbal lock at ±90° pitch (every Z-up
                // import), where it cannot tell yaw from roll. The twist and the rebuild use
                // the deterministic kernel so the posed root matches the extracted yaw on
                // every platform (audit-v040 H2).
                let yaw0 = twist_yaw(track.rotations.sample(0.0));
                let yaw = twist_yaw(pose.rotations[joint]);
                pose.rotations[joint] =
                    (det_rotation_y(wrap_pi(yaw0 - yaw)) * pose.rotations[joint]).normalize();
            }
        }
    }

    /// Notifies crossed when playing forward from `prev` to `time` (one loop wrap when
    /// `wrapped`). Fires on interval crossing in playback direction; a notify exactly at `time`
    /// fires, and across a wrap a notify at `0` fires too.
    pub fn notifies_crossed(&self, prev: f32, time: f32, wrapped: bool, out: &mut Vec<Notify>) {
        let step = ClipStep {
            prev,
            time,
            loops: i32::from(wrapped),
        };
        self.notifies_in_step(&step, false, out);
    }

    /// Notifies crossed by one playback step, in playback order, appended to `out`.
    ///
    /// Playing forward, a notify fires when the clock passes it on the interval `(prev, end]`;
    /// in reverse, on `[end, prev)`. A step that laps the clip several times fires each notify
    /// once per lap it crosses, up to [`MAX_NOTIFY_REPEATS`] times. `include_start` also fires
    /// notifies exactly at `prev` (the first step of a clip that just started playing, so a
    /// notify at time 0 is not lost).
    pub fn notifies_in_step(&self, step: &ClipStep, include_start: bool, out: &mut Vec<Notify>) {
        if self.notifies.is_empty() {
            return;
        }
        let start = step.prev;
        let duration = self.duration;
        if step.loops == 0 || duration <= 0.0 || !duration.is_finite() {
            let end = step.time;
            let forward = end >= start;
            let first = out.len();
            for notify in &self.notifies {
                let x = notify.time;
                let hit = if forward {
                    x > start && x <= end
                } else {
                    x >= end && x < start
                };
                if hit || (include_start && x == start) {
                    out.push(notify.clone());
                }
            }
            if !forward {
                out[first..].reverse();
            }
            return;
        }
        // Several laps: every notify occurs at `x + j·duration` on the unwrapped clock.
        let end = step.time + step.loops as f32 * duration;
        let forward = end >= start;
        let mut hits: Vec<(f32, usize)> = Vec::new();
        for (index, notify) in self.notifies.iter().enumerate() {
            let x = notify.time;
            let (j_min, j_max) = if forward {
                (
                    ((start - x) / duration).floor() + 1.0,
                    ((end - x) / duration).floor(),
                )
            } else {
                (
                    ((end - x) / duration).ceil(),
                    ((start - x) / duration).ceil() - 1.0,
                )
            };
            let mut j = j_min;
            let mut fired = 0;
            while j <= j_max && fired < MAX_NOTIFY_REPEATS {
                hits.push((x + j * duration, index));
                j += 1.0;
                fired += 1;
            }
            if include_start && x == start && !hits.iter().any(|&(at, i)| i == index && at == start) {
                hits.push((start, index));
            }
        }
        if forward {
            hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        } else {
            hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        }
        out.extend(hits.into_iter().map(|(_, index)| self.notifies[index].clone()));
    }

    /// A sub-clip of the half-open-ish range `[start, end]`, with times shifted so the slice
    /// starts at 0. Boundary samples are inserted so the slice begins and ends on the original
    /// curve. Used to split a glTF animation by time ranges at import.
    pub fn slice(&self, name: impl Into<String>, start: f32, end: f32) -> Self {
        let duration = crate::num::finite_or(self.duration, 0.0).max(0.0);
        let start = crate::num::finite_or(start, 0.0).clamp(0.0, duration);
        let end = crate::num::finite_or(end, duration).clamp(start, duration);
        Self {
            name: name.into(),
            duration: end - start,
            sample_rate: self.sample_rate,
            tracks: self
                .tracks
                .iter()
                .map(|track| JointTrack {
                    joint: track.joint,
                    translations: slice_curve_vec3(&track.translations, start, end),
                    rotations: slice_curve_quat(&track.rotations, start, end),
                    scales: slice_curve_vec3(&track.scales, start, end),
                })
                .collect(),
            morph_tracks: self
                .morph_tracks
                .iter()
                .map(|track| MorphTrack {
                    target: track.target,
                    weights: slice_curve_f32(&track.weights, start, end),
                    node: track.node.clone(),
                })
                .collect(),
            notifies: self
                .notifies
                .iter()
                .filter(|notify| notify.time >= start && notify.time <= end)
                .map(|notify| Notify {
                    time: notify.time - start,
                    name: notify.name.clone(),
                    payload: notify.payload.clone(),
                })
                .collect(),
            curves: self
                .curves
                .iter()
                .map(|curve| NamedCurve {
                    name: curve.name.clone(),
                    curve: slice_curve_f32(&curve.curve, start, end),
                })
                .collect(),
            property_tracks: self
                .property_tracks
                .iter()
                .map(|track| PropertyTrack {
                    path: track.path.clone(),
                    curve: slice_curve_f32(&track.curve, start, end),
                })
                .collect(),
            root_motion: self.root_motion.clone(),
            node_animation: self.node_animation.as_ref().map(|nodes| nodes.slice(start, end)),
        }
    }

    /// Advance a clip-local time by `delta`, wrapping at `duration` when `looping` (a negative
    /// `delta` plays in reverse and wraps at 0). A clip that does not loop clamps to
    /// `0..=duration`. Returns the new time and whether it wrapped; [`ClipStep::advance`] also
    /// reports the direction and the number of laps.
    pub fn advance(time: f32, delta: f32, duration: f32, looping: bool) -> (f32, bool) {
        let step = ClipStep::advance(time, delta, duration, looping);
        (step.time, step.loops != 0)
    }
}

/// The most times one notify fires in a single playback step that laps its clip repeatedly
/// (a long hitch or a very fast clip); the rest are dropped.
pub const MAX_NOTIFY_REPEATS: u32 = 16;

/// One playback step of a clip: the clock before and after, and the loop boundaries crossed.
///
/// `prev` and `time` are clip-local, in `0..=duration`. `loops` counts laps: `+1` for each time
/// forward playback passes the clip end, `-1` for each time reverse playback passes the start,
/// and always `0` for clips that do not loop. The unwrapped clock moves from `prev` to
/// `time + loops · duration`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClipStep {
    /// Clip-local time before the step.
    pub prev: f32,
    /// Clip-local time after the step.
    pub time: f32,
    /// Signed number of loop boundaries crossed.
    pub loops: i32,
}

impl ClipStep {
    /// Advance `prev` by `delta` seconds on a clip of `duration` seconds.
    ///
    /// ```
    /// use regia_animation::clip::ClipStep;
    ///
    /// // A 2 s clip at 1.5 s, stepped 1 s: it laps once and lands at 0.5 s.
    /// let step = ClipStep::advance(1.5, 1.0, 2.0, true);
    /// assert_eq!(step.loops, 1);
    /// assert!((step.time - 0.5).abs() < 1e-6);
    ///
    /// // Without looping the clip stops at its end.
    /// let step = ClipStep::advance(1.5, 1.0, 2.0, false);
    /// assert_eq!((step.loops, step.time), (0, 2.0));
    /// ```
    ///
    /// Non-finite inputs are treated as zero, and `prev` is clamped into the clip first, so the
    /// step is always well formed. A zero or non-finite `duration` yields a step at time 0.
    pub fn advance(prev: f32, delta: f32, duration: f32, looping: bool) -> Self {
        if !duration.is_finite() || duration <= 0.0 {
            return ClipStep::default();
        }
        let prev = crate::num::finite_or(prev, 0.0).clamp(0.0, duration);
        let raw = prev + crate::num::finite_or(delta, 0.0);
        if !looping {
            return ClipStep {
                prev,
                time: raw.clamp(0.0, duration),
                loops: 0,
            };
        }
        let mut time = raw.rem_euclid(duration);
        if time >= duration {
            time = 0.0;
        }
        // `as` saturates, so an absurd delta cannot overflow.
        let loops = ((raw - time) / duration).round() as i32;
        ClipStep { prev, time, loops }
    }

    /// True when the step plays the clip in reverse.
    pub fn is_reverse(&self) -> bool {
        self.loops < 0 || (self.loops == 0 && self.time < self.prev)
    }
}

/// Apply a root-motion extraction mode to an unfiltered delta.
pub(crate) fn filter_root_motion(mode: RootMotionMode, translation: Vec3, rotation: Quat) -> (Vec3, Quat) {
    let translation = match mode {
        RootMotionMode::None | RootMotionMode::Yaw => Vec3::ZERO,
        RootMotionMode::XZ => Vec3::new(translation.x, 0.0, translation.z),
        RootMotionMode::XYZ => translation,
    };
    let rotation = match mode {
        // The twist about +Y of the object-space delta (swing-twist decomposition, ADR-2604).
        RootMotionMode::Yaw => det_rotation_y(twist_yaw(rotation)),
        // Translation modes leave the root's rotation in the pose (only `Yaw` extracts it), so
        // they report no rotation: a consumer must not turn twice.
        RootMotionMode::None | RootMotionMode::XZ | RootMotionMode::XYZ => Quat::IDENTITY,
    };
    (translation, rotation)
}

/// `Quat::from_rotation_y` built from the deterministic trig kernels instead of the platform
/// `sin_cos`: the value lands in the lockstep root-motion query, whose bits must hold on every
/// target (glibc, bionic, the Windows CRT; ADR-4653).
fn det_rotation_y(angle: f32) -> Quat {
    let half = angle * 0.5;
    Quat::from_xyzw(0.0, det_sin(half), 0.0, det_cos(half))
}

/// The twist of `q` about object-space +Y in `[−π, π]`: the angle of `T` in the swing-twist
/// decomposition `q = T · S`, where `T` turns about +Y and the swing `S` turns about an axis
/// perpendicular to it (Dobrowolski 2015; ADR-2604). Unlike an Euler yaw it has no gimbal lock
/// at ±90° pitch, and `twist_yaw(Ry(a) · q) = twist_yaw(q) + a` (mod 2π) for every `q`.
pub(crate) fn twist_yaw(q: Quat) -> f32 {
    // The short way round: `q` and `−q` are one rotation. The deterministic atan2 keeps the
    // extracted yaw bit-identical across platform libm (audit-v040 H2).
    let q = if q.w < 0.0 { -q } else { q };
    if q.y == 0.0 && q.w == 0.0 {
        // A half turn about a horizontal axis: no twist.
        return 0.0;
    }
    // Pinned atan2: the twist feeds the lockstep root-motion query (ADR-4653).
    2.0 * det_atan2(q.y, q.w)
}

/// `angle` wrapped to `(−π, π]`.
fn wrap_pi(angle: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let a = angle.rem_euclid(tau);
    if a > std::f32::consts::PI { a - tau } else { a }
}

/// `q` raised to an integer power (the same rotation axis, `k` times the angle), on the pinned
/// axis-angle kernels: `atan2` for the angle, `sin_cos` for the rebuild (ADR-4653).
fn quat_pow(q: Quat, k: i32) -> Quat {
    let q = if q.w < 0.0 { -q } else { q };
    let v = Vec3::new(q.x, q.y, q.z);
    let length = v.length();
    if length < 1.0e-8 {
        return Quat::IDENTITY;
    }
    let angle = 2.0 * det_atan2(length, q.w) * k as f32;
    if angle.abs() < 1e-9 {
        return Quat::IDENTITY;
    }
    let half = angle * 0.5;
    let s = det_sin(half);
    let axis = v / length;
    Quat::from_xyzw(axis.x * s, axis.y * s, axis.z * s, det_cos(half))
}

fn slice_curve_vec3(curve: &TrackCurve<Vec3>, start: f32, end: f32) -> TrackCurve<Vec3> {
    let exact = match curve {
        TrackCurve::Static(v) => return TrackCurve::Static(*v),
        TrackCurve::Animated { exact_rotation, .. } => *exact_rotation,
    };
    let (times, values) = slice_samples(start, end, |t| curve.sample(t), curve);
    TrackCurve::Animated {
        times,
        values,
        exact_rotation: exact,
    }
}

fn slice_curve_quat(curve: &TrackCurve<Quat>, start: f32, end: f32) -> TrackCurve<Quat> {
    let exact = match curve {
        TrackCurve::Static(v) => return TrackCurve::Static(*v),
        TrackCurve::Animated { exact_rotation, .. } => *exact_rotation,
    };
    let (times, values) = slice_samples(start, end, |t| curve.sample(t), curve);
    TrackCurve::Animated {
        times,
        values,
        exact_rotation: exact,
    }
}

fn slice_curve_f32(curve: &TrackCurve<f32>, start: f32, end: f32) -> TrackCurve<f32> {
    let exact = match curve {
        TrackCurve::Static(v) => return TrackCurve::Static(*v),
        TrackCurve::Animated { exact_rotation, .. } => *exact_rotation,
    };
    let (times, values) = slice_samples(start, end, |t| curve.sample(t), curve);
    TrackCurve::Animated {
        times,
        values,
        exact_rotation: exact,
    }
}

/// Boundary samples plus every original key strictly inside `(start, end)`, shifted by `-start`.
fn slice_samples<T: Copy>(
    start: f32,
    end: f32,
    sample: impl Fn(f32) -> T,
    curve: &TrackCurve<T>,
) -> (Vec<f32>, Vec<T>) {
    let mut times = vec![0.0];
    let mut values = vec![sample(start)];
    if let TrackCurve::Animated {
        times: keys,
        values: key_values,
        ..
    } = curve
    {
        for (&t, &v) in keys.iter().zip(key_values) {
            if t > start && t < end {
                times.push(t - start);
                values.push(v);
            }
        }
    }
    let end_t = end - start;
    if times.last().copied().unwrap_or(0.0) < end_t - 1e-5 {
        times.push(end_t);
        values.push(sample(end));
    }
    (times, values)
}

/// Version of the cooked clip payload (`regia upgrade-project`, `cargo xtask check-formats`).
/// Version 2 adds [`MorphTrack::node`] (ADR-297). A version 1 payload carries no magic and loads
/// with every morph track unbound, so old content behaves exactly as before. Version 3 (ADR-7227)
/// holds either the clip losslessly ([`AnimationClip::to_cooked_payload`]) or its joint tracks
/// compressed at cook ([`AnimationClip::to_compressed_cooked_payload`], `docs/specs/17-animation.md`
/// § 1); a version 1 or 2 payload migrates to a lossless version 3 one.
pub const CLIP_FORMAT_VERSION: u32 = 3;

/// The cooked-clip versions a migration step reads from.
pub fn clip_migration_sources() -> Vec<u32> {
    vec![1, 2]
}

/// Serde shape of the cooked clip payload, for `cargo xtask check-formats`.
pub fn clip_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<ClipBase>(),
        serde_field_names::<JointTrack>(),
        serde_field_names::<MorphTrack>(),
        serde_field_names::<MorphNode>(),
        serde_field_names::<NamedCurve>(),
        serde_field_names::<Notify>(),
        serde_field_names::<PropertyTrack>(),
        serde_field_names::<RootMotionTrack>(),
        serde_variant_names::<RootMotionMode>(),
        serde_variant_names::<TrackCurve<f32>>(),
        serde_variant_names::<CookedClipBody>(),
        vec![crate::node_tracks::node_tracks_schema_fingerprint()],
        serde_field_names::<CompressedClipBody>(),
        serde_field_names::<crate::compress::CompressionSettings>(),
        serde_field_names::<crate::compress::CompressedJointTrack>(),
        serde_variant_names::<crate::compress::CompressedVec3Track>(),
        serde_variant_names::<crate::compress::CompressedQuatTrack>(),
    ])
}

/// Magic that starts a version 2+ cooked clip payload. A version 1 payload is the bare postcard
/// clip, which starts with the clip name: a 70-byte name beginning with `CLP` starts with these
/// bytes too (the length varint 70 is `F`), so [`AnimationClip::decode_cooked_payload`] tells the
/// two apart by decoding.
const CLIP_PAYLOAD_MAGIC: &[u8; 4] = b"FCLP";

/// Version 1 of [`MorphTrack`]: no node.
#[derive(Deserialize)]
struct MorphTrackV1 {
    target: u32,
    weights: TrackCurve<f32>,
}

/// Version 1 of [`AnimationClip`] (same fields, version 1 morph tracks).
#[derive(Deserialize)]
struct AnimationClipV1 {
    name: String,
    duration: f32,
    sample_rate: f32,
    tracks: Vec<JointTrack>,
    morph_tracks: Vec<MorphTrackV1>,
    notifies: Vec<Notify>,
    curves: Vec<NamedCurve>,
    property_tracks: Vec<PropertyTrack>,
    root_motion: Option<RootMotionTrack>,
}

impl AnimationClipV1 {
    /// The clip at the current version: every morph track unbound.
    fn upgrade(self) -> AnimationClip {
        AnimationClip {
            name: self.name,
            duration: self.duration,
            sample_rate: self.sample_rate,
            tracks: self.tracks,
            morph_tracks: self
                .morph_tracks
                .into_iter()
                .map(|t| MorphTrack::new(t.target, t.weights))
                .collect(),
            notifies: self.notifies,
            curves: self.curves,
            property_tracks: self.property_tracks,
            root_motion: self.root_motion,
            node_animation: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ClipBase {
    /// Clip name (used by graphs to reference it).
    name: String,
    /// Duration in seconds (max key time across tracks).
    duration: f32,
    /// Nominal sample rate at author time (informational).
    sample_rate: f32,
    /// Per-joint TRS tracks, sorted by joint id.
    tracks: Vec<JointTrack>,
    /// Morph weight tracks.
    morph_tracks: Vec<MorphTrack>,
    /// Notifies, sorted by time.
    notifies: Vec<Notify>,
    /// Named float curves.
    curves: Vec<NamedCurve>,
    /// Property tracks (non-skeletal animation: doors, lights, cinematics).
    property_tracks: Vec<PropertyTrack>,
    /// Root motion source, when extracted.
    root_motion: Option<RootMotionTrack>,
}

impl From<&AnimationClip> for ClipBase {
    fn from(clip: &AnimationClip) -> Self {
        Self {
            name: clip.name.clone(),
            duration: clip.duration,
            sample_rate: clip.sample_rate,
            tracks: clip.tracks.clone(),
            morph_tracks: clip.morph_tracks.clone(),
            notifies: clip.notifies.clone(),
            curves: clip.curves.clone(),
            property_tracks: clip.property_tracks.clone(),
            root_motion: clip.root_motion.clone(),
        }
    }
}

impl ClipBase {
    fn into_clip(self) -> AnimationClip {
        AnimationClip {
            name: self.name,
            duration: self.duration,
            sample_rate: self.sample_rate,
            tracks: self.tracks,
            morph_tracks: self.morph_tracks,
            notifies: self.notifies,
            curves: self.curves,
            property_tracks: self.property_tracks,
            root_motion: self.root_motion,
            node_animation: None,
        }
    }
}

/// The body of a version 3 cooked clip payload (ADR-7227): the clip as it was authored, or its
/// joint tracks compressed at cook.
#[derive(Deserialize)]
enum CookedClipBody {
    /// The clip, every key at `f32` (authored clips, migrated version 1 and 2 payloads).
    Raw(ClipBase),
    /// Joint tracks key-reduced and quantized by [`crate::compress`].
    Compressed(CompressedClipBody),
    /// Lossless base clip and a versioned node-track body.
    RawNodes(ClipBase, crate::node_tracks::NodeAnimation),
    /// Compressed joint tracks and lossless source-node tracks.
    CompressedNodes(CompressedClipBody, crate::node_tracks::NodeAnimation),
}

/// Serializes like [`CookedClipBody`] (same variants, same order) without cloning the clip.
#[derive(Serialize)]
enum CookedClipBodyRef<'a> {
    /// See [`CookedClipBody::Raw`].
    Raw(&'a ClipBase),
    /// See [`CookedClipBody::Compressed`].
    Compressed(&'a CompressedClipBody),
    RawNodes(&'a ClipBase, &'a crate::node_tracks::NodeAnimation),
    CompressedNodes(&'a CompressedClipBody, &'a crate::node_tracks::NodeAnimation),
}

/// A clip compressed at cook: its joint tracks compressed, everything else (morph weights, which may
/// leave `0..=1`, notifies, curves, property tracks, root motion) kept as authored.
#[derive(Serialize, Deserialize)]
struct CompressedClipBody {
    /// The tolerances the joint tracks were reduced to (informational).
    settings: crate::compress::CompressionSettings,
    /// One compressed track per joint track of the source, in its order.
    joints: Vec<crate::compress::CompressedJointTrack>,
    /// Indices into `joints` whose rotations interpolate with slerp ([`TrackCurve::Animated`]'s
    /// `exact_rotation`): those keep every key, quantized only.
    exact_rotation: Vec<u32>,
    /// The rest of the clip, with no joint tracks.
    rest: ClipBase,
}

impl CompressedClipBody {
    fn into_clip(self) -> AnimationClip {
        let mut clip = self.rest.into_clip();
        clip.tracks = self
            .joints
            .iter()
            .map(crate::compress::decompress_joint_track)
            .collect();
        for &i in &self.exact_rotation {
            if let Some(TrackCurve::Animated { exact_rotation, .. }) =
                clip.tracks.get_mut(i as usize).map(|t| &mut t.rotations)
            {
                *exact_rotation = true;
            }
        }
        clip
    }
}

impl CookedClipBody {
    fn into_clip(self) -> Result<AnimationClip, postcard::Error> {
        let (mut clip, nodes) = match self {
            Self::Raw(clip) => (clip.into_clip(), None),
            Self::Compressed(body) => (body.into_clip(), None),
            Self::RawNodes(clip, nodes) => (clip.into_clip(), Some(nodes)),
            Self::CompressedNodes(body, nodes) => (body.into_clip(), Some(nodes)),
        };
        if let Some(nodes) = &nodes {
            nodes
                .validate()
                .map_err(|_| postcard::Error::DeserializeBadEncoding)?;
        }
        clip.node_animation = nodes;
        Ok(clip)
    }
}

/// Magic and version 3 header followed by `body`.
fn versioned_payload(body: &CookedClipBodyRef<'_>) -> Result<Vec<u8>, postcard::Error> {
    let body = postcard::to_allocvec(body)?;
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(CLIP_PAYLOAD_MAGIC);
    out.extend_from_slice(&CLIP_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

impl AnimationClip {
    /// Serialize as the current cooked payload, losslessly: magic, the `u32` LE version, then the
    /// postcard body holding the clip as it is (every key at `f32`).
    ///
    /// # Errors
    ///
    /// Returns a postcard error for invalid node data or pathological sizes.
    pub fn to_cooked_payload(&self) -> Result<Vec<u8>, postcard::Error> {
        let base = ClipBase::from(self);
        if let Some(nodes) = &self.node_animation {
            nodes.validate().map_err(|_| postcard::Error::SerdeSerCustom)?;
            versioned_payload(&CookedClipBodyRef::RawNodes(&base, nodes))
        } else {
            versioned_payload(&CookedClipBodyRef::Raw(&base))
        }
    }

    /// Serialize as the current cooked payload with the joint tracks compressed
    /// (`docs/specs/17-animation.md` § 1, ADR-7227): [`crate::compress::compress_clip`] reduces keys
    /// within `settings` and quantizes what is left (rotations smallest-three, translations and
    /// scales 16-bit over the track range). `skeleton` gives the chain lengths the rotation error is
    /// propagated over (`None` treats every chain as 1 m). A rotation track that slerps keeps every
    /// key, quantized only. Morph weights, notifies, curves, property tracks and root motion are
    /// stored as they are. Loading decompresses the joint tracks into an ordinary clip; the same
    /// input always gives the same bytes.
    ///
    /// # Errors
    ///
    /// Returns a postcard error for invalid node data or pathological sizes.
    pub fn to_compressed_cooked_payload(
        &self,
        skeleton: Option<&crate::skeleton::Skeleton>,
        settings: &crate::compress::CompressionSettings,
    ) -> Result<Vec<u8>, postcard::Error> {
        let _span = tracing::trace_span!(target: "regia_animation", "clip_compress_cook").entered();
        let mut joints = crate::compress::compress_clip(self, skeleton, settings).tracks;
        let mut exact_rotation = Vec::new();
        for (i, (track, out)) in self.tracks.iter().zip(joints.iter_mut()).enumerate() {
            let TrackCurve::Animated {
                times,
                values,
                exact_rotation: true,
            } = &track.rotations
            else {
                continue;
            };
            if let crate::compress::CompressedQuatTrack::Animated { .. } = out.rotations {
                // Key reduction measures the nlerp chord; a slerp track keeps every key instead.
                let n = times.len().min(values.len());
                out.rotations = crate::compress::CompressedQuatTrack::Animated {
                    times: times[..n].to_vec(),
                    keys: values[..n]
                        .iter()
                        .map(|&q| crate::compress::pack_quat(q))
                        .collect(),
                };
                exact_rotation.push(i as u32);
            }
        }
        let body = CompressedClipBody {
            settings: *settings,
            joints,
            exact_rotation,
            rest: ClipBase {
                name: self.name.clone(),
                duration: self.duration,
                sample_rate: self.sample_rate,
                tracks: Vec::new(),
                morph_tracks: self.morph_tracks.clone(),
                notifies: self.notifies.clone(),
                curves: self.curves.clone(),
                property_tracks: self.property_tracks.clone(),
                root_motion: self.root_motion.clone(),
            },
        };
        if let Some(nodes) = &self.node_animation {
            nodes.validate().map_err(|_| postcard::Error::SerdeSerCustom)?;
            versioned_payload(&CookedClipBodyRef::CompressedNodes(&body, nodes))
        } else {
            versioned_payload(&CookedClipBodyRef::Compressed(&body))
        }
    }

    /// The compressed joint tracks of a version 3 payload written by
    /// [`Self::to_compressed_cooked_payload`], as a [`crate::compress::CompressedClip`] (for
    /// [`crate::compress::measure_compression_error`] and tools); `None` for any other payload. Its
    /// `morph_tracks` are empty: a cooked clip keeps morph weights at `f32` (they are in the clip
    /// [`Self::from_cooked_payload`] returns).
    pub fn compressed_cooked_payload(bytes: &[u8]) -> Option<crate::compress::CompressedClip> {
        let body = match bytes {
            [b'F', b'C', b'L', b'P', a, b, c, d, body @ ..] if u32::from_le_bytes([*a, *b, *c, *d]) == 3 => {
                body
            }
            _ => return None,
        };
        let (body, node_animation) = match postcard::from_bytes::<CookedClipBody>(body).ok()? {
            CookedClipBody::Raw(_) | CookedClipBody::RawNodes(..) => return None,
            CookedClipBody::Compressed(body) => (body, None),
            CookedClipBody::CompressedNodes(body, nodes) => {
                nodes.validate().ok()?;
                (body, Some(nodes))
            }
        };
        Some(crate::compress::CompressedClip {
            name: body.rest.name,
            duration: body.rest.duration,
            sample_rate: body.rest.sample_rate,
            tracks: body.joints,
            morph_tracks: Vec::new(),
            notifies: body.rest.notifies,
            curves: body.rest.curves,
            property_tracks: body.rest.property_tracks,
            root_motion: body.rest.root_motion,
            node_animation,
        })
    }

    /// The version of a cooked clip payload: the one [`Self::decode_cooked_payload`] reads it as,
    /// or, when it does not decode, the version its header names (1 when it has no magic).
    pub fn cooked_payload_version(bytes: &[u8]) -> u32 {
        Self::decode_cooked_payload(bytes).map_or_else(
            |_| match bytes {
                [b'F', b'C', b'L', b'P', a, b, c, d, ..] => u32::from_le_bytes([*a, *b, *c, *d]),
                _ => 1,
            },
            |(version, _)| version,
        )
    }

    /// Decode a cooked clip payload of any version up to [`CLIP_FORMAT_VERSION`]. A version 1
    /// payload loads with every morph track unbound (it applies to every morph mesh); a compressed
    /// version 3 payload loads with its joint tracks decompressed.
    ///
    /// # Errors
    ///
    /// Returns a postcard error when the bytes are not a clip, or were written by a newer engine.
    pub fn from_cooked_payload(bytes: &[u8]) -> Result<Self, postcard::Error> {
        Self::decode_cooked_payload(bytes).map(|(_, clip)| clip)
    }

    /// [`Self::from_cooked_payload`], with the version the payload was read as.
    ///
    /// A payload that starts with the magic is either version 2+ or a version 1 clip whose name
    /// begins with the magic (see `CLIP_PAYLOAD_MAGIC`). It is read at the version it names when
    /// that is a known one and decodes; otherwise it is a version 1 clip only when it decodes as
    /// one using every byte, which a newer engine's payload does not.
    ///
    /// # Errors
    ///
    /// As [`Self::from_cooked_payload`].
    pub fn decode_cooked_payload(bytes: &[u8]) -> Result<(u32, Self), postcard::Error> {
        let (claimed, body) = match bytes {
            [b'F', b'C', b'L', b'P', a, b, c, d, body @ ..] => (u32::from_le_bytes([*a, *b, *c, *d]), body),
            _ => {
                let old: AnimationClipV1 = postcard::from_bytes(bytes)?;
                return Ok((1, old.upgrade()));
            }
        };
        let versioned: Result<Self, postcard::Error> = match claimed {
            2 => postcard::from_bytes::<ClipBase>(body).map(ClipBase::into_clip),
            3 => postcard::from_bytes::<CookedClipBody>(body).and_then(CookedClipBody::into_clip),
            _ => Err(postcard::Error::DeserializeBadEncoding),
        };
        match versioned {
            Ok(clip) => Ok((claimed, clip)),
            Err(err) => match postcard::take_from_bytes::<AnimationClipV1>(bytes) {
                Ok((old, [])) => Ok((1, old.upgrade())),
                _ => Err(err),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quat_track() -> TrackCurve<Quat> {
        TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Quat::IDENTITY, Quat::from_rotation_y(1.0)],
            exact_rotation: false,
        }
    }

    #[test]
    fn sampling_tolerates_more_times_than_values() {
        let curve = TrackCurve::Animated {
            times: vec![0.0, 1.0, 2.0, 3.0],
            values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)],
            exact_rotation: false,
        };
        // Past the last key that has a value: clamps to it instead of indexing out of range.
        assert_eq!(curve.sample(2.5), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(curve.sample(0.5), Vec3::new(0.5, 0.0, 0.0));
    }

    #[test]
    fn sampling_is_exact_at_keys() {
        let curve = TrackCurve::Animated {
            times: vec![0.0, 1.0, 2.0],
            values: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 2.0, 3.0),
                Vec3::new(4.0, 0.0, 0.0),
            ],
            exact_rotation: false,
        };
        assert_eq!(curve.sample(0.0), Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(curve.sample(1.0), Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(curve.sample(2.0), Vec3::new(4.0, 0.0, 0.0));
    }

    #[test]
    fn sampling_interpolates_linearly() {
        let curve = TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![Vec3::ZERO, Vec3::new(2.0, 4.0, -2.0)],
            exact_rotation: false,
        };
        assert_eq!(curve.sample(0.5), Vec3::new(0.5, 1.0, -0.5));
        assert_eq!(curve.sample(1.0), Vec3::new(1.0, 2.0, -1.0));
    }

    #[test]
    fn sampling_clamps_outside_range() {
        let curve = TrackCurve::Animated {
            times: vec![1.0, 2.0],
            values: vec![Vec3::ONE, Vec3::splat(5.0)],
            exact_rotation: false,
        };
        assert_eq!(curve.sample(0.0), Vec3::ONE);
        assert_eq!(curve.sample(9.0), Vec3::splat(5.0));
    }

    #[test]
    fn static_curve_returns_constant() {
        let curve: TrackCurve<Vec3> = TrackCurve::Static(Vec3::new(7.0, 0.0, 0.0));
        assert_eq!(curve.sample(0.0), Vec3::new(7.0, 0.0, 0.0));
        assert_eq!(curve.sample(100.0), Vec3::new(7.0, 0.0, 0.0));
        assert_eq!(curve.key_count(), 1);
    }

    #[test]
    fn rotation_track_nlerp_and_slerp() {
        let a = Quat::from_rotation_x(0.0);
        let b = Quat::from_rotation_x(1.5);
        let nlerp_track = TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![a, b],
            exact_rotation: false,
        };
        let slerp_track = TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![a, b],
            exact_rotation: true,
        };
        let via_nlerp = nlerp_track.sample(0.5);
        let via_slerp = slerp_track.sample(0.5);
        let expected_half = Quat::from_rotation_x(0.75);
        assert!(via_slerp.abs_diff_eq(expected_half, 1e-4));
        // nlerp errs slightly for large angles but stays on the same arc.
        let (ny, _, _) = via_nlerp.to_euler(regia_core::glam::EulerRot::XYZ);
        let _ = ny;
        assert!(via_nlerp.angle_between(expected_half) < 0.02);
    }

    #[test]
    fn rotation_track_nlerp_and_slerp_differ_for_wide_keys() {
        let a = Quat::from_rotation_x(2.0944);
        let b = Quat::from_rotation_y(2.0944);
        let make = |exact_rotation| TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![a, b],
            exact_rotation,
        };
        let via_nlerp = make(false).sample(0.25);
        let via_slerp = make(true).sample(0.25);
        assert!(
            via_nlerp.angle_between(via_slerp) > 1e-3,
            "exact_rotation must select slerp over nlerp"
        );
        // Slerp is the constant-speed arc: a quarter of the way in, a quarter of the angle.
        let total = a.angle_between(b);
        assert!((via_slerp.angle_between(a) - total * 0.25).abs() < 1e-3);
        assert!((via_slerp.angle_between(b) - total * 0.75).abs() < 1e-3);
    }

    #[test]
    fn sample_pose_writes_only_tracked_joints() {
        let mut clip = AnimationClip::empty("t", 1.0);
        clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::new(0.0, 1.0, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut pose = Pose::from_identity(2);
        clip.sample_pose(0.0, &mut pose);
        assert_eq!(pose.translations[1], Vec3::new(0.0, 1.0, 0.0));
        assert_eq!(pose.translations[0], Vec3::ZERO, "untracked joint untouched");
    }

    #[test]
    fn sample_pose_skips_a_joint_channel_whose_times_and_values_differ() {
        let mut clip = AnimationClip::empty("uneven", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 0.5, 1.0],
                values: vec![Vec3::ZERO, Vec3::X],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut pose = Pose::from_identity(1);
        pose.translations[0] = Vec3::Y;
        clip.sample_pose(0.5, &mut pose);
        assert_eq!(
            pose.translations[0],
            Vec3::Y,
            "an unequal joint channel keeps the pose's current value"
        );

        let mut layered = AnimationClip::empty("dup", 1.0);
        layered.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::new(3.0, 0.0, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        layered.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 0.5, 1.0],
                values: vec![Vec3::X],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut pose = Pose::from_identity(1);
        layered.sample_pose(0.5, &mut pose);
        assert_eq!(pose.translations[0], Vec3::new(3.0, 0.0, 0.0));
    }

    #[test]
    fn sample_pose_keeps_a_key_that_lies_outside_the_clip() {
        let mut clip = AnimationClip::empty("past", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![Vec3::ZERO, Vec3::new(2.0, 4.0, -2.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut pose = Pose::from_identity(1);
        clip.sample_pose(1.0, &mut pose);
        assert_eq!(pose.translations[0], Vec3::new(1.0, 2.0, -1.0));
    }

    #[test]
    fn a_non_finite_key_poisons_only_the_segment_that_contains_it() {
        let curve = TrackCurve::Animated {
            times: vec![0.0, 1.0, 2.0],
            values: vec![0.0, f32::NAN, 0.0],
            exact_rotation: false,
        };
        assert_eq!(curve.sample(0.0), 0.0);
        assert_eq!(curve.sample(2.0), 0.0);
        assert!(curve.sample(0.5).is_nan());
        assert!(curve.sample(1.0).is_nan());
        assert!(curve.sample(1.5).is_nan());
    }

    #[test]
    fn a_short_morph_track_plays_its_paired_prefix() {
        let mut clip = AnimationClip::empty("morph", 2.0);
        clip.morph_tracks.push(MorphTrack::new(
            1,
            TrackCurve::Animated {
                times: vec![0.0, 1.0, 2.0],
                values: vec![0.0, 1.0],
                exact_rotation: false,
            },
        ));
        let mut out = Vec::new();
        clip.sample_morphs(0.5, &mut out);
        assert_eq!(out, vec![(1, 0.5)]);
        clip.sample_morphs(2.0, &mut out);
        assert_eq!(out, vec![(1, 1.0)]);
    }

    #[test]
    fn notifies_fire_on_crossing() {
        let mut clip = AnimationClip::empty("t", 2.0);
        clip.notifies = vec![
            Notify {
                time: 0.5,
                name: "a".into(),
                payload: None,
            },
            Notify {
                time: 1.5,
                name: "b".into(),
                payload: Some("(damage: 10)".into()),
            },
        ];
        let mut fired = Vec::new();
        clip.notifies_crossed(0.0, 1.0, false, &mut fired);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].name, "a");

        fired.clear();
        clip.notifies_crossed(1.0, 2.0, false, &mut fired);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].name, "b");

        // No double-fire: half-open interval.
        fired.clear();
        clip.notifies_crossed(0.5, 1.0, false, &mut fired);
        assert!(fired.is_empty());
    }

    #[test]
    fn notifies_fire_across_loop_wrap() {
        let mut clip = AnimationClip::empty("t", 2.0);
        clip.notifies = vec![
            Notify {
                time: 1.8,
                name: "tail".into(),
                payload: None,
            },
            Notify {
                time: 0.2,
                name: "head".into(),
                payload: None,
            },
        ];
        let mut fired = Vec::new();
        clip.notifies_crossed(1.5, 0.5, true, &mut fired);
        assert_eq!(
            fired.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            ["tail", "head"]
        );
    }

    #[test]
    fn advance_wraps_and_clamps() {
        let (t, wrapped) = AnimationClip::advance(0.9, 0.3, 1.0, true);
        assert!(wrapped);
        assert!((t - 0.2).abs() < 1e-6, "t = {t}");
        let (t, wrapped) = AnimationClip::advance(0.9, 0.3, 1.0, false);
        assert!((t - 1.0).abs() < 1e-6);
        assert!(!wrapped);
        let (t, wrapped) = AnimationClip::advance(0.2, 0.3, 1.0, true);
        assert!((t - 0.5).abs() < 1e-6);
        assert!(!wrapped);
    }

    #[test]
    fn advance_clamps_reverse_playback_at_start() {
        // Reverse time never goes below the clip start: a clip that does not loop clamps at 0,
        // a looping clip wraps back to its end (the lap-aware `ClipStep`, a reverse lap).
        let (t, wrapped) = AnimationClip::advance(0.1, -0.5, 1.0, false);
        assert_eq!(t, 0.0);
        assert!(!wrapped);
        let (t, wrapped) = AnimationClip::advance(0.1, -0.5, 1.0, true);
        assert!(
            (0.0..1.0).contains(&t) && (t - 0.6).abs() < 1e-6,
            "wraps to the end: {t}"
        );
        assert!(wrapped);
        let (t, _) = AnimationClip::advance(0.0, 0.25, 1.0, true);
        assert!((t - 0.25).abs() < 1e-6, "no negative debt to climb: {t}");
    }

    #[test]
    fn malformed_track_curves_do_not_panic_or_write() {
        let mut clip = AnimationClip::empty("bad", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            // Fewer values than times, and an empty translation track.
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::new(5.0, 5.0, 5.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![],
                exact_rotation: false,
            },
            scales: TrackCurve::Animated {
                times: vec![],
                values: vec![],
                exact_rotation: false,
            },
        });
        let mut pose = Pose::from_identity(1);
        pose.translations[0] = Vec3::new(1.0, 2.0, 3.0);
        clip.sample_pose(0.5, &mut pose);
        clip.sample_pose(2.0, &mut pose);
        assert_eq!(pose.translations[0], Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(pose.rotations[0], Quat::IDENTITY);
        assert_eq!(pose.scales[0], Vec3::ONE);
    }

    #[test]
    fn slice_shifts_keys_and_notifies() {
        let mut clip = AnimationClip::empty("full", 2.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0, 2.0],
                values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.notifies.push(Notify {
            time: 1.25,
            name: "mid".into(),
            payload: None,
        });
        let slice = clip.slice("mid", 1.0, 2.0);
        assert!((slice.duration - 1.0).abs() < 1e-6);
        assert_eq!(slice.tracks[0].translations.sample(0.0), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(slice.tracks[0].translations.sample(1.0), Vec3::new(2.0, 0.0, 0.0));
        assert_eq!(slice.notifies.len(), 1);
        assert!((slice.notifies[0].time - 0.25).abs() < 1e-6);
    }

    #[test]
    fn root_motion_delta_translation() {
        let mut clip = AnimationClip::empty("t", 2.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![Vec3::ZERO, Vec3::new(2.0, 6.0, 4.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        let (delta, rot) = clip.root_motion_delta(0.5, 1.0, false).expect("root motion");
        // Half of the segment: (0.5, 1.5, 1.0) with Y stripped.
        assert!(delta.abs_diff_eq(Vec3::new(0.5, 0.0, 1.0), 1e-5));
        assert!(rot.abs_diff_eq(Quat::IDENTITY, 1e-6));

        let (delta, _) = clip.root_motion_delta(1.5, 0.5, true).expect("root motion");
        // 0.5 s to the end (0.5, 1.5, 1.0) + 0.5 s from the start (0.5, 1.5, 1.0), XZ only.
        assert!(delta.abs_diff_eq(Vec3::new(1.0, 0.0, 2.0), 1e-5));
    }

    #[test]
    fn quat_track_sampling_endpoints() {
        let track = quat_track();
        assert!(track.sample(0.0).abs_diff_eq(Quat::IDENTITY, 1e-6));
        assert!(track.sample(1.0).abs_diff_eq(Quat::from_rotation_y(1.0), 1e-6));
    }

    fn ramp(a: f32, b: f32) -> TrackCurve<f32> {
        TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![a, b],
            exact_rotation: false,
        }
    }

    /// The version 1 wire shape of a clip with one morph track (the old cooked payload).
    #[derive(Serialize)]
    struct OldMorph {
        target: u32,
        weights: TrackCurve<f32>,
    }

    #[derive(Serialize)]
    struct OldClip {
        name: String,
        duration: f32,
        sample_rate: f32,
        tracks: Vec<JointTrack>,
        morph_tracks: Vec<OldMorph>,
        notifies: Vec<Notify>,
        curves: Vec<NamedCurve>,
        property_tracks: Vec<PropertyTrack>,
        root_motion: Option<RootMotionTrack>,
    }

    #[test]
    fn v1_payload_migrates_with_unbound_morph_tracks() {
        let old = OldClip {
            name: "smile".into(),
            duration: 1.0,
            sample_rate: 30.0,
            tracks: Vec::new(),
            morph_tracks: vec![OldMorph {
                target: 2,
                weights: ramp(0.0, 1.0),
            }],
            notifies: Vec::new(),
            curves: Vec::new(),
            property_tracks: Vec::new(),
            root_motion: None,
        };
        let bytes = postcard::to_allocvec(&old).expect("encode v1");
        assert_eq!(AnimationClip::cooked_payload_version(&bytes), 1);
        let clip = AnimationClip::from_cooked_payload(&bytes).expect("v1 loads");
        assert_eq!(clip.morph_tracks, vec![MorphTrack::new(2, ramp(0.0, 1.0))]);
        let mut pairs = Vec::new();
        clip.sample_morphs(0.5, &mut pairs);
        assert_eq!(pairs, vec![(2, 0.5)]);
        let mut nodes = Vec::new();
        clip.sample_node_morphs(0.5, &mut nodes);
        assert!(nodes.is_empty());
    }

    #[test]
    fn v2_payload_round_trips_and_newer_is_rejected() {
        let mut clip = AnimationClip::empty("two", 1.0);
        let node = MorphNode {
            index: 3,
            name: "head".into(),
        };
        clip.morph_tracks
            .push(MorphTrack::for_node(node, 1, ramp(0.0, 1.0)));
        let bytes = clip.to_cooked_payload().expect("encode");
        assert_eq!(AnimationClip::cooked_payload_version(&bytes), CLIP_FORMAT_VERSION);
        assert_eq!(AnimationClip::from_cooked_payload(&bytes).expect("decode"), clip);
        let mut newer = bytes;
        newer[4..8].copy_from_slice(&(CLIP_FORMAT_VERSION + 1).to_le_bytes());
        assert!(AnimationClip::from_cooked_payload(&newer).is_err());
    }

    /// A version 1 clip whose 70-byte name begins with `CLP` starts with the version 2 magic (the
    /// name length varint 70 is `F`; grok27 diff-core #3). It still loads as version 1, also when
    /// the next four name bytes read as version 2 (a newer version stays rejected:
    /// `v2_payload_round_trips_and_newer_is_rejected`).
    #[test]
    fn a_v1_clip_whose_name_spells_the_magic_still_loads() {
        for prefix in ["CLPxxxx", "CLP\u{2}\0\0\0"] {
            let name = format!("{prefix}{}", "x".repeat(70 - prefix.len()));
            assert_eq!(name.len(), 70);
            let old = OldClip {
                name: name.clone(),
                duration: 1.0,
                sample_rate: 30.0,
                tracks: Vec::new(),
                morph_tracks: vec![OldMorph {
                    target: 2,
                    weights: ramp(0.0, 1.0),
                }],
                notifies: Vec::new(),
                curves: Vec::new(),
                property_tracks: Vec::new(),
                root_motion: None,
            };
            let bytes = postcard::to_allocvec(&old).expect("encode v1");
            assert_eq!(&bytes[..4], CLIP_PAYLOAD_MAGIC, "the collision under test");
            assert_eq!(AnimationClip::cooked_payload_version(&bytes), 1);
            let clip = AnimationClip::from_cooked_payload(&bytes).expect("v1 loads");
            assert_eq!(clip.name, name);
            assert_eq!(clip.morph_tracks, vec![MorphTrack::new(2, ramp(0.0, 1.0))]);
            let current = clip.to_cooked_payload().expect("encode");
            assert_eq!(
                AnimationClip::decode_cooked_payload(&current).expect("decode"),
                (CLIP_FORMAT_VERSION, clip)
            );
        }
    }

    #[test]
    fn node_tracks_sample_separately_from_unbound_tracks() {
        let mut clip = AnimationClip::empty("mix", 1.0);
        let a = MorphNode {
            index: 0,
            name: "a".into(),
        };
        let b = MorphNode {
            index: 1,
            name: "b".into(),
        };
        clip.morph_tracks.push(MorphTrack::for_node(a, 0, ramp(0.0, 1.0)));
        clip.morph_tracks.push(MorphTrack::for_node(b, 0, ramp(1.0, 0.0)));
        clip.morph_tracks
            .push(MorphTrack::new(5, TrackCurve::Static(0.25)));
        let mut nodes = Vec::new();
        clip.sample_node_morphs(0.25, &mut nodes);
        assert_eq!(nodes, vec![(0, 0, 0.25), (1, 0, 0.75)]);
        let mut plain = Vec::new();
        clip.sample_morphs(0.25, &mut plain);
        assert_eq!(plain, vec![(5, 0.25)]);
        let sliced = clip.slice("s", 0.5, 1.0);
        assert!(sliced.morph_tracks[0].node.is_some());
        assert!(sliced.morph_tracks[2].node.is_none());
    }

    /// A root joint that turns `turn` radians about +Y and slides `slide` metres along −Z over
    /// `duration` seconds, with root motion extracted in `mode`.
    fn turning_clip(duration: f32, turn: f32, slide: f32, mode: RootMotionMode) -> AnimationClip {
        let mut clip = AnimationClip::empty("turn", duration);
        let times: Vec<f32> = (0..=40).map(|k| k as f32 * duration / 40.0).collect();
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times
                    .iter()
                    .map(|t| Vec3::new(0.0, 0.0, -slide * t / duration))
                    .collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times: times.clone(),
                values: times
                    .iter()
                    .map(|t| Quat::from_rotation_y(turn * t / duration))
                    .collect(),
                exact_rotation: true,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack { joint: 0, mode });
        clip
    }

    fn yaw_of(q: Quat) -> f32 {
        q.to_euler(regia_core::glam::EulerRot::YXZ).0
    }

    #[test]
    fn looping_turn_keeps_turning_across_the_wrap() {
        // 90° per lap. Crossing the loop point from 0.9 to 0.1 s turns 0.2 laps = 18° forward;
        // the old composition reused the end rotation and swung back by 72°.
        let quarter = std::f32::consts::FRAC_PI_2;
        let clip = turning_clip(1.0, quarter, 0.0, RootMotionMode::Yaw);
        let (_, rotation) = clip.root_motion_delta(0.9, 0.1, true).expect("root motion");
        let yaw = yaw_of(rotation);
        assert!((yaw - 0.2 * quarter).abs() < 1e-3, "yaw across the wrap {yaw}");
        // Not wrapping: the plain difference.
        let (_, rotation) = clip.root_motion_delta(0.2, 0.5, false).expect("root motion");
        assert!((yaw_of(rotation) - 0.3 * quarter).abs() < 1e-3);
    }

    #[test]
    fn step_counts_laps_in_both_directions() {
        let step = ClipStep::advance(0.9, 2.3, 1.0, true);
        assert!((step.time - 0.2).abs() < 1e-4, "{step:?}");
        assert_eq!(step.loops, 3);
        assert!(!step.is_reverse());
        let step = ClipStep::advance(0.1, -0.3, 1.0, true);
        assert!((step.time - 0.8).abs() < 1e-4, "{step:?}");
        assert_eq!(step.loops, -1);
        assert!(step.is_reverse());
        let step = ClipStep::advance(0.5, -0.2, 1.0, true);
        assert_eq!(step.loops, 0);
        assert!(step.is_reverse());
        // Clips that do not loop clamp at both ends; old `advance` let reverse time go negative.
        assert_eq!(ClipStep::advance(0.2, -1.0, 1.0, false).time, 0.0);
        assert_eq!(AnimationClip::advance(0.2, -1.0, 1.0, false), (0.0, false));
        assert_eq!(ClipStep::advance(0.2, 5.0, 1.0, false).time, 1.0);
        // Garbage in, a well-formed step out.
        let step = ClipStep::advance(f32::NAN, f32::INFINITY, 1.0, true);
        assert_eq!((step.prev, step.time, step.loops), (0.0, 0.0, 0));
        assert_eq!(ClipStep::advance(0.5, 0.1, f32::NAN, true), ClipStep::default());
        assert_eq!(ClipStep::advance(0.5, 0.1, 0.0, true), ClipStep::default());
        // An absurd delta saturates instead of overflowing.
        let step = ClipStep::advance(0.0, 1.0e30, 1.0, true);
        assert!(step.loops > 0 && step.time >= 0.0 && step.time < 1.0, "{step:?}");
    }

    #[test]
    fn a_long_step_counts_every_lap_of_root_motion() {
        // 1 m per 1 s lap. A 2.75 s step from 0.5 s (a hitch, or a far LOD batching frames)
        // travels 2.75 m; the old wrap logic counted at most one lap (0.75 m).
        let clip = turning_clip(1.0, 0.0, 1.0, RootMotionMode::XZ);
        let step = clip.step(0.5, 2.75, true);
        assert_eq!(step.loops, 3);
        let (translation, _) = clip.root_motion_delta_step(&step).expect("root motion");
        assert!((translation.z + 2.75).abs() < 1e-3, "{translation}");
        // Three quarter-turn laps compose to 270°.
        let quarter = std::f32::consts::FRAC_PI_2;
        let turning = turning_clip(1.0, quarter, 0.0, RootMotionMode::Yaw);
        let (_, rotation) = turning
            .root_motion_delta_step(&turning.step(0.0, 3.0, true))
            .expect("root motion");
        assert!(
            crate::compress::rotation_error(rotation, Quat::from_rotation_y(3.0 * quarter)) < 1e-3,
            "{rotation:?}"
        );
    }

    #[test]
    fn reverse_playback_moves_root_motion_backwards() {
        let clip = turning_clip(1.0, 0.0, 1.0, RootMotionMode::XZ);
        // 0.1 s back to 0.8 s across the start: −0.3 m of travel, so +0.3 m along +Z.
        let step = clip.step(0.1, -0.3, true);
        let (translation, _) = clip.root_motion_delta_step(&step).expect("root motion");
        assert!((translation.z - 0.3).abs() < 1e-3, "{translation}");
        let step = clip.step(0.6, -0.2, true);
        let (translation, _) = clip.root_motion_delta_step(&step).expect("root motion");
        assert!((translation.z - 0.2).abs() < 1e-3, "{translation}");
    }

    /// [`turning_clip`] with a constant `pitch` about X under the turn: `q(t) = Ry(θ(t))·Rx(pitch)`
    /// (a Z-up import has pitch −90°).
    fn pitched_turning_clip(duration: f32, turn: f32, pitch: f32) -> AnimationClip {
        let mut clip = turning_clip(duration, turn, 0.0, RootMotionMode::Yaw);
        if let TrackCurve::Animated { values, .. } = &mut clip.tracks[0].rotations {
            for q in values.iter_mut() {
                *q *= Quat::from_rotation_x(pitch);
            }
        }
        clip
    }

    #[test]
    fn yaw_root_motion_is_the_twist_about_y_on_a_pitched_root() {
        // ADR-2604: a root pitched −90° (Z-up) or 89° turning 90° about +Y over 1 s. The old
        // Euler YXZ yaw of the joint-local delta read 0 at −90° (the local delta turns about Z)
        // and pinning, in gimbal lock, turned the pitch into a roll.
        let quarter = std::f32::consts::FRAC_PI_2;
        for pitch in [-quarter, 89f32.to_radians(), -0.3, 0.0] {
            let clip = pitched_turning_clip(1.0, quarter, pitch);
            let (_, rotation) = clip.root_motion_delta(0.2, 0.5, false).expect("root motion");
            let expected = Quat::from_rotation_y(0.3 * quarter);
            assert!(
                crate::compress::rotation_error(rotation, expected) < 1e-3,
                "pitch {pitch}: {rotation:?}"
            );
            // Across a wrap and over several laps the turn keeps adding up.
            let step = clip.step(0.9, 2.3, true);
            let (_, rotation) = clip.root_motion_delta_step(&step).expect("root motion");
            let (_, rotation) = filter_root_motion(RootMotionMode::Yaw, Vec3::ZERO, rotation);
            assert!(
                crate::compress::rotation_error(rotation, Quat::from_rotation_y(2.3 * quarter)) < 1e-3,
                "pitch {pitch}, laps {}: {rotation:?}",
                step.loops
            );
            // Pinning removes exactly the turn: the pitch stays, and the pinned rotation turned
            // by the reported yaw since time zero is the clip's rotation.
            let mut pose = Pose::from_identity(1);
            for t in [0.0, 0.25, 0.5, 0.99] {
                clip.sample_pose(t, &mut pose);
                let sampled = pose.rotations[0];
                clip.pin_root_in_place(&mut pose, RootMotionMode::Yaw);
                let pinned = pose.rotations[0];
                assert!(
                    crate::compress::rotation_error(pinned, Quat::from_rotation_x(pitch)) < 1e-3,
                    "pitch {pitch} t {t}: pinned {pinned:?}"
                );
                let (_, turned) = clip.root_motion_delta(0.0, t, false).expect("root motion");
                assert!(
                    crate::compress::rotation_error(turned * pinned, sampled) < 1e-3,
                    "pitch {pitch} t {t}: turn·pinned != clip"
                );
            }
        }
    }

    /// Audit M2 (read-only determinism audit of `animation3`): a playback step that crosses the
    /// clip end runs the lap path (`quat_pow`), and the filtered yaw quaternion must be pinned
    /// to the software kernels' bits on every target.
    #[test]
    fn yaw_root_motion_lap_is_pinned_to_software_bits() {
        let quarter = std::f32::consts::FRAC_PI_2;
        let sqrt_half = 0.707_106_77_f32; // nearest f32 to √2/2 (sin(π/4) == cos(π/4))
        let mut clip = AnimationClip::empty("turn", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::IDENTITY, Quat::from_xyzw(0.0, sqrt_half, 0.0, sqrt_half)],
                exact_rotation: true,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::Yaw,
        });
        // 0.9 → 0.1 across the clip end: one lap, the path `quat_pow` serves.
        let step = clip.step(0.9, 0.2, true);
        assert_eq!(step.loops, 1);
        let (_, rotation) = clip.root_motion_delta_step(&step).expect("root motion");
        let (_, yaw) = filter_root_motion(RootMotionMode::Yaw, Vec3::ZERO, rotation);
        assert_eq!(
            [yaw.x.to_bits(), yaw.y.to_bits(), yaw.z.to_bits(), yaw.w.to_bits()],
            [0x0000_0000, 0x3e20_305e, 0x0000_0000, 0x3f7c_d925],
            "the lap yaw quaternion must be bit-identical on every target"
        );
        // Accuracy guard: 0.2 s of the turn at a quarter-turn per second.
        assert!(
            crate::compress::rotation_error(yaw, Quat::from_rotation_y(0.2 * quarter)) < 1e-4,
            "lap yaw angle"
        );
    }

    /// Audit (corr-animation): an empty animated translation track pinned the root to the
    /// sampling placeholder (1, 1, 1).
    #[test]
    fn pinning_skips_a_malformed_root_track() {
        let mut clip = AnimationClip::empty("bad", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![],
                values: vec![],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XYZ,
        });
        let mut pose = Pose::from_identity(1);
        pose.translations[0] = Vec3::new(0.0, 2.0, 0.0);
        clip.pin_root_in_place(&mut pose, RootMotionMode::XYZ);
        assert_eq!(pose.translations[0], Vec3::new(0.0, 2.0, 0.0));
        clip.pin_root_in_place(&mut pose, RootMotionMode::XZ);
        assert_eq!(pose.translations[0], Vec3::new(0.0, 2.0, 0.0));
    }

    #[test]
    fn twist_yaw_is_additive_under_a_yaw_and_ignores_the_swing() {
        let swing = Quat::from_axis_angle(Vec3::new(1.0, 0.0, 1.0).normalize(), -1.2);
        for yaw in [-3.0f32, -1.0, 0.0, 0.5, 2.0, 3.1] {
            let q = Quat::from_rotation_y(yaw) * swing;
            assert!(
                (wrap_pi(twist_yaw(q) - yaw)).abs() < 1e-5,
                "yaw {yaw}: {}",
                twist_yaw(q)
            );
            assert!((wrap_pi(twist_yaw(-q) - yaw)).abs() < 1e-5, "−q, yaw {yaw}");
        }
        // A half turn about a horizontal axis has no twist.
        assert_eq!(twist_yaw(Quat::from_rotation_x(std::f32::consts::PI)), 0.0);
    }

    fn clip_with_notifies(times: &[f32]) -> AnimationClip {
        let mut clip = AnimationClip::empty("n", 1.0);
        clip.notifies = times
            .iter()
            .map(|&time| Notify {
                time,
                name: format!("at{time}"),
                payload: None,
            })
            .collect();
        clip
    }

    fn names(fired: &[Notify]) -> Vec<&str> {
        fired.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn a_notify_at_zero_fires_every_lap() {
        // A footstep keyed at the very start of a loop never fired before: (0, time] skipped it.
        let clip = clip_with_notifies(&[0.0]);
        let mut fired = Vec::new();
        clip.notifies_crossed(0.9, 0.1, true, &mut fired);
        assert_eq!(names(&fired), ["at0"]);
        fired.clear();
        clip.notifies_crossed(0.1, 0.4, false, &mut fired);
        assert!(fired.is_empty());
        // The first step of a clip that just started also fires a notify at its start.
        fired.clear();
        clip.notifies_in_step(&clip.step(0.0, 0.1, true), true, &mut fired);
        assert_eq!(names(&fired), ["at0"]);
        fired.clear();
        clip.notifies_in_step(&clip.step(0.0, 0.1, true), false, &mut fired);
        assert!(fired.is_empty());
    }

    #[test]
    fn multi_lap_steps_fire_each_notify_once_per_lap_in_order() {
        let clip = clip_with_notifies(&[0.25, 0.75]);
        let mut fired = Vec::new();
        // 0.5 → 2.5 laps worth (ends at 0.5 after two wraps): 0.75, 0.25, 0.75, 0.25.
        clip.notifies_in_step(&clip.step(0.5, 2.0, true), false, &mut fired);
        assert_eq!(names(&fired), ["at0.75", "at0.25", "at0.75", "at0.25"]);
        // Reverse: 0.8 back to 0.1 passes 0.75 then 0.25.
        fired.clear();
        clip.notifies_in_step(&clip.step(0.8, -0.7, true), false, &mut fired);
        assert_eq!(names(&fired), ["at0.75", "at0.25"]);
        // Reverse across the start: 0.3 back past 0 to 0.8 passes 0.25 and stops before 0.75.
        fired.clear();
        clip.notifies_in_step(&clip.step(0.3, -0.5, true), false, &mut fired);
        assert_eq!(names(&fired), ["at0.25"]);
        // A runaway step is capped per notify.
        fired.clear();
        clip.notifies_in_step(&clip.step(0.0, 1000.0, true), false, &mut fired);
        assert_eq!(fired.len(), 2 * MAX_NOTIFY_REPEATS as usize);
    }

    #[test]
    fn slice_survives_non_finite_bounds() {
        let mut clip = AnimationClip::empty("c", 2.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![Vec3::ZERO, Vec3::X],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let slice = clip.slice("nan", f32::NAN, f32::NAN);
        assert!(
            (slice.duration - 2.0).abs() < 1e-6,
            "NaN bounds slice the whole clip"
        );
        let empty = clip.slice("backwards", 1.5, 0.5);
        assert_eq!(empty.duration, 0.0);
    }
}
