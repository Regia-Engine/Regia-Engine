// SPDX-License-Identifier: MPL-2.0
//! Object-space animation clip compression: curve-fitting keyframe reduction with an
//! object-space error metric, then quantised storage.
//!
//! This is the published approach of the Animation Compression Library (ACL) family of
//! compressors, re-derived from the public descriptions (see `docs/provenance.md`):
//!
//! - Error is measured where it is *seen*: on probe points carried by each bone, in model
//!   (object) space, after forward kinematics. A per-bone "shell" distance stands in for the
//!   farthest skinned vertex from the bone origin (default 1 unit), so a rotation error is
//!   charged for the lever arm it actually swings.
//! - A parent bone's approximation is charged at its descendants' probes too, so a small
//!   local rotation error at the root that swings a distant hand is treated as the large
//!   object-space error it is. Bones are processed root-first; when a bone is reduced, its
//!   ancestors are already compressed, and the displacement measured at a descendant's
//!   probes is the *cumulative* displacement caused by every compressed ancestor plus this
//!   bone. Each channel of each animated ancestor (the joint itself included) is allowed an
//!   equal share (`1 / (animated ancestors × 3 channels)`) of each descendant's tolerance, so
//!   by the triangle inequality over successive single-bone
//!   replacements the final object-space error at any probe stays within tolerance. A final
//!   whole-clip verification pass measures the real error and re-inserts keys (or upgrades a
//!   rotation format) if it ever exceeds tolerance.
//! - Reduction is iterative least-error removal (the repeated-elimination idea of
//!   Visvalingam & Whyatt, applied per track): repeatedly remove the interior key whose
//!   removal causes the least object-space error, while the error stays inside budget.
//!   Keys are quantised *before* reduction, and the chord is judged against the values the
//!   decompressor will actually reconstruct, so quantisation error is inside the budget.
//! - Storage: rotations use smallest-three packing, 48 bits (2-bit dropped index plus
//!   three 15-bit components) or 32 bits (2 plus three 10-bit), chosen per track by the
//!   object-space displacement its packing error would cause. Translations and scales use
//!   per-track range-normalised 16-bit keys. Constant tracks collapse to one value; tracks
//!   that reproduce the bind pose within tolerance are dropped entirely.
//!
//! Decompression ([`CompressedClip::sample`]) is linear interpolation for translations and
//! scales and normalised lerp for rotations. It is a pure function of the stored bytes and
//! the sample time (no hashing, no clocks), so it is bit-deterministic.
//!
//! Scope: this module compresses the skeletal TRS tracks of an [`AnimationClip`]. Morph
//! tracks, notifies, named curves and property tracks are not carried by this format; cook
//! those through [`crate::compress`] if a clip needs them. Complexity is cook-time:
//! removing keys costs `O(keys^2 * subtree probes)` per track in the worst case.
//!
//! # Example
//!
//! ```ignore
//! use regia_animation::clip_compress::{compress, CompressOptions};
//! use regia_animation::pose::Pose;
//!
//! let options = CompressOptions::default();
//! let cooked = compress(&clip, &skeleton, &options)?;
//! let mut pose = skeleton.bind_pose_pose();
//! cooked.sample(0.25, &mut pose);
//! assert!(cooked.report().max_error <= options.tolerance);
//! ```

use regia_core::math::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::clip::{AnimationClip, TrackCurve};
use crate::pose::Pose;
use crate::skeleton::{Skeleton, SkeletonError};

/// The clip type compressed here: the crate's skeletal [`AnimationClip`].
pub type Clip = AnimationClip;

/// Errors returned by [`compress`].
#[derive(Debug, thiserror::Error)]
pub enum CompressError {
    /// The skeleton failed structural validation.
    #[error("invalid skeleton: {0}")]
    Skeleton(#[from] SkeletonError),
    /// A tolerance was not a positive finite number.
    #[error("tolerance must be positive and finite, got {value}")]
    InvalidTolerance {
        /// The offending value.
        value: f32,
    },
    /// A shell distance was negative or not finite.
    #[error("shell distance must be non-negative and finite, got {value}")]
    InvalidShell {
        /// The offending value.
        value: f32,
    },
    /// The clip duration (or sample rate) was negative or not finite.
    #[error("clip duration/sample-rate must be non-negative and finite, got {value}")]
    InvalidDuration {
        /// The offending value.
        value: f32,
    },
    /// A key value contained a NaN or infinite component.
    #[error("joint {joint} {channel} key {key} has a non-finite value")]
    NonFiniteValue {
        /// The joint whose track is offending.
        joint: u32,
        /// Which channel: `"translations"`, `"rotations"` or `"scales"`.
        channel: &'static str,
        /// Index of the offending key.
        key: usize,
    },
    /// A key time was NaN or infinite.
    #[error("joint {joint} {channel} key {key} has a non-finite time")]
    NonFiniteTime {
        /// The joint whose track is offending.
        joint: u32,
        /// Which channel.
        channel: &'static str,
        /// Index of the offending key.
        key: usize,
    },
    /// A bind-pose component was NaN or infinite.
    #[error("joint {joint} bind pose has a non-finite component")]
    NonFiniteBind {
        /// The offending joint.
        joint: usize,
    },
    /// A rotation key was the zero quaternion (or so small it cannot be normalised).
    #[error("joint {joint} rotation key {key} is a degenerate quaternion")]
    DegenerateQuaternion {
        /// The joint whose track is offending.
        joint: u32,
        /// Index of the offending key.
        key: usize,
    },
    /// An animated curve's time and value arrays disagree in length.
    #[error("joint {joint} {channel} has {times} times but {values} values")]
    MismatchedKeys {
        /// The joint whose track is offending.
        joint: u32,
        /// Which channel.
        channel: &'static str,
        /// Number of key times.
        times: usize,
        /// Number of key values.
        values: usize,
    },
    /// Key times must be strictly increasing.
    #[error("joint {joint} {channel} key {key} does not increase in time")]
    NonIncreasingTimes {
        /// The joint whose track is offending.
        joint: u32,
        /// Which channel.
        channel: &'static str,
        /// Index of the offending key.
        key: usize,
    },
    /// A track targets a joint the skeleton does not have.
    #[error("track targets joint {joint} but the skeleton has {joints} joints")]
    JointOutOfRange {
        /// The offending joint id.
        joint: u32,
        /// Number of joints in the skeleton.
        joints: usize,
    },
    /// Two tracks target the same joint.
    #[error("clip has more than one track for joint {joint}")]
    DuplicateTrack {
        /// The duplicated joint id.
        joint: u32,
    },
}

/// Storage width of a smallest-three packed rotation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RotationFormat {
    /// 2-bit dropped-component index plus three 15-bit components: 47 bits used, stored
    /// (and charged) as 48 bits. Worst-case packing error is about 2e-4 rad.
    Bits48,
    /// 2-bit dropped-component index plus three 10-bit components: exactly 32 bits.
    /// Worst-case packing error is about 6e-3 rad.
    Bits32,
}

impl RotationFormat {
    /// Bits charged per key of this format (48 or 32).
    pub fn bits(self) -> u32 {
        match self {
            RotationFormat::Bits48 => 48,
            RotationFormat::Bits32 => 32,
        }
    }

    /// Bytes charged per key of this format (6 or 4).
    pub fn bytes(self) -> usize {
        (self.bits() / 8) as usize
    }

    /// Bits per stored component (15 or 10).
    fn component_bits(self) -> u32 {
        match self {
            RotationFormat::Bits48 => 15,
            RotationFormat::Bits32 => 10,
        }
    }
}

/// How [`compress`] picks a [`RotationFormat`] per rotation track.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RotationFormatPreference {
    /// Per track: use 32 bits when the measured packing error, swung over the track's
    /// longest lever arm in the skeleton, stays within a quarter of the strictest
    /// tolerance in the bone's subtree; otherwise 48 bits.
    #[default]
    Auto,
    /// Always use the 48-bit format.
    Force48,
    /// Always use the 32-bit format (the error budget then decides how many keys survive;
    /// the report may exceed tolerance if packing error alone does).
    Force32,
}

/// Options for [`compress`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressOptions {
    /// Default maximum object-space error per bone, in the skeleton's length units
    /// (metres for a metre skeleton). Must be positive and finite.
    pub tolerance: f32,
    /// Per-joint tolerance overrides, indexed by joint id. Joints past the end of this
    /// list (or an empty list) use [`CompressOptions::tolerance`].
    pub per_bone_tolerance: Vec<f32>,
    /// Default shell distance: the distance from a bone's origin at which its probe
    /// points sit, standing in for its farthest skinned vertices. Must be non-negative.
    pub shell_radius: f32,
    /// Per-joint shell overrides, indexed by joint id.
    pub per_bone_shell: Vec<f32>,
    /// Rotation storage selection.
    pub rotation_format: RotationFormatPreference,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            tolerance: 0.01,
            per_bone_tolerance: Vec::new(),
            shell_radius: 1.0,
            per_bone_shell: Vec::new(),
            rotation_format: RotationFormatPreference::Auto,
        }
    }
}

impl CompressOptions {
    /// Options with the given default tolerance and everything else defaulted.
    pub fn new(tolerance: f32) -> Self {
        Self {
            tolerance,
            ..Self::default()
        }
    }

    /// The tolerance in force for `joint`.
    pub fn tolerance_for(&self, joint: usize) -> f32 {
        self.per_bone_tolerance
            .get(joint)
            .copied()
            .unwrap_or(self.tolerance)
    }

    /// The shell distance in force for `joint`.
    pub fn shell_for(&self, joint: usize) -> f32 {
        self.per_bone_shell
            .get(joint)
            .copied()
            .unwrap_or(self.shell_radius)
    }
}

/// A quantised translation or scale track: 16-bit keys relative to the track range.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Vec3Track {
    /// The track reproduces the bind value within tolerance and was dropped: sampling
    /// leaves the pose component untouched (start from the bind pose to get bind values).
    Dropped,
    /// The track is constant: one stored value, exact.
    Constant(Vec3),
    /// Timed quantised keys.
    Animated {
        /// Key times in seconds, strictly increasing.
        times: Vec<f32>,
        /// Quantised keys: each component normalised to the track's `[min, max]` range.
        keys: Vec<[u16; 3]>,
        /// Per-axis minimum of the original track range.
        min: Vec3,
        /// Per-axis maximum of the original track range.
        max: Vec3,
    },
}

impl Vec3Track {
    /// Number of stored keys (0 dropped, 1 constant).
    pub fn key_count(&self) -> usize {
        match self {
            Vec3Track::Dropped => 0,
            Vec3Track::Constant(_) => 1,
            Vec3Track::Animated { keys, .. } => keys.len(),
        }
    }

    /// True when the track was dropped (sampling leaves the pose untouched).
    pub fn is_dropped(&self) -> bool {
        matches!(self, Vec3Track::Dropped)
    }

    /// Bytes charged for this track under the size model of
    /// [`CompressedClip::compressed_bytes`].
    fn byte_cost(&self) -> usize {
        match self {
            Vec3Track::Dropped => 1,
            Vec3Track::Constant(_) => 1 + 12,
            Vec3Track::Animated { times, keys, .. } => 1 + times.len() * 4 + keys.len() * 6 + 24,
        }
    }

    /// The value at `time`, or `None` when dropped (the caller leaves the pose alone).
    fn sample(&self, time: f32) -> Option<Vec3> {
        match self {
            Vec3Track::Dropped => None,
            Vec3Track::Constant(v) => Some(*v),
            Vec3Track::Animated {
                times,
                keys,
                min,
                max,
            } => {
                if keys.is_empty() || times.is_empty() {
                    return None;
                }
                let n = times.len().min(keys.len());
                let (lo, hi, w) = bracket(&times[..n], time);
                let a = dequantize_vec3(keys[lo], *min, *max);
                let b = dequantize_vec3(keys[hi], *min, *max);
                Some(a.lerp(b, w))
            }
        }
    }
}

/// A quantised rotation track: smallest-three packed keys.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum RotationTrack {
    /// The track reproduces the bind rotation within tolerance and was dropped.
    Dropped,
    /// The track is constant: one packed key.
    Constant {
        /// Packing format of `key`.
        format: RotationFormat,
        /// The packed rotation (only the low `format.bits()` bits are meaningful).
        key: u64,
    },
    /// Timed keys in the 48-bit format.
    Animated48 {
        /// Key times in seconds, strictly increasing.
        times: Vec<f32>,
        /// Packed keys (low 47 bits meaningful).
        keys: Vec<u64>,
    },
    /// Timed keys in the 32-bit format.
    Animated32 {
        /// Key times in seconds, strictly increasing.
        times: Vec<f32>,
        /// Packed keys.
        keys: Vec<u32>,
    },
}

impl RotationTrack {
    /// Number of stored keys (0 dropped, 1 constant).
    pub fn key_count(&self) -> usize {
        match self {
            RotationTrack::Dropped => 0,
            RotationTrack::Constant { .. } => 1,
            RotationTrack::Animated48 { keys, .. } => keys.len(),
            RotationTrack::Animated32 { keys, .. } => keys.len(),
        }
    }

    /// True when the track was dropped.
    pub fn is_dropped(&self) -> bool {
        matches!(self, RotationTrack::Dropped)
    }

    /// The packing format in use, or `None` when dropped.
    pub fn format(&self) -> Option<RotationFormat> {
        match self {
            RotationTrack::Dropped => None,
            RotationTrack::Constant { format, .. } => Some(*format),
            RotationTrack::Animated48 { .. } => Some(RotationFormat::Bits48),
            RotationTrack::Animated32 { .. } => Some(RotationFormat::Bits32),
        }
    }

    /// Bytes charged for this track under the size model.
    fn byte_cost(&self) -> usize {
        match self {
            RotationTrack::Dropped => 1,
            RotationTrack::Constant { format, .. } => 1 + format.bytes(),
            RotationTrack::Animated48 { times, keys } => 1 + times.len() * 4 + keys.len() * 6,
            RotationTrack::Animated32 { times, keys } => 1 + times.len() * 4 + keys.len() * 4,
        }
    }

    /// The rotation at `time` (nlerp between unpacked keys), or `None` when dropped.
    fn sample(&self, time: f32) -> Option<Quat> {
        match self {
            RotationTrack::Dropped => None,
            RotationTrack::Constant { format, key } => Some(unpack_smallest_three(*key, *format)),
            RotationTrack::Animated48 { times, keys } => {
                if keys.is_empty() || times.is_empty() {
                    return None;
                }
                let n = times.len().min(keys.len());
                let (lo, hi, w) = bracket(&times[..n], time);
                let a = unpack_rotation_48(keys[lo]);
                let b = unpack_rotation_48(keys[hi]);
                Some(normalize_quat(a.lerp(b, w)))
            }
            RotationTrack::Animated32 { times, keys } => {
                if keys.is_empty() || times.is_empty() {
                    return None;
                }
                let n = times.len().min(keys.len());
                let (lo, hi, w) = bracket(&times[..n], time);
                let a = unpack_rotation_32(keys[lo]);
                let b = unpack_rotation_32(keys[hi]);
                Some(normalize_quat(a.lerp(b, w)))
            }
        }
    }
}

/// The three compressed channels of one joint.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressedJointTrack {
    /// Target joint id in the skeleton.
    pub joint: u32,
    /// Compressed translation channel.
    pub translations: Vec3Track,
    /// Compressed rotation channel.
    pub rotations: RotationTrack,
    /// Compressed scale channel.
    pub scales: Vec3Track,
}

impl CompressedJointTrack {
    /// Total number of stored keys over the three channels.
    pub fn key_count(&self) -> usize {
        self.translations.key_count() + self.rotations.key_count() + self.scales.key_count()
    }

    /// Bytes charged for this joint under the size model (4-byte joint id plus channels).
    fn byte_cost(&self) -> usize {
        4 + self.translations.byte_cost() + self.rotations.byte_cost() + self.scales.byte_cost()
    }
}

/// What [`compress`] achieved: sizes under a stated model and measured object-space error.
///
/// Size model (deterministic, independent of serde encoding): the original clip is charged
/// raw `f32` storage for its skeletal tracks (4 bytes per key time, 12 per `Vec3` key,
/// 16 per quaternion key, one stored value per static curve) plus 16 header bytes and the
/// clip name. The compressed clip is charged by [`CompressedClip::compressed_bytes`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressionReport {
    /// Raw size of the original skeletal tracks under the size model.
    pub original_bytes: usize,
    /// Compressed size under the size model.
    pub compressed_bytes: usize,
    /// Number of keys in the original skeletal tracks (1 per static curve).
    pub original_keys: usize,
    /// Number of stored keys after compression (0 for dropped, 1 for constant).
    pub compressed_keys: usize,
    /// Largest measured object-space probe displacement over all bones and sampled times.
    pub max_error: f32,
    /// Measured maximum object-space probe displacement per bone, indexed by joint id.
    pub max_error_per_bone: Vec<f32>,
}

impl CompressionReport {
    /// Compressed size as a fraction of the original size (1.0 when the original is empty).
    pub fn ratio(&self) -> f32 {
        if self.original_bytes == 0 {
            1.0
        } else {
            self.compressed_bytes as f32 / self.original_bytes as f32
        }
    }
}

/// A clip compressed by [`compress`]: quantised, reduced skeletal tracks plus its report.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressedClip {
    /// Clip name (copied from the source clip).
    pub name: String,
    /// Duration in seconds (copied from the source clip).
    pub duration: f32,
    /// Compressed per-joint tracks, sorted by joint id. Joints the source clip did not
    /// animate have no entry and are never touched by sampling.
    pub tracks: Vec<CompressedJointTrack>,
    report: CompressionReport,
}

impl CompressedClip {
    /// Sample the clip at `time` seconds into `pose`.
    ///
    /// Only channels that were not dropped write to the pose; start from the bind pose
    /// ([`Skeleton::bind_pose_pose`]) so dropped channels yield bind values. Times outside
    /// a track's key range clamp to its end keys; a non-finite time samples at 0. Joints
    /// past the end of a (possibly malformed) pose are skipped without panicking. The
    /// result is a pure function of the stored data and `time`, bit-deterministic.
    pub fn sample(&self, time: f32, pose: &mut Pose) {
        let time = if time.is_finite() { time } else { 0.0 };
        for track in &self.tracks {
            let i = track.joint as usize;
            if let Some(value) = track.translations.sample(time)
                && let Some(slot) = pose.translations.get_mut(i)
            {
                *slot = value;
            }
            if let Some(value) = track.rotations.sample(time)
                && let Some(slot) = pose.rotations.get_mut(i)
            {
                *slot = value;
            }
            if let Some(value) = track.scales.sample(time)
                && let Some(slot) = pose.scales.get_mut(i)
            {
                *slot = value;
            }
        }
    }

    /// The compression report measured by [`compress`].
    pub fn report(&self) -> &CompressionReport {
        &self.report
    }

    /// Compressed size under the size model: 16 header bytes, the clip name, and per joint
    /// 4 bytes plus its channels (1 tag byte each; a constant `Vec3` 12 bytes, a constant
    /// rotation its format width; animated keys 4 bytes per time plus 6 per `Vec3` key
    /// (and 24 for its range), 6 per 48-bit rotation key, 4 per 32-bit rotation key).
    pub fn compressed_bytes(&self) -> usize {
        16 + self.name.len()
            + self
                .tracks
                .iter()
                .map(CompressedJointTrack::byte_cost)
                .sum::<usize>()
    }

    /// The compressed track for `joint`, when the source clip animated it.
    pub fn track_for(&self, joint: u32) -> Option<&CompressedJointTrack> {
        self.tracks.iter().find(|t| t.joint == joint)
    }

    /// Total number of stored keys over all tracks.
    pub fn key_count(&self) -> usize {
        self.tracks.iter().map(CompressedJointTrack::key_count).sum()
    }
}

/// Segment bracketing `time` in strictly increasing `times`: `(lo, hi, weight)`, clamped.
/// A NaN time yields the first key. Callers guarantee `times` is non-empty.
fn bracket(times: &[f32], time: f32) -> (usize, usize, f32) {
    if times.len() <= 1 || time.is_nan() || time <= times[0] {
        return (0, 0, 0.0);
    }
    let last = times.len() - 1;
    if time >= times[last] {
        return (last, last, 0.0);
    }
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
    let w = if span > 0.0 {
        (time - times[lo]) / span
    } else {
        0.0
    };
    (lo, hi, w)
}

/// Normalise, mapping a zero/non-finite quaternion to identity instead of propagating NaN.
fn normalize_quat(q: Quat) -> Quat {
    let len_sq = q.length_squared();
    if len_sq.is_finite() && len_sq > 1e-12 {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// Pack `q` smallest-three into the low bits of a `u64`: bits 0..2 the dropped (largest
/// absolute) component index, then the other three components in xyzw order, each mapped
/// from `[-1/sqrt(2), 1/sqrt(2)]` to `component_bits` unsigned levels. The quaternion's
/// sign is flipped so the dropped component is non-negative and `sqrt` recovers it.
fn pack_smallest_three(q: Quat, format: RotationFormat) -> u64 {
    let q = normalize_quat(q);
    let mut c = [q.x, q.y, q.z, q.w];
    let mut dropped = 0usize;
    for i in 1..4 {
        if c[i].abs() > c[dropped].abs() {
            dropped = i;
        }
    }
    if c[dropped] < 0.0 {
        for v in &mut c {
            *v = -*v;
        }
    }
    let bits = format.component_bits();
    let levels = ((1u64 << bits) - 1) as f32;
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
    let mut packed = dropped as u64;
    let mut shift = 2u32;
    for (i, &v) in c.iter().enumerate() {
        if i == dropped {
            continue;
        }
        let t = (v * inv_sqrt2 + 0.5).clamp(0.0, 1.0);
        let code = (t * levels).round() as u64;
        packed |= code << shift;
        shift += bits;
    }
    packed
}

/// Inverse of [`pack_smallest_three`].
fn unpack_smallest_three(packed: u64, format: RotationFormat) -> Quat {
    let bits = format.component_bits();
    let mask = (1u64 << bits) - 1;
    let levels = mask as f32;
    let dropped = (packed & 3) as usize;
    let sqrt2 = std::f32::consts::SQRT_2;
    let mut c = [0.0f32; 4];
    let mut sum_sq = 0.0f32;
    let mut shift = 2u32;
    for (i, slot) in c.iter_mut().enumerate() {
        if i == dropped {
            continue;
        }
        let code = ((packed >> shift) & mask) as f32;
        shift += bits;
        let v = (code / levels - 0.5) * sqrt2;
        *slot = v;
        sum_sq += v * v;
    }
    if dropped < 4 {
        c[dropped] = (1.0 - sum_sq).max(0.0).sqrt();
    }
    normalize_quat(Quat::from_xyzw(c[0], c[1], c[2], c[3]))
}

/// Pack in the 48-bit format.
fn pack_rotation_48(q: Quat) -> u64 {
    pack_smallest_three(q, RotationFormat::Bits48)
}

/// Pack in the 32-bit format.
fn pack_rotation_32(q: Quat) -> u32 {
    pack_smallest_three(q, RotationFormat::Bits32) as u32
}

/// Unpack a 48-bit packed rotation.
fn unpack_rotation_48(packed: u64) -> Quat {
    unpack_smallest_three(packed, RotationFormat::Bits48)
}

/// Unpack a 32-bit packed rotation.
fn unpack_rotation_32(packed: u32) -> Quat {
    unpack_smallest_three(u64::from(packed), RotationFormat::Bits32)
}

/// Quantise one scalar to 16 bits over `[min, max]` (0 when the range is degenerate).
fn quantize_scalar(v: f32, min: f32, max: f32) -> u16 {
    let range = max - min;
    if !range.is_finite() || range <= 0.0 {
        return 0;
    }
    let t = ((v - min) / range).clamp(0.0, 1.0);
    (t * 65535.0).round() as u16
}

/// Inverse of [`quantize_scalar`].
fn dequantize_scalar(q: u16, min: f32, max: f32) -> f32 {
    min + (f32::from(q) / 65535.0) * (max - min)
}

/// Quantise a vector per axis over the track range.
fn quantize_vec3(v: Vec3, min: Vec3, max: Vec3) -> [u16; 3] {
    [
        quantize_scalar(v.x, min.x, max.x),
        quantize_scalar(v.y, min.y, max.y),
        quantize_scalar(v.z, min.z, max.z),
    ]
}

/// Inverse of [`quantize_vec3`].
fn dequantize_vec3(q: [u16; 3], min: Vec3, max: Vec3) -> Vec3 {
    Vec3::new(
        dequantize_scalar(q[0], min.x, max.x),
        dequantize_scalar(q[1], min.y, max.y),
        dequantize_scalar(q[2], min.z, max.z),
    )
}

/// Validate one `Vec3` curve (`channel` names it in errors).
fn validate_vec_curve(
    joint: u32,
    channel: &'static str,
    curve: &TrackCurve<Vec3>,
) -> Result<(), CompressError> {
    match curve {
        TrackCurve::Static(v) => {
            if !v.is_finite() {
                return Err(CompressError::NonFiniteValue {
                    joint,
                    channel,
                    key: 0,
                });
            }
            Ok(())
        }
        TrackCurve::Animated { times, values, .. } => {
            if times.len() != values.len() {
                return Err(CompressError::MismatchedKeys {
                    joint,
                    channel,
                    times: times.len(),
                    values: values.len(),
                });
            }
            for (key, &t) in times.iter().enumerate() {
                if !t.is_finite() {
                    return Err(CompressError::NonFiniteTime { joint, channel, key });
                }
                if key > 0 && t <= times[key - 1] {
                    return Err(CompressError::NonIncreasingTimes { joint, channel, key });
                }
            }
            for (key, v) in values.iter().enumerate() {
                if !v.is_finite() {
                    return Err(CompressError::NonFiniteValue { joint, channel, key });
                }
            }
            Ok(())
        }
    }
}

/// Validate one quaternion curve.
fn validate_rot_curve(joint: u32, curve: &TrackCurve<Quat>) -> Result<(), CompressError> {
    let channel = "rotations";
    let check_value = |key: usize, q: &Quat| -> Result<(), CompressError> {
        if !q.is_finite() {
            return Err(CompressError::NonFiniteValue { joint, channel, key });
        }
        if q.length_squared() <= 1e-12 {
            return Err(CompressError::DegenerateQuaternion { joint, key });
        }
        Ok(())
    };
    match curve {
        TrackCurve::Static(q) => check_value(0, q),
        TrackCurve::Animated { times, values, .. } => {
            if times.len() != values.len() {
                return Err(CompressError::MismatchedKeys {
                    joint,
                    channel,
                    times: times.len(),
                    values: values.len(),
                });
            }
            for (key, &t) in times.iter().enumerate() {
                if !t.is_finite() {
                    return Err(CompressError::NonFiniteTime { joint, channel, key });
                }
                if key > 0 && t <= times[key - 1] {
                    return Err(CompressError::NonIncreasingTimes { joint, channel, key });
                }
            }
            for (key, q) in values.iter().enumerate() {
                check_value(key, q)?;
            }
            Ok(())
        }
    }
}

/// Validate options, skeleton bind pose, clip header and every skeletal track.
fn validate_inputs(clip: &Clip, skeleton: &Skeleton, options: &CompressOptions) -> Result<(), CompressError> {
    if !options.tolerance.is_finite() || options.tolerance <= 0.0 {
        return Err(CompressError::InvalidTolerance {
            value: options.tolerance,
        });
    }
    for &value in &options.per_bone_tolerance {
        if !value.is_finite() || value <= 0.0 {
            return Err(CompressError::InvalidTolerance { value });
        }
    }
    if !options.shell_radius.is_finite() || options.shell_radius < 0.0 {
        return Err(CompressError::InvalidShell {
            value: options.shell_radius,
        });
    }
    for &value in &options.per_bone_shell {
        if !value.is_finite() || value < 0.0 {
            return Err(CompressError::InvalidShell { value });
        }
    }
    if !clip.duration.is_finite() || clip.duration < 0.0 {
        return Err(CompressError::InvalidDuration { value: clip.duration });
    }
    if !clip.sample_rate.is_finite() || clip.sample_rate < 0.0 {
        return Err(CompressError::InvalidDuration {
            value: clip.sample_rate,
        });
    }
    skeleton.validate()?;
    for (joint, bind) in skeleton.bind_pose.iter().enumerate() {
        if !bind.translation.is_finite() || !bind.rotation.is_finite() || !bind.scale.is_finite() {
            return Err(CompressError::NonFiniteBind { joint });
        }
    }
    let joints = skeleton.len();
    let mut seen = vec![false; joints];
    for track in &clip.tracks {
        let joint = track.joint as usize;
        if joint >= joints {
            return Err(CompressError::JointOutOfRange {
                joint: track.joint,
                joints,
            });
        }
        if seen[joint] {
            return Err(CompressError::DuplicateTrack { joint: track.joint });
        }
        seen[joint] = true;
        validate_vec_curve(track.joint, "translations", &track.translations)?;
        validate_rot_curve(track.joint, &track.rotations)?;
        validate_vec_curve(track.joint, "scales", &track.scales)?;
    }
    Ok(())
}

/// Probe points of one bone: its origin plus the six shell axis points (only the origin
/// when the shell is zero, which still charges translation and descendant lever arms).
fn probe_points(shell: f32) -> Vec<Vec3> {
    if shell <= 0.0 {
        return vec![Vec3::ZERO];
    }
    vec![
        Vec3::ZERO,
        Vec3::new(shell, 0.0, 0.0),
        Vec3::new(-shell, 0.0, 0.0),
        Vec3::new(0.0, shell, 0.0),
        Vec3::new(0.0, -shell, 0.0),
        Vec3::new(0.0, 0.0, shell),
        Vec3::new(0.0, 0.0, -shell),
    ]
}

/// For each joint, itself and all descendants, in ascending joint order (deterministic).
fn descendants_of(skeleton: &Skeleton) -> Vec<Vec<usize>> {
    let n = skeleton.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, parent) in skeleton.parents.iter().enumerate() {
        if let Some(p) = parent {
            let p = *p as usize;
            if p < n && p != i {
                children[p].push(i);
            }
        }
    }
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(n);
    for root in 0..n {
        let mut set = vec![false; n];
        let mut stack = vec![root];
        set[root] = true;
        while let Some(j) = stack.pop() {
            for &c in &children[j] {
                if !set[c] {
                    set[c] = true;
                    stack.push(c);
                }
            }
        }
        out.push((0..n).filter(|&i| set[i]).collect());
    }
    out
}

/// The ancestor chain of `joint` including itself, root first (bounded, cycle-safe).
fn ancestors_of(skeleton: &Skeleton, joint: usize) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut current = Some(joint);
    while let Some(j) = current {
        if j >= skeleton.len() || chain.contains(&j) {
            break;
        }
        chain.push(j);
        current = skeleton.parents.get(j).copied().flatten().map(|p| p as usize);
    }
    chain.reverse();
    chain
}

/// Which `Vec3` channel of a joint a candidate replaces (rotations have their own path).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Translation,
    Scale,
}

/// Cook-time error context: working poses per evaluation time, their object-space
/// matrices, probe sets and the per-bone step budget (`tolerance / animated path steps`).
struct ErrorCtx {
    eval_times: Vec<f32>,
    work_poses: Vec<Pose>,
    base_globals: Vec<Vec<Mat4>>,
    descendants: Vec<Vec<usize>>,
    probes: Vec<Vec<Vec3>>,
    budgets: Vec<f32>,
    bind: Pose,
}

impl ErrorCtx {
    /// Recompute object-space matrices from the working poses.
    fn refresh_globals(&mut self, skeleton: &Skeleton) {
        self.base_globals = self
            .work_poses
            .iter()
            .map(|p| skeleton.globals(p, None))
            .collect();
    }

    /// Largest budget-normalised probe displacement caused by replacing `joint`'s local
    /// transform with `local_at(time_index, time)`, measured at the probes of `joint` and
    /// every descendant, relative to the current working pose. A score of 1.0 spends the
    /// whole per-step budget; `range` restricts evaluation times, `limit` prunes early.
    fn score(
        &self,
        skeleton: &Skeleton,
        joint: usize,
        range: Option<(f32, f32)>,
        limit: f32,
        mut local_at: impl FnMut(usize, f32) -> (Vec3, Quat, Vec3),
    ) -> f32 {
        let Some(descendants) = self.descendants.get(joint) else {
            return 0.0;
        };
        let parent = skeleton
            .parents
            .get(joint)
            .copied()
            .flatten()
            .map(|p| p as usize)
            .filter(|&p| p < joint);
        let mut worst = 0.0f32;
        for (k, &time) in self.eval_times.iter().enumerate() {
            if let Some((lo, hi)) = range
                && (time < lo || time > hi)
            {
                continue;
            }
            let Some(base_pose) = self.work_poses.get(k) else {
                continue;
            };
            let Some(base_globals) = self.base_globals.get(k) else {
                continue;
            };
            if joint >= base_pose.translations.len()
                || joint >= base_pose.rotations.len()
                || joint >= base_pose.scales.len()
                || joint >= base_globals.len()
            {
                continue;
            }
            let (t, r, s) = local_at(k, time);
            if t == base_pose.translations[joint]
                && r == base_pose.rotations[joint]
                && s == base_pose.scales[joint]
            {
                continue;
            }
            let parent_global = match parent {
                Some(p) if p < base_globals.len() => base_globals[p],
                _ => Mat4::IDENTITY,
            };
            let candidate_global = parent_global * Mat4::from_scale_rotation_translation(s, r, t);
            // Maps every base model-space point of the subtree to its candidate position.
            let delta = candidate_global * base_globals[joint].inverse();
            for &d in descendants {
                let Some(&budget) = self.budgets.get(d) else {
                    continue;
                };
                let Some(global_d) = base_globals.get(d) else {
                    continue;
                };
                let Some(probes) = self.probes.get(d) else {
                    continue;
                };
                if budget.is_nan() || budget <= 0.0 {
                    continue;
                }
                for &probe in probes {
                    let base_point = global_d.transform_point3(probe);
                    let disp = (delta.transform_point3(base_point) - base_point).length();
                    if !disp.is_finite() {
                        return f32::INFINITY;
                    }
                    let normalised = disp / budget;
                    if normalised > worst {
                        worst = normalised;
                        if worst > limit {
                            return worst;
                        }
                    }
                }
            }
        }
        worst
    }

    /// The bind value of `joint`'s `channel`: what a dropped `Vec3` track yields.
    fn bind_vec(&self, joint: usize, channel: Channel) -> Vec3 {
        match channel {
            Channel::Scale => self.bind.scales.get(joint).copied().unwrap_or(Vec3::ONE),
            Channel::Translation => self.bind.translations.get(joint).copied().unwrap_or(Vec3::ZERO),
        }
    }

    /// Score a candidate `Vec3` channel (a dropped track yields the bind value).
    fn score_vec(
        &self,
        skeleton: &Skeleton,
        joint: usize,
        channel: Channel,
        track: &Vec3Track,
        range: Option<(f32, f32)>,
        limit: f32,
    ) -> f32 {
        let bind_value = self.bind_vec(joint, channel);
        self.score(skeleton, joint, range, limit, |k, time| {
            let base = &self.work_poses[k];
            let value = track.sample(time).unwrap_or(bind_value);
            match channel {
                Channel::Translation => (value, base.rotations[joint], base.scales[joint]),
                Channel::Scale => (base.translations[joint], base.rotations[joint], value),
            }
        })
    }

    /// Score a candidate rotation channel.
    fn score_rot(
        &self,
        skeleton: &Skeleton,
        joint: usize,
        track: &RotationTrack,
        bind_value: Quat,
        range: Option<(f32, f32)>,
        limit: f32,
    ) -> f32 {
        self.score(skeleton, joint, range, limit, |k, time| {
            let base = &self.work_poses[k];
            let value = track.sample(time).unwrap_or(bind_value);
            (base.translations[joint], value, base.scales[joint])
        })
    }

    /// Commit a chosen channel: working poses take its sampled values (bind when dropped).
    fn commit_vec(&mut self, skeleton: &Skeleton, joint: usize, channel: Channel, track: &Vec3Track) {
        let bind_value = self.bind_vec(joint, channel);
        for (k, &time) in self.eval_times.iter().enumerate() {
            let value = track.sample(time).unwrap_or(bind_value);
            if let Some(pose) = self.work_poses.get_mut(k) {
                match channel {
                    Channel::Translation => {
                        if let Some(slot) = pose.translations.get_mut(joint) {
                            *slot = value;
                        }
                    }
                    Channel::Scale => {
                        if let Some(slot) = pose.scales.get_mut(joint) {
                            *slot = value;
                        }
                    }
                }
            }
        }
        self.refresh_globals(skeleton);
    }

    /// Commit a chosen rotation channel.
    fn commit_rot(&mut self, skeleton: &Skeleton, joint: usize, track: &RotationTrack) {
        let bind_value = self.bind.rotations.get(joint).copied().unwrap_or(Quat::IDENTITY);
        for (k, &time) in self.eval_times.iter().enumerate() {
            let value = track.sample(time).unwrap_or(bind_value);
            if let Some(pose) = self.work_poses.get_mut(k)
                && let Some(slot) = pose.rotations.get_mut(joint)
            {
                *slot = value;
            }
        }
        self.refresh_globals(skeleton);
    }
}

/// The fully-keyed quantised form of a `Vec3` curve (no reduction, dropping or collapse
/// beyond a single key becoming constant).
fn full_vec_track(curve: &TrackCurve<Vec3>) -> Vec3Track {
    match curve {
        TrackCurve::Static(v) => Vec3Track::Constant(*v),
        TrackCurve::Animated { times, values, .. } => {
            if values.is_empty() {
                return Vec3Track::Dropped;
            }
            if values.len() == 1 {
                return Vec3Track::Constant(values[0]);
            }
            let mut min = values[0];
            let mut max = values[0];
            for v in values {
                min = min.min(*v);
                max = max.max(*v);
            }
            Vec3Track::Animated {
                times: times.clone(),
                keys: values.iter().map(|&v| quantize_vec3(v, min, max)).collect(),
                min,
                max,
            }
        }
    }
}

/// The fully-keyed packed form of a rotation curve in `format`.
fn full_rot_track(curve: &TrackCurve<Quat>, format: RotationFormat) -> RotationTrack {
    let constant = |q: Quat| RotationTrack::Constant {
        format,
        key: pack_smallest_three(q, format),
    };
    match curve {
        TrackCurve::Static(q) => constant(*q),
        TrackCurve::Animated { times, values, .. } => {
            if values.is_empty() {
                return RotationTrack::Dropped;
            }
            if values.len() == 1 {
                return constant(values[0]);
            }
            match format {
                RotationFormat::Bits48 => RotationTrack::Animated48 {
                    times: times.clone(),
                    keys: values.iter().map(|&q| pack_rotation_48(q)).collect(),
                },
                RotationFormat::Bits32 => RotationTrack::Animated32 {
                    times: times.clone(),
                    keys: values.iter().map(|&q| pack_rotation_32(q)).collect(),
                },
            }
        }
    }
}

/// First value of a curve, if it has any (the constant-collapse candidate).
fn first_vec_value(curve: &TrackCurve<Vec3>) -> Option<Vec3> {
    match curve {
        TrackCurve::Static(v) => Some(*v),
        TrackCurve::Animated { values, .. } => values.first().copied(),
    }
}

/// First value of a rotation curve, if it has any.
fn first_rot_value(curve: &TrackCurve<Quat>) -> Option<Quat> {
    match curve {
        TrackCurve::Static(q) => Some(*q),
        TrackCurve::Animated { values, .. } => values.first().copied(),
    }
}

/// True for an animated curve with no keys (unsampleable in the source clip: bind value).
fn curve_is_empty<T>(curve: &TrackCurve<T>) -> bool {
    matches!(curve, TrackCurve::Animated { values, .. } if values.is_empty())
}

/// Iterative least-error key removal. `score(times, keys, range, limit)` returns the
/// budget-normalised object-space error of the candidate key set. The full key set must
/// itself score at most 1.0 or nothing is removed; then each round removes the interior
/// key whose removal scores least (judged on the span it bridges, outside which the
/// candidate equals the already-accepted set), while the best score stays within budget.
/// End keys are never removed. Ties keep the earliest index: deterministic.
fn reduce_keys<K: Copy>(
    mut times: Vec<f32>,
    mut keys: Vec<K>,
    mut score: impl FnMut(&[f32], &[K], Option<(f32, f32)>, f32) -> f32,
) -> (Vec<f32>, Vec<K>) {
    if times.len() <= 2 || keys.len() != times.len() {
        return (times, keys);
    }
    if score(&times, &keys, None, f32::INFINITY) > 1.0 {
        return (times, keys);
    }
    while times.len() > 2 {
        let mut best: Option<usize> = None;
        let mut best_score = f32::INFINITY;
        for i in 1..times.len() - 1 {
            let mut cand_times = times.clone();
            let mut cand_keys = keys.clone();
            cand_times.remove(i);
            cand_keys.remove(i);
            let range = Some((times[i - 1], times[i + 1]));
            let s = score(&cand_times, &cand_keys, range, best_score);
            if s < best_score {
                best_score = s;
                best = Some(i);
            }
        }
        match best {
            Some(i) if best_score <= 1.0 => {
                times.remove(i);
                keys.remove(i);
            }
            _ => break,
        }
    }
    (times, keys)
}

/// Compress one `Vec3` channel: drop, then constant, then reduced quantised keys.
fn compress_vec_channel(
    ctx: &ErrorCtx,
    skeleton: &Skeleton,
    joint: usize,
    channel: Channel,
    curve: &TrackCurve<Vec3>,
) -> Vec3Track {
    if curve_is_empty(curve) {
        return Vec3Track::Dropped;
    }
    if ctx.score_vec(skeleton, joint, channel, &Vec3Track::Dropped, None, 1.0) <= 1.0 {
        return Vec3Track::Dropped;
    }
    if let Some(first) = first_vec_value(curve) {
        let constant = Vec3Track::Constant(first);
        if ctx.score_vec(skeleton, joint, channel, &constant, None, 1.0) <= 1.0 {
            return constant;
        }
    }
    match full_vec_track(curve) {
        Vec3Track::Animated {
            times,
            keys,
            min,
            max,
        } => {
            let (times, keys) = reduce_keys(times, keys, |t, k, range, limit| {
                let candidate = Vec3Track::Animated {
                    times: t.to_vec(),
                    keys: k.to_vec(),
                    min,
                    max,
                };
                ctx.score_vec(skeleton, joint, channel, &candidate, range, limit)
            });
            Vec3Track::Animated {
                times,
                keys,
                min,
                max,
            }
        }
        other => other,
    }
}

/// Compress one rotation channel: drop, format choice, constant, reduced packed keys.
fn compress_rot_channel(
    ctx: &ErrorCtx,
    skeleton: &Skeleton,
    joint: usize,
    curve: &TrackCurve<Quat>,
    bind_value: Quat,
    preference: RotationFormatPreference,
) -> RotationTrack {
    if curve_is_empty(curve) {
        return RotationTrack::Dropped;
    }
    if ctx.score_rot(skeleton, joint, &RotationTrack::Dropped, bind_value, None, 1.0) <= 1.0 {
        return RotationTrack::Dropped;
    }
    let format = match preference {
        RotationFormatPreference::Force48 => RotationFormat::Bits48,
        RotationFormatPreference::Force32 => RotationFormat::Bits32,
        RotationFormatPreference::Auto => {
            let trial = full_rot_track(curve, RotationFormat::Bits32);
            if ctx.score_rot(skeleton, joint, &trial, bind_value, None, 0.25) <= 0.25 {
                RotationFormat::Bits32
            } else {
                RotationFormat::Bits48
            }
        }
    };
    if let Some(first) = first_rot_value(curve) {
        let constant = RotationTrack::Constant {
            format,
            key: pack_smallest_three(first, format),
        };
        if matches!(curve, TrackCurve::Static(_))
            || ctx.score_rot(skeleton, joint, &constant, bind_value, None, 1.0) <= 1.0
        {
            return constant;
        }
    }
    match full_rot_track(curve, format) {
        RotationTrack::Animated48 { times, keys } => {
            let (times, keys) = reduce_keys(times, keys, |t, k, range, limit| {
                let candidate = RotationTrack::Animated48 {
                    times: t.to_vec(),
                    keys: k.to_vec(),
                };
                ctx.score_rot(skeleton, joint, &candidate, bind_value, range, limit)
            });
            RotationTrack::Animated48 { times, keys }
        }
        RotationTrack::Animated32 { times, keys } => {
            let (times, keys) = reduce_keys(times, keys, |t, k, range, limit| {
                let candidate = RotationTrack::Animated32 {
                    times: t.to_vec(),
                    keys: k.to_vec(),
                };
                ctx.score_rot(skeleton, joint, &candidate, bind_value, range, limit)
            });
            RotationTrack::Animated32 { times, keys }
        }
        other => other,
    }
}

/// Raw `f32` size of one curve under the report's size model.
fn curve_bytes<T>(curve: &TrackCurve<T>, value_bytes: usize) -> usize {
    match curve {
        TrackCurve::Static(_) => value_bytes,
        TrackCurve::Animated { times, values, .. } => times.len() * 4 + values.len() * value_bytes,
    }
}

/// Original size and key count of the clip's skeletal tracks under the size model.
fn original_size(clip: &Clip) -> (usize, usize) {
    let mut bytes = 16 + clip.name.len();
    let mut keys = 0usize;
    for track in &clip.tracks {
        bytes += curve_bytes(&track.translations, 12);
        bytes += curve_bytes(&track.rotations, 16);
        bytes += curve_bytes(&track.scales, 12);
        keys += track.translations.key_count() + track.rotations.key_count() + track.scales.key_count();
    }
    (bytes, keys)
}

/// Evaluation times: 0, the duration, every skeletal key time and midpoints between
/// consecutive distinct times, sorted and deduplicated (deterministic).
fn evaluation_times(clip: &Clip) -> Vec<f32> {
    let mut times: Vec<f32> = vec![0.0, clip.duration];
    for track in &clip.tracks {
        for curve_times in [
            match &track.translations {
                TrackCurve::Animated { times, .. } => Some(times),
                TrackCurve::Static(_) => None,
            },
            match &track.rotations {
                TrackCurve::Animated { times, .. } => Some(times),
                TrackCurve::Static(_) => None,
            },
            match &track.scales {
                TrackCurve::Animated { times, .. } => Some(times),
                TrackCurve::Static(_) => None,
            },
        ]
        .into_iter()
        .flatten()
        {
            times.extend_from_slice(curve_times);
        }
    }
    times.sort_by(|a, b| a.total_cmp(b));
    times.dedup();
    let mut with_midpoints: Vec<f32> = Vec::with_capacity(times.len() * 2);
    for pair in times.windows(2) {
        with_midpoints.push(pair[0]);
        with_midpoints.push((pair[0] + pair[1]) * 0.5);
    }
    if let Some(&last) = times.last() {
        with_midpoints.push(last);
    }
    if with_midpoints.is_empty() {
        with_midpoints.push(0.0);
    }
    with_midpoints
}

/// Verification times: the evaluation times plus a uniform 17-point grid over the clip.
fn verification_times(clip: &Clip, eval_times: &[f32]) -> Vec<f32> {
    let mut times: Vec<f32> = eval_times.to_vec();
    if clip.duration > 0.0 {
        for i in 0..=16u32 {
            times.push(clip.duration * (i as f32 / 16.0));
        }
    }
    times.sort_by(|a, b| a.total_cmp(b));
    times.dedup();
    times
}

/// Measured maximum object-space probe displacement per bone between the source clip and
/// `compressed`, sampled at `times` from the bind pose on both sides. Also used as the
/// [`CompressionReport`] error and the repair pass's criterion.
fn measure_max_errors(
    clip: &Clip,
    skeleton: &Skeleton,
    compressed: &CompressedClip,
    times: &[f32],
    probes: &[Vec<Vec3>],
) -> Vec<f32> {
    let bind = skeleton.bind_pose_pose();
    let mut errors = vec![0.0f32; skeleton.len()];
    for &time in times {
        let mut original_pose = bind.clone();
        clip.sample_pose(time, &mut original_pose);
        let mut compressed_pose = bind.clone();
        compressed.sample(time, &mut compressed_pose);
        let original_globals = skeleton.globals(&original_pose, None);
        let compressed_globals = skeleton.globals(&compressed_pose, None);
        for (joint, slot) in errors.iter_mut().enumerate() {
            let (Some(original_global), Some(compressed_global), Some(joint_probes)) = (
                original_globals.get(joint),
                compressed_globals.get(joint),
                probes.get(joint),
            ) else {
                continue;
            };
            for &probe in joint_probes {
                let disp = (compressed_global.transform_point3(probe)
                    - original_global.transform_point3(probe))
                .length();
                let disp = if disp.is_finite() { disp } else { f32::INFINITY };
                if disp > *slot {
                    *slot = disp;
                }
            }
        }
    }
    errors
}

/// Compress `clip`'s skeletal tracks for `skeleton` under `options`.
///
/// Bones are reduced root-first (the skeleton is topologically sorted, so ascending joint
/// id). For each channel the candidates are tried in order — dropped (bind value),
/// constant, then the fully-keyed quantised track reduced by iterative least-error key
/// removal — and each candidate is charged, in object space at the probes of the bone and
/// all its descendants, against an equal share of every descendant's tolerance. A final
/// verification pass measures the real whole-clip error; if a bone exceeds its tolerance
/// the ancestors of the worst bone are restored to their full key sets (rotations at
/// 48 bits unless 32 was forced) and the clip is re-measured, up to eight rounds. When
/// quantisation alone exceeds an extremely small tolerance the report says so honestly
/// (`max_error` above tolerance) instead of hiding it.
///
/// Morph tracks, notifies, named curves, property tracks and root-motion metadata are not
/// carried by this format.
///
/// # Errors
///
/// Returns [`CompressError`] for an invalid skeleton or options, a non-finite duration,
/// key time or key value (including NaN), a degenerate quaternion key, malformed key
/// arrays, duplicate tracks, or a track targeting a joint the skeleton does not have.
pub fn compress(
    clip: &Clip,
    skeleton: &Skeleton,
    options: &CompressOptions,
) -> Result<CompressedClip, CompressError> {
    validate_inputs(clip, skeleton, options)?;
    let n = skeleton.len();
    let bind = skeleton.bind_pose_pose();

    let mut track_map: Vec<Option<&crate::clip::JointTrack>> = vec![None; n];
    for track in &clip.tracks {
        track_map[track.joint as usize] = Some(track);
    }

    let eval_times = evaluation_times(clip);
    let work_poses: Vec<Pose> = eval_times
        .iter()
        .map(|&time| {
            let mut pose = bind.clone();
            clip.sample_pose(time, &mut pose);
            pose
        })
        .collect();

    let descendants = descendants_of(skeleton);
    let probes: Vec<Vec<Vec3>> = (0..n).map(|j| probe_points(options.shell_for(j))).collect();
    let budgets: Vec<f32> = (0..n)
        .map(|d| {
            let steps = ancestors_of(skeleton, d)
                .iter()
                .filter(|&&a| track_map.get(a).copied().flatten().is_some())
                .count()
                * 3;
            let tolerance = options.tolerance_for(d);
            if steps > 0 {
                tolerance / steps as f32
            } else {
                tolerance
            }
        })
        .collect();

    // Full-key backups for the repair pass (48-bit rotations unless 32 was forced).
    let backup_format = match options.rotation_format {
        RotationFormatPreference::Force32 => RotationFormat::Bits32,
        _ => RotationFormat::Bits48,
    };
    let backups: Vec<Option<CompressedJointTrack>> = track_map
        .iter()
        .map(|track| {
            let track = (*track)?;
            Some(CompressedJointTrack {
                joint: track.joint,
                translations: full_vec_track(&track.translations),
                rotations: full_rot_track(&track.rotations, backup_format),
                scales: full_vec_track(&track.scales),
            })
        })
        .collect();

    let mut ctx = ErrorCtx {
        eval_times: eval_times.clone(),
        work_poses,
        base_globals: Vec::new(),
        descendants,
        probes: probes.clone(),
        budgets,
        bind: bind.clone(),
    };
    ctx.refresh_globals(skeleton);

    let mut tracks: Vec<CompressedJointTrack> = Vec::new();
    for (joint, track) in track_map.iter().enumerate() {
        let Some(track) = *track else {
            continue;
        };
        let bind_r = bind.rotations.get(joint).copied().unwrap_or(Quat::IDENTITY);
        let translations =
            compress_vec_channel(&ctx, skeleton, joint, Channel::Translation, &track.translations);
        ctx.commit_vec(skeleton, joint, Channel::Translation, &translations);
        let rotations = compress_rot_channel(
            &ctx,
            skeleton,
            joint,
            &track.rotations,
            bind_r,
            options.rotation_format,
        );
        ctx.commit_rot(skeleton, joint, &rotations);
        let scales = compress_vec_channel(&ctx, skeleton, joint, Channel::Scale, &track.scales);
        ctx.commit_vec(skeleton, joint, Channel::Scale, &scales);
        tracks.push(CompressedJointTrack {
            joint: joint as u32,
            translations,
            rotations,
            scales,
        });
    }

    let (original_bytes, original_keys) = original_size(clip);
    let mut compressed = CompressedClip {
        name: clip.name.clone(),
        duration: clip.duration,
        tracks,
        report: CompressionReport {
            original_bytes,
            compressed_bytes: 0,
            original_keys,
            compressed_keys: 0,
            max_error: 0.0,
            max_error_per_bone: vec![0.0; n],
        },
    };

    // Verification and repair: restore the ancestors of the worst bone to full keys.
    let verify_times = verification_times(clip, &eval_times);
    for _round in 0..8 {
        let errors = measure_max_errors(clip, skeleton, &compressed, &verify_times, &probes);
        let mut worst_joint: Option<usize> = None;
        let mut worst_ratio = 1.0f32;
        for (joint, &error) in errors.iter().enumerate() {
            let ratio = error / options.tolerance_for(joint);
            if ratio > worst_ratio {
                worst_ratio = ratio;
                worst_joint = Some(joint);
            }
        }
        let Some(worst) = worst_joint else { break };
        let mut changed = false;
        for ancestor in ancestors_of(skeleton, worst) {
            let Some(backup) = backups.get(ancestor).cloned().flatten() else {
                continue;
            };
            if let Some(slot) = compressed
                .tracks
                .iter_mut()
                .find(|t| t.joint as usize == ancestor)
                && *slot != backup
            {
                *slot = backup;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let errors = measure_max_errors(clip, skeleton, &compressed, &verify_times, &probes);
    let max_error = errors.iter().copied().fold(0.0f32, f32::max);
    compressed.report = CompressionReport {
        original_bytes,
        compressed_bytes: compressed.compressed_bytes(),
        original_keys,
        compressed_keys: compressed.key_count(),
        max_error,
        max_error_per_bone: errors,
    };
    Ok(compressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::JointTrack;
    use crate::pose::translated;

    /// A straight chain of `n` joints along +X, each `bone_len` from its parent.
    fn chain(n: usize, bone_len: f32) -> Skeleton {
        let names: Vec<String> = (0..n).map(|i| format!("j{i}")).collect();
        let parents: Vec<Option<u32>> = (0..n)
            .map(|i| if i == 0 { None } else { Some((i - 1) as u32) })
            .collect();
        let binds: Vec<crate::skeleton::LocalBind> = (0..n)
            .map(|i| {
                translated(if i == 0 {
                    Vec3::ZERO
                } else {
                    Vec3::new(bone_len, 0.0, 0.0)
                })
            })
            .collect();
        Skeleton::from_local_bind(names, parents, binds).expect("valid chain skeleton")
    }

    /// Bind translation of joint `j` in a [`chain`] skeleton.
    fn bind_translation(j: usize, bone_len: f32) -> Vec3 {
        if j == 0 {
            Vec3::ZERO
        } else {
            Vec3::new(bone_len, 0.0, 0.0)
        }
    }

    fn animated_vec(times: Vec<f32>, values: Vec<Vec3>) -> TrackCurve<Vec3> {
        TrackCurve::Animated {
            times,
            values,
            exact_rotation: false,
        }
    }

    fn animated_rot(times: Vec<f32>, values: Vec<Quat>) -> TrackCurve<Quat> {
        TrackCurve::Animated {
            times,
            values,
            exact_rotation: false,
        }
    }

    fn track(
        joint: u32,
        translations: TrackCurve<Vec3>,
        rotations: TrackCurve<Quat>,
        scales: TrackCurve<Vec3>,
    ) -> JointTrack {
        JointTrack {
            joint,
            translations,
            rotations,
            scales,
        }
    }

    /// A track that reproduces the bind pose of joint `j` in a chain skeleton.
    fn bind_track(j: usize, bone_len: f32) -> JointTrack {
        track(
            j as u32,
            TrackCurve::Static(bind_translation(j, bone_len)),
            TrackCurve::Static(Quat::IDENTITY),
            TrackCurve::Static(Vec3::ONE),
        )
    }

    fn clip_with(name: &str, duration: f32, tracks: Vec<JointTrack>) -> AnimationClip {
        let mut clip = AnimationClip::empty(name, duration);
        clip.tracks = tracks;
        clip
    }

    /// One animated joint, tolerance 0.03. The per-channel budget is `0.03 / 3`. An interior
    /// translation whose probe error is 0.02 scores 2 and the key stays.
    #[test]
    fn one_joint_keeps_an_interior_key_above_the_per_channel_share() {
        let skeleton = chain(1, 1.0);
        let clip = clip_with(
            "bob",
            1.0,
            vec![track(
                0,
                animated_vec(
                    vec![0.0, 0.5, 1.0],
                    vec![Vec3::ZERO, Vec3::new(0.0, 0.02, 0.0), Vec3::ZERO],
                ),
                TrackCurve::Static(Quat::IDENTITY),
                TrackCurve::Static(Vec3::ONE),
            )],
        );
        let options = CompressOptions::new(0.03);
        let compressed = compress(&clip, &skeleton, &options).expect("compress");
        let track = compressed.tracks.iter().find(|t| t.joint == 0).expect("joint 0");
        assert_eq!(
            track.translations.key_count(),
            3,
            "0.02 is inside a 0.03 bone share and outside the per-channel budget of 0.01"
        );
    }

    /// Independent oracle: max probe displacement per bone between source and compressed,
    /// sampled on a uniform grid, sharing no code with the compressor's measurement
    /// beyond the public sampling and forward-kinematics APIs.
    fn independent_max_errors(
        clip: &AnimationClip,
        skeleton: &Skeleton,
        compressed: &CompressedClip,
        options: &CompressOptions,
        samples: u32,
    ) -> Vec<f32> {
        let bind = skeleton.bind_pose_pose();
        let mut errors = vec![0.0f32; skeleton.len()];
        for step in 0..=samples {
            let time = if samples == 0 {
                0.0
            } else {
                clip.duration * (step as f32 / samples as f32)
            };
            let mut original = bind.clone();
            clip.sample_pose(time, &mut original);
            let mut decompressed = bind.clone();
            compressed.sample(time, &mut decompressed);
            let original_globals = skeleton.globals(&original, None);
            let compressed_globals = skeleton.globals(&decompressed, None);
            for joint in 0..skeleton.len() {
                let shell = options.shell_for(joint);
                let probes = [
                    Vec3::ZERO,
                    Vec3::new(shell, 0.0, 0.0),
                    Vec3::new(-shell, 0.0, 0.0),
                    Vec3::new(0.0, shell, 0.0),
                    Vec3::new(0.0, -shell, 0.0),
                    Vec3::new(0.0, 0.0, shell),
                    Vec3::new(0.0, 0.0, -shell),
                ];
                for probe in probes {
                    let disp = (compressed_globals[joint].transform_point3(probe)
                        - original_globals[joint].transform_point3(probe))
                    .length();
                    if disp > errors[joint] {
                        errors[joint] = disp;
                    }
                }
            }
        }
        errors
    }

    /// SplitMix64-style deterministic generator (explicit seed, no external crates).
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn unit(&mut self) -> f32 {
            (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
        }

        fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.unit()
        }
    }

    /// A seeded smooth random clip: every joint rotates sinusoidally about a random axis.
    fn random_clip(seed: u64, joints: usize, bone_len: f32) -> AnimationClip {
        let mut rng = Rng(seed ^ 0xA5A5_1234_5678_9ABC);
        let times: Vec<f32> = (0..=6).map(|i| i as f32 / 6.0).collect();
        let mut tracks = Vec::new();
        for j in 0..joints {
            let axis_raw = Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
            let axis = if axis_raw.length_squared() > 1e-6 {
                axis_raw.normalize()
            } else {
                Vec3::X
            };
            let amplitude = rng.range(0.05, 0.2);
            let phase = rng.range(0.0, std::f32::consts::TAU);
            let rotations: Vec<Quat> = times
                .iter()
                .map(|&t| Quat::from_axis_angle(axis, amplitude * (std::f32::consts::TAU * t + phase).sin()))
                .collect();
            tracks.push(track(
                j as u32,
                TrackCurve::Static(bind_translation(j, bone_len)),
                animated_rot(times.clone(), rotations),
                TrackCurve::Static(Vec3::ONE),
            ));
        }
        clip_with("random", 1.0, tracks)
    }

    #[test]
    fn constant_clip_collapses_to_constant_tracks() {
        let skeleton = chain(3, 0.5);
        let pose_t = Vec3::new(1.0, 2.0, 3.0);
        let pose_r = Quat::from_rotation_y(0.4);
        let tracks: Vec<JointTrack> = (0..3)
            .map(|j| {
                let times: Vec<f32> = (0..=4).map(|i| i as f32 * 0.25).collect();
                track(
                    j as u32,
                    animated_vec(times.clone(), vec![pose_t; 5]),
                    animated_rot(times, vec![pose_r; 5]),
                    TrackCurve::Static(Vec3::splat(2.0)),
                )
            })
            .collect();
        let clip = clip_with("constant", 1.0, tracks);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        for j in 0..3u32 {
            let t = compressed.track_for(j).expect("track present");
            assert!(
                matches!(t.translations, Vec3Track::Constant(_)),
                "joint {j} translations"
            );
            assert!(
                matches!(t.rotations, RotationTrack::Constant { .. }),
                "joint {j} rotations"
            );
            assert!(matches!(t.scales, Vec3Track::Constant(_)), "joint {j} scales");
            assert_eq!(t.key_count(), 3);
        }
        let mut pose = skeleton.bind_pose_pose();
        compressed.sample(0.37, &mut pose);
        assert!(pose.translations[0].abs_diff_eq(pose_t, 1e-6));
        assert!(pose.rotations[0].angle_between(pose_r) < 1e-3);
        assert!(pose.scales[0].abs_diff_eq(Vec3::splat(2.0), 1e-6));
    }

    #[test]
    fn default_bind_tracks_are_dropped() {
        let skeleton = chain(4, 0.25);
        let tracks: Vec<JointTrack> = (0..4).map(|j| bind_track(j, 0.25)).collect();
        let clip = clip_with("bind", 1.0, tracks);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        assert_eq!(compressed.key_count(), 0, "every channel dropped");
        for t in &compressed.tracks {
            assert!(t.translations.is_dropped() && t.rotations.is_dropped() && t.scales.is_dropped());
        }
        let mut pose = skeleton.bind_pose_pose();
        let before = pose.clone();
        compressed.sample(0.5, &mut pose);
        assert_eq!(
            pose, before,
            "sampling a fully dropped clip leaves the pose untouched"
        );
        assert_eq!(compressed.report().max_error, 0.0);
    }

    #[test]
    fn linear_translation_reduces_to_two_keys() {
        let skeleton = chain(2, 0.5);
        let times: Vec<f32> = (0..=10).map(|i| i as f32 * 0.1).collect();
        let values: Vec<Vec3> = times
            .iter()
            .map(|&t| Vec3::new(2.0 * t, 1.0 - t, 0.5 * t))
            .collect();
        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(times, values);
        let clip = clip_with("linear", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::new(0.01)).expect("compress");
        let t = compressed.track_for(0).expect("root track");
        assert_eq!(
            t.translations.key_count(),
            2,
            "linear motion keeps only its end keys"
        );
        let mut pose = skeleton.bind_pose_pose();
        compressed.sample(0.5, &mut pose);
        assert!(pose.translations[0].abs_diff_eq(Vec3::new(1.0, 0.5, 0.25), 1e-3));
    }

    #[test]
    fn linear_scale_reduces_to_two_keys() {
        let skeleton = chain(1, 0.5);
        let times: Vec<f32> = (0..=8).map(|i| i as f32 * 0.125).collect();
        let values: Vec<Vec3> = times.iter().map(|&t| Vec3::splat(1.0 + t)).collect();
        let mut root = bind_track(0, 0.5);
        root.scales = animated_vec(times, values);
        let clip = clip_with("scale", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::new(0.01)).expect("compress");
        assert_eq!(compressed.track_for(0).expect("track").scales.key_count(), 2);
        let mut pose = skeleton.bind_pose_pose();
        compressed.sample(0.5, &mut pose);
        assert!(pose.scales[0].abs_diff_eq(Vec3::splat(1.5), 1e-3));
    }

    #[test]
    fn round_trip_error_below_tolerance_on_100_seeded_random_clips_on_30_bone_chain() {
        let skeleton = chain(30, 0.05);
        let options = CompressOptions::new(0.05);
        for seed in 0..100u64 {
            let clip = random_clip(seed, 30, 0.05);
            let compressed = compress(&clip, &skeleton, &options).expect("compress random clip");
            assert!(
                compressed.report().max_error <= options.tolerance,
                "seed {seed}: report max_error {} exceeds {}",
                compressed.report().max_error,
                options.tolerance
            );
            assert_eq!(compressed.report().max_error_per_bone.len(), 30);
            let oracle = independent_max_errors(&clip, &skeleton, &compressed, &options, 20);
            let oracle_max = oracle.iter().copied().fold(0.0f32, f32::max);
            assert!(
                oracle_max <= options.tolerance,
                "seed {seed}: oracle max error {oracle_max} exceeds {}",
                options.tolerance
            );
            assert!(compressed.report().compressed_keys <= compressed.report().original_keys);
        }
    }

    #[test]
    fn parent_error_is_checked_at_a_distant_child_chain_amplification() {
        // 21 joints x 0.2 = a 4-unit lever. A 0.05 rad root bump moves root shell probes
        // (shell 0.01) by 0.0005 -- invisible locally -- but the tip origin by ~0.2.
        let skeleton = chain(21, 0.2);
        let bump = Quat::from_rotation_z(0.05);
        let times = vec![0.0, 0.5, 1.0];
        let values = vec![Quat::IDENTITY, bump, Quat::IDENTITY];
        let mut options = CompressOptions::new(0.05);
        options.shell_radius = 0.01;

        let mut root = bind_track(0, 0.2);
        root.rotations = animated_rot(times.clone(), values.clone());
        let clip = clip_with("root-bump", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &options).expect("compress");
        assert_eq!(
            compressed.track_for(0).expect("root").rotations.key_count(),
            3,
            "the middle root key must survive: removing it swings the tip by ~0.2"
        );
        let oracle = independent_max_errors(&clip, &skeleton, &compressed, &options, 20);
        assert!(
            oracle[20] <= options.tolerance,
            "tip error {} within tolerance",
            oracle[20]
        );

        // The same bump on the tip joint itself only moves its own 0.01 shell: collapses.
        let mut tip = bind_track(20, 0.2);
        tip.rotations = animated_rot(times, values);
        let clip = clip_with("tip-bump", 1.0, vec![tip]);
        let compressed = compress(&clip, &skeleton, &options).expect("compress");
        assert!(
            compressed.track_for(20).expect("tip").rotations.key_count() < 3,
            "a purely local bump inside its tiny shell may be approximated away"
        );
    }

    #[test]
    fn compressed_size_strictly_smaller_for_smooth_clip_with_ratio() {
        let skeleton = chain(5, 0.2);
        let times: Vec<f32> = (0..=60).map(|i| i as f32 / 30.0).collect();
        let tracks: Vec<JointTrack> = (0..5)
            .map(|j| {
                let mut t = bind_track(j, 0.2);
                t.rotations = animated_rot(
                    times.clone(),
                    times
                        .iter()
                        .map(|&time| {
                            Quat::from_rotation_y(0.3 * (std::f32::consts::PI * time + j as f32).sin())
                        })
                        .collect(),
                );
                t
            })
            .collect();
        let clip = clip_with("smooth", 2.0, tracks);
        let options = CompressOptions::new(0.05);
        let compressed = compress(&clip, &skeleton, &options).expect("compress");
        let report = compressed.report();
        assert!(
            report.compressed_bytes < report.original_bytes,
            "strictly smaller: {report:?}"
        );
        assert!(
            report.ratio() < 0.6,
            "ratio {} should show real reduction",
            report.ratio()
        );
        assert!(report.compressed_keys < report.original_keys);
        assert!(report.max_error <= options.tolerance);
    }

    #[test]
    fn compression_is_deterministic() {
        let skeleton = chain(6, 0.1);
        let clip = random_clip(42, 6, 0.1);
        let options = CompressOptions::new(0.02);
        let a = compress(&clip, &skeleton, &options).expect("first");
        let b = compress(&clip, &skeleton, &options).expect("second");
        assert_eq!(
            a, b,
            "identical input compresses to identical output, report included"
        );
        let mut pa = skeleton.bind_pose_pose();
        let mut pb = skeleton.bind_pose_pose();
        a.sample(0.31, &mut pa);
        b.sample(0.31, &mut pb);
        assert_eq!(pa, pb);
        for (x, y) in pa.translations.iter().zip(&pb.translations) {
            assert_eq!(x.to_array().map(f32::to_bits), y.to_array().map(f32::to_bits));
        }
    }

    #[test]
    fn zero_length_clip_compresses_and_samples() {
        let skeleton = chain(3, 0.5);
        let clip = AnimationClip::empty("empty", 0.0);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        assert_eq!(compressed.duration, 0.0);
        assert!(compressed.tracks.is_empty());
        assert_eq!(compressed.report().max_error, 0.0);
        let mut pose = skeleton.bind_pose_pose();
        let before = pose.clone();
        compressed.sample(0.0, &mut pose);
        compressed.sample(10.0, &mut pose);
        assert_eq!(pose, before);
    }

    #[test]
    fn single_frame_clip_becomes_constant() {
        let skeleton = chain(2, 0.5);
        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(vec![0.0], vec![Vec3::new(1.0, 1.0, 1.0)]);
        root.rotations = animated_rot(vec![0.0], vec![Quat::from_rotation_x(0.3)]);
        let clip = clip_with("one-frame", 0.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        let t = compressed.track_for(0).expect("track");
        assert!(matches!(t.translations, Vec3Track::Constant(_)));
        assert!(matches!(t.rotations, RotationTrack::Constant { .. }));
        let mut pose = skeleton.bind_pose_pose();
        compressed.sample(0.0, &mut pose);
        assert!(pose.translations[0].abs_diff_eq(Vec3::new(1.0, 1.0, 1.0), 1e-6));
        compressed.sample(5.0, &mut pose);
        assert!(pose.translations[0].abs_diff_eq(Vec3::new(1.0, 1.0, 1.0), 1e-6));
    }

    #[test]
    fn nan_translation_value_returns_error() {
        let skeleton = chain(1, 0.5);
        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(vec![0.0, 1.0], vec![Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 0.0)]);
        let clip = clip_with("nan", 1.0, vec![root]);
        let result = compress(&clip, &skeleton, &CompressOptions::default());
        assert!(matches!(
            result,
            Err(CompressError::NonFiniteValue { joint: 0, key: 1, .. })
        ));
    }

    #[test]
    fn nan_rotation_and_nan_time_and_infinite_scale_return_errors() {
        let skeleton = chain(1, 0.5);
        let mut root = bind_track(0, 0.5);
        root.rotations = TrackCurve::Static(Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0));
        let clip = clip_with("nan-rot", 1.0, vec![root]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::NonFiniteValue { .. })
        ));

        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(vec![0.0, f32::NAN], vec![Vec3::ZERO, Vec3::ONE]);
        let clip = clip_with("nan-time", 1.0, vec![root]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::NonFiniteTime { key: 1, .. })
        ));

        let mut root = bind_track(0, 0.5);
        root.scales = TrackCurve::Static(Vec3::new(1.0, f32::INFINITY, 1.0));
        let clip = clip_with("inf-scale", 1.0, vec![root]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::NonFiniteValue { .. })
        ));
    }

    #[test]
    fn zero_quaternion_key_returns_error() {
        let skeleton = chain(1, 0.5);
        let mut root = bind_track(0, 0.5);
        root.rotations = TrackCurve::Static(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0));
        let clip = clip_with("zero-quat", 1.0, vec![root]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::DegenerateQuaternion { joint: 0, key: 0 })
        ));
    }

    #[test]
    fn invalid_tolerances_and_shells_return_errors() {
        let skeleton = chain(1, 0.5);
        let clip = clip_with("ok", 1.0, vec![bind_track(0, 0.5)]);
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(matches!(
                compress(&clip, &skeleton, &CompressOptions::new(bad)),
                Err(CompressError::InvalidTolerance { .. })
            ));
        }
        let options = CompressOptions {
            per_bone_tolerance: vec![f32::NAN],
            ..CompressOptions::default()
        };
        assert!(matches!(
            compress(&clip, &skeleton, &options),
            Err(CompressError::InvalidTolerance { .. })
        ));
        let options = CompressOptions {
            shell_radius: -0.5,
            ..CompressOptions::default()
        };
        assert!(matches!(
            compress(&clip, &skeleton, &options),
            Err(CompressError::InvalidShell { .. })
        ));
        let mut bad_clip = clip_with("bad-duration", f32::NAN, vec![bind_track(0, 0.5)]);
        bad_clip.duration = f32::NAN;
        assert!(matches!(
            compress(&bad_clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::InvalidDuration { .. })
        ));
    }

    #[test]
    fn structural_errors_out_of_range_duplicate_mismatched_and_non_increasing() {
        let skeleton = chain(2, 0.5);
        let far = track(
            9,
            TrackCurve::Static(Vec3::ZERO),
            TrackCurve::Static(Quat::IDENTITY),
            TrackCurve::Static(Vec3::ONE),
        );
        let clip = clip_with("far", 1.0, vec![far]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::JointOutOfRange { joint: 9, joints: 2 })
        ));

        let clip = clip_with("dup", 1.0, vec![bind_track(0, 0.5), bind_track(0, 0.5)]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::DuplicateTrack { joint: 0 })
        ));

        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(vec![0.0, 1.0], vec![Vec3::ZERO]);
        let clip = clip_with("mismatch", 1.0, vec![root]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::MismatchedKeys {
                times: 2,
                values: 1,
                ..
            })
        ));

        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(vec![0.0, 0.5, 0.5], vec![Vec3::ZERO, Vec3::ONE, Vec3::splat(2.0)]);
        let clip = clip_with("flat-times", 1.0, vec![root]);
        assert!(matches!(
            compress(&clip, &skeleton, &CompressOptions::default()),
            Err(CompressError::NonIncreasingTimes { key: 2, .. })
        ));
    }

    #[test]
    fn smallest_three_48_round_trips_within_a_milliradian() {
        let cases = [
            Quat::IDENTITY,
            Quat::from_rotation_y(1.2),
            Quat::from_axis_angle(Vec3::new(1.0, 2.0, 3.0).normalize(), -2.4),
            Quat::from_xyzw(-0.99, 0.0, 0.0, 0.14106736).normalize(),
            Quat::from_xyzw(0.5, 0.5, 0.5, 0.5),
            Quat::from_rotation_x(std::f32::consts::PI),
        ];
        for q in cases {
            let back = unpack_rotation_48(pack_rotation_48(q));
            let angle = back.angle_between(normalize_quat(q));
            assert!(angle < 1e-3, "48-bit angle error {angle} for {q:?}");
            assert!((back.length() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn smallest_three_32_round_trips_coarser_than_48() {
        let mut rng = Rng(7);
        let mut worst32 = 0.0f32;
        let mut worst48 = 0.0f32;
        for _ in 0..64 {
            let axis = Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
            if axis.length_squared() < 1e-4 {
                continue;
            }
            let q = Quat::from_axis_angle(axis.normalize(), rng.range(-3.0, 3.0));
            worst32 = worst32.max(unpack_rotation_32(pack_rotation_32(q)).angle_between(q));
            worst48 = worst48.max(unpack_rotation_48(pack_rotation_48(q)).angle_between(q));
        }
        assert!(worst32 < 0.02, "32-bit worst angle error {worst32}");
        assert!(
            worst48 < worst32,
            "48-bit ({worst48}) must beat 32-bit ({worst32})"
        );
        assert_eq!(RotationFormat::Bits32.bytes(), 4);
        assert_eq!(RotationFormat::Bits48.bytes(), 6);
    }

    #[test]
    fn rotation_format_auto_and_forced_preferences() {
        let skeleton = chain(1, 0.5);
        let make_clip = || {
            let mut root = bind_track(0, 0.5);
            root.rotations = animated_rot(
                vec![0.0, 0.5, 1.0],
                vec![
                    Quat::IDENTITY,
                    Quat::from_rotation_y(1.0),
                    Quat::from_rotation_y(-0.5),
                ],
            );
            clip_with("spin", 1.0, vec![root])
        };
        let loose = compress(&make_clip(), &skeleton, &CompressOptions::new(1.0)).expect("loose");
        assert_eq!(
            loose.track_for(0).expect("track").rotations.format(),
            Some(RotationFormat::Bits32),
            "a generous tolerance buys the 32-bit format"
        );
        let tight = compress(&make_clip(), &skeleton, &CompressOptions::new(0.002)).expect("tight");
        assert_eq!(
            tight.track_for(0).expect("track").rotations.format(),
            Some(RotationFormat::Bits48),
            "a tight tolerance needs the 48-bit format"
        );
        let mut forced = CompressOptions::new(1.0);
        forced.rotation_format = RotationFormatPreference::Force48;
        let clip48 = compress(&make_clip(), &skeleton, &forced).expect("forced 48");
        assert_eq!(
            clip48.track_for(0).expect("track").rotations.format(),
            Some(RotationFormat::Bits48)
        );
        forced.rotation_format = RotationFormatPreference::Force32;
        let clip32 = compress(&make_clip(), &skeleton, &forced).expect("forced 32");
        assert_eq!(
            clip32.track_for(0).expect("track").rotations.format(),
            Some(RotationFormat::Bits32)
        );
    }

    #[test]
    fn sample_clamps_outside_the_key_range_and_survives_nan_time() {
        let skeleton = chain(1, 0.5);
        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(
            vec![0.25, 0.75],
            vec![Vec3::new(1.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)],
        );
        let clip = clip_with("clamp", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::new(0.01)).expect("compress");
        let sample_x = |time: f32| {
            let mut pose = skeleton.bind_pose_pose();
            compressed.sample(time, &mut pose);
            pose.translations[0].x
        };
        assert!((sample_x(-10.0) - 1.0).abs() < 1e-3);
        assert!((sample_x(10.0) - 3.0).abs() < 1e-3);
        assert!((sample_x(0.5) - 2.0).abs() < 1e-3);
        assert!(
            (sample_x(f32::NAN) - 1.0).abs() < 1e-3,
            "NaN time samples the first key"
        );
        assert!((sample_x(f32::INFINITY) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn sampling_into_a_short_pose_does_not_panic() {
        let skeleton = chain(6, 0.5);
        let clip = random_clip(3, 6, 0.5);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        let mut short = Pose::from_identity(1);
        compressed.sample(0.5, &mut short);
        let mut empty = Pose::default();
        compressed.sample(0.5, &mut empty);
        assert_eq!(short.len(), 1);
        assert!(empty.is_empty());
    }

    #[test]
    fn empty_animated_curves_are_treated_as_dropped() {
        let skeleton = chain(1, 0.5);
        let root = track(
            0,
            animated_vec(Vec::new(), Vec::new()),
            animated_rot(Vec::new(), Vec::new()),
            animated_vec(Vec::new(), Vec::new()),
        );
        let clip = clip_with("empty-curves", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        let t = compressed.track_for(0).expect("track");
        assert!(t.translations.is_dropped() && t.rotations.is_dropped() && t.scales.is_dropped());
    }

    #[test]
    fn per_bone_tolerance_and_shell_change_the_outcome() {
        let skeleton = chain(1, 0.5);
        let make_clip = || {
            let mut root = bind_track(0, 0.5);
            root.rotations = animated_rot(
                vec![0.0, 0.5, 1.0],
                vec![Quat::IDENTITY, Quat::from_rotation_z(0.1), Quat::IDENTITY],
            );
            clip_with("bump", 1.0, vec![root])
        };
        // A huge per-bone tolerance lets the whole bump vanish.
        let mut loose = CompressOptions::new(0.001);
        loose.per_bone_tolerance = vec![10.0];
        let compressed = compress(&make_clip(), &skeleton, &loose).expect("loose bone");
        assert!(compressed.track_for(0).expect("track").rotations.key_count() <= 1);
        // A zero shell charges a leaf rotation nothing at its own origin.
        let mut no_shell = CompressOptions::new(0.01);
        no_shell.shell_radius = 0.0;
        let compressed = compress(&make_clip(), &skeleton, &no_shell).expect("no shell");
        assert!(compressed.track_for(0).expect("track").rotations.is_dropped());
        // A big shell makes the same bump expensive: all keys stay.
        let mut big_shell = CompressOptions::new(0.01);
        big_shell.shell_radius = 5.0;
        let compressed = compress(&make_clip(), &skeleton, &big_shell).expect("big shell");
        assert_eq!(compressed.track_for(0).expect("track").rotations.key_count(), 3);
    }

    #[test]
    fn non_unit_quaternion_keys_are_normalised_not_rejected() {
        let skeleton = chain(1, 0.5);
        let mut root = bind_track(0, 0.5);
        let doubled = Quat::from_rotation_y(0.6) * 2.0;
        root.rotations = TrackCurve::Static(doubled);
        let clip = clip_with("scaled-quat", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::new(0.05)).expect("compress");
        let mut pose = skeleton.bind_pose_pose();
        compressed.sample(0.0, &mut pose);
        assert!((pose.rotations[0].length() - 1.0).abs() < 1e-5);
        assert!(pose.rotations[0].angle_between(Quat::from_rotation_y(0.6)) < 0.01);
    }

    #[test]
    fn report_fields_are_mutually_consistent() {
        let skeleton = chain(4, 0.2);
        let clip = random_clip(9, 4, 0.2);
        let compressed = compress(&clip, &skeleton, &CompressOptions::new(0.05)).expect("compress");
        let report = compressed.report();
        assert_eq!(report.compressed_bytes, compressed.compressed_bytes());
        assert_eq!(report.compressed_keys, compressed.key_count());
        assert_eq!(report.max_error_per_bone.len(), skeleton.len());
        let max_per_bone = report.max_error_per_bone.iter().copied().fold(0.0f32, f32::max);
        assert_eq!(report.max_error, max_per_bone);
        assert!(
            (report.ratio() - report.compressed_bytes as f32 / report.original_bytes as f32).abs() < 1e-6
        );
        assert!(report.original_keys > 0);
    }

    #[test]
    fn untracked_joints_have_no_track_and_keep_their_pose() {
        let skeleton = chain(3, 0.5);
        let mut middle = bind_track(1, 0.5);
        middle.translations = animated_vec(
            vec![0.0, 1.0],
            vec![Vec3::new(0.5, 0.0, 0.0), Vec3::new(0.5, 1.0, 0.0)],
        );
        let clip = clip_with("middle-only", 1.0, vec![middle]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        assert!(compressed.track_for(0).is_none());
        assert!(compressed.track_for(1).is_some());
        assert!(compressed.track_for(2).is_none());
        let mut pose = skeleton.bind_pose_pose();
        pose.translations[2] = Vec3::new(9.0, 9.0, 9.0);
        compressed.sample(1.0, &mut pose);
        assert_eq!(pose.translations[2], Vec3::new(9.0, 9.0, 9.0));
        assert!(pose.translations[1].abs_diff_eq(Vec3::new(0.5, 1.0, 0.0), 1e-3));
    }

    #[test]
    fn reduction_never_removes_the_end_keys() {
        let skeleton = chain(1, 0.5);
        let mut root = bind_track(0, 0.5);
        root.translations = animated_vec(
            vec![0.0, 0.25, 0.5, 0.75, 1.0],
            vec![
                Vec3::new(-1.0, 0.0, 0.0),
                Vec3::ZERO,
                Vec3::new(0.0, 5.0, 0.0),
                Vec3::ZERO,
                Vec3::new(1.0, 0.0, 0.0),
            ],
        );
        let clip = clip_with("spike", 1.0, vec![root]);
        let compressed = compress(&clip, &skeleton, &CompressOptions::new(0.01)).expect("compress");
        let mut pose = skeleton.bind_pose_pose();
        compressed.sample(0.0, &mut pose);
        assert!(pose.translations[0].abs_diff_eq(Vec3::new(-1.0, 0.0, 0.0), 1e-3));
        compressed.sample(1.0, &mut pose);
        assert!(pose.translations[0].abs_diff_eq(Vec3::new(1.0, 0.0, 0.0), 1e-3));
        compressed.sample(0.5, &mut pose);
        assert!(
            pose.translations[0].abs_diff_eq(Vec3::new(0.0, 5.0, 0.0), 1e-2),
            "the spike key survives a tight tolerance, got {:?}",
            pose.translations[0]
        );
    }

    #[test]
    fn exact_rotation_slerp_tracks_stay_within_tolerance() {
        let skeleton = chain(4, 0.2);
        let times: Vec<f32> = (0..=8).map(|i| i as f32 / 8.0).collect();
        let tracks: Vec<JointTrack> = (0..4)
            .map(|j| {
                let mut t = bind_track(j, 0.2);
                t.rotations = TrackCurve::Animated {
                    times: times.clone(),
                    values: times
                        .iter()
                        .map(|&time| Quat::from_rotation_z(0.4 * (time * std::f32::consts::PI).sin()))
                        .collect(),
                    exact_rotation: true,
                };
                t
            })
            .collect();
        let clip = clip_with("slerp", 1.0, tracks);
        let options = CompressOptions::new(0.05);
        let compressed = compress(&clip, &skeleton, &options).expect("compress");
        assert!(compressed.report().max_error <= options.tolerance);
        let oracle = independent_max_errors(&clip, &skeleton, &compressed, &options, 20);
        assert!(oracle.iter().copied().fold(0.0f32, f32::max) <= options.tolerance);
    }

    #[test]
    fn empty_skeleton_and_empty_clip_compress() {
        let skeleton = Skeleton::default();
        let clip = AnimationClip::empty("nothing", 0.0);
        let compressed = compress(&clip, &skeleton, &CompressOptions::default()).expect("compress");
        assert!(compressed.tracks.is_empty());
        assert!(compressed.report().max_error_per_bone.is_empty());
        let mut pose = Pose::default();
        compressed.sample(0.0, &mut pose);
    }

    #[test]
    fn quantised_translation_keys_reproduce_the_range_endpoints() {
        let min = Vec3::new(-2.0, 0.0, 10.0);
        let max = Vec3::new(2.0, 1.0, 10.0);
        for v in [min, max, Vec3::new(0.0, 0.5, 10.0), Vec3::new(1.234, 0.777, 10.0)] {
            let back = dequantize_vec3(quantize_vec3(v, min, max), min, max);
            let err = (back - v).length();
            assert!(err <= 4.0 / 65535.0, "16-bit round-trip error {err} for {v:?}");
        }
        // A degenerate range stays pinned at the single value.
        let pinned = dequantize_vec3(quantize_vec3(max, max, max), max, max);
        assert_eq!(pinned, max);
    }

    #[test]
    fn huge_and_tiny_tolerances_do_not_panic() {
        let skeleton = chain(3, 0.2);
        let clip = random_clip(11, 3, 0.2);
        let huge = compress(&clip, &skeleton, &CompressOptions::new(1.0e6)).expect("huge tolerance");
        assert_eq!(
            huge.key_count(),
            0,
            "everything is within a huge tolerance of bind"
        );
        let tiny = compress(&clip, &skeleton, &CompressOptions::new(1.0e-7)).expect("tiny tolerance");
        assert!(
            tiny.report().compressed_keys >= huge.report().compressed_keys,
            "a tiny tolerance keeps at least as many keys"
        );
        assert!(tiny.report().max_error.is_finite());
    }

    // Lane verification tests (sprint-muse-verify-animcomp): every delivered test uses a
    // straight chain; these cover a branching skeleton and hemisphere-flipped keys.

    /// A small branching humanoid: pelvis, two spine joints, head, two arms and two legs of
    /// three joints each (16 joints, metres).
    fn branching_humanoid() -> Skeleton {
        let mut names = Vec::new();
        let mut parents: Vec<Option<u32>> = Vec::new();
        let mut binds = Vec::new();
        let mut add = |parent: Option<u32>, t: Vec3| -> u32 {
            names.push(format!("b{}", parents.len()));
            parents.push(parent);
            binds.push(translated(t));
            (parents.len() - 1) as u32
        };
        let pelvis = add(None, Vec3::new(0.0, 1.0, 0.0));
        let spine0 = add(Some(pelvis), Vec3::new(0.0, 0.2, 0.0));
        let spine1 = add(Some(spine0), Vec3::new(0.0, 0.2, 0.0));
        add(Some(spine1), Vec3::new(0.0, 0.2, 0.0));
        for side in [-1.0f32, 1.0] {
            let mut arm = spine1;
            for len in [0.15, 0.3, 0.25] {
                arm = add(Some(arm), Vec3::new(side * len, 0.0, 0.0));
            }
            let mut leg = pelvis;
            for len in [0.1, 0.45, 0.43] {
                leg = add(Some(leg), Vec3::new(side * 0.02, -len, 0.0));
            }
        }
        Skeleton::from_local_bind(names, parents, binds).expect("valid humanoid skeleton")
    }

    #[test]
    fn branching_skeleton_round_trip_stays_within_tolerance_on_a_dense_grid() {
        let skeleton = branching_humanoid();
        assert_eq!(skeleton.len(), 16);
        let mut rng = Rng(2326);
        let times: Vec<f32> = (0..=30).map(|i| i as f32 / 30.0).collect();
        let tracks: Vec<JointTrack> = (0..skeleton.len())
            .map(|j| {
                let axis =
                    Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(0.1, 1.0)).normalize();
                let amplitude = rng.range(0.05, 0.5);
                let phase = rng.range(0.0, std::f32::consts::TAU);
                let mut t = track(
                    j as u32,
                    TrackCurve::Static(skeleton.bind_pose[j].translation),
                    animated_rot(
                        times.clone(),
                        times
                            .iter()
                            .map(|&time| {
                                Quat::from_axis_angle(
                                    axis,
                                    amplitude * (std::f32::consts::TAU * time + phase).sin(),
                                )
                            })
                            .collect(),
                    ),
                    TrackCurve::Static(Vec3::ONE),
                );
                if j == 0 {
                    // The root walks forward with a vertical bob.
                    t.translations = animated_vec(
                        times.clone(),
                        times
                            .iter()
                            .map(|&time| {
                                Vec3::new(
                                    0.0,
                                    1.0 + 0.03 * (std::f32::consts::TAU * 2.0 * time).sin(),
                                    1.4 * time,
                                )
                            })
                            .collect(),
                    );
                }
                t
            })
            .collect();
        let clip = clip_with("walk", 1.0, tracks);
        let options = CompressOptions {
            shell_radius: 0.1,
            ..CompressOptions::new(0.01)
        };
        let compressed = compress(&clip, &skeleton, &options).expect("compress humanoid");
        assert!(compressed.report().max_error <= options.tolerance);
        let oracle = independent_max_errors(&clip, &skeleton, &compressed, &options, 600);
        for (joint, error) in oracle.iter().enumerate() {
            assert!(
                *error <= options.tolerance,
                "joint {joint}: dense-grid error {error} exceeds {}",
                options.tolerance
            );
        }
        let reduced = compressed
            .tracks
            .iter()
            .filter(|t| t.rotations.key_count() > 1 && t.rotations.key_count() < times.len())
            .count();
        assert!(reduced > 0, "at least one rotation track loses keys");
    }

    #[test]
    fn hemisphere_flipped_rotation_keys_stay_within_tolerance() {
        // Raw capture data often alternates q and -q (the same rotation); both the source
        // sampler and the decompressor must take the short way between them.
        let skeleton = chain(3, 0.3);
        let times: Vec<f32> = (0..=30).map(|i| i as f32 / 30.0).collect();
        let values: Vec<Quat> = times
            .iter()
            .enumerate()
            .map(|(i, &t)| {
                let q = Quat::from_rotation_z(1.2 * (3.0 * t).sin());
                if i % 2 == 1 { -q } else { q }
            })
            .collect();
        let middle = track(
            1,
            TrackCurve::Static(bind_translation(1, 0.3)),
            animated_rot(times.clone(), values),
            TrackCurve::Static(Vec3::ONE),
        );
        let clip = clip_with("flip", 1.0, vec![middle]);
        let options = CompressOptions::new(0.01);
        let compressed = compress(&clip, &skeleton, &options).expect("compress");
        assert!(compressed.report().max_error <= options.tolerance);
        let oracle = independent_max_errors(&clip, &skeleton, &compressed, &options, 300);
        assert!(oracle.iter().copied().fold(0.0f32, f32::max) <= options.tolerance);
        assert!(
            compressed.track_for(1).expect("track").rotations.key_count() < times.len(),
            "sign flips are not mistaken for motion"
        );
    }
}
