// SPDX-License-Identifier: MPL-2.0
//! Clip compression: tolerance-based key reduction plus key quantisation.
//!
//! Specification: `docs/specs/17-animation.md` § 1:
//! - drop keys within error tolerance (per-track adaptive, position error in metres, rotation
//!   in radians, measured at the joint *and* propagated to a virtual end-effector distance),
//! - quantise rotations with smallest-three packing (2-bit component index plus three
//!   signed 16-bit components, 50 bits in a `u64`; tighter error than 15-bit components),
//! - quantise translations/scales to 16 bits relative to the track range.
//!
//! The quantisation error is part of the error budget (ADR-871): every key is quantised
//! first, and key reduction measures the chord between two *quantised* keys against the
//! original samples it replaces, its own endpoints included (ADR-1387; before, it subtracted
//! the worst-case quantisation error from the tolerance). So the reconstructed curves stay
//! within the tolerance at every checked time, not only at the kept keys. The kept keys need
//! not be samples: a least-squares fit of the inner knot values (ends pinned) centres each
//! chord on the curve it replaces, and a track keeps whichever of the fitted and the
//! interpolating key sets is smaller (ADR-1389). A track whose range
//! is so large that 16-bit steps alone exceed the tolerance (a kilometre-long root track at
//! 5 mm) keeps every key, and [`measure_compression_error`] reports the overshoot so a cook
//! can flag it.
//!
//! Provenance: greedy curve simplification with an error bound (a Douglas–Peucker-style
//! decimation, Douglas & Peucker 1973); least-squares knot values for a piecewise-linear curve
//! with fixed knots (de Boor, *A Practical Guide to Splines*, ch. XIV) solved as a tridiagonal
//! system (Thomas algorithm); smallest-three quaternion packing derived from the
//! unit-norm constraint (a unit quaternion is determined by three components and the sign of
//! the dropped one). See `docs/provenance.md`.

use std::ops::{Add, AddAssign, Mul, Sub, SubAssign};

use regia_core::math::{Quat, Vec3, Vec4};
use serde::{Deserialize, Serialize};

use crate::clip::{
    AnimationClip, JointTrack, MorphNode, MorphTrack, NamedCurve, Notify, PropertyTrack, RootMotionTrack,
    TrackCurve,
};
use crate::num::{det_atan2, det_sin_cos_f64};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Error tolerances for key reduction (`docs/specs/17-animation.md` § 1).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressionSettings {
    /// Maximum allowed translation error at the joint, metres.
    pub position_tolerance: f32,
    /// Maximum allowed rotation error at the joint, radians.
    pub rotation_tolerance: f32,
    /// Maximum allowed scale error.
    pub scale_tolerance: f32,
    /// Maximum allowed error propagated to a virtual end-effector at the far end of the
    /// joint's subtree, metres (`rot_error · chain_length + position_error`).
    pub propagated_tolerance: f32,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            position_tolerance: 0.005,
            rotation_tolerance: 0.005,
            scale_tolerance: 0.005,
            propagated_tolerance: 0.01,
        }
    }
}

/// A quantised vec3 track: 16-bit keys relative to the track range.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum CompressedVec3Track {
    /// Constant value (collapsed track); stored as f32 (one value).
    Static(Vec3),
    /// Timed quantised keys.
    Animated {
        /// Key times in seconds.
        times: Vec<f32>,
        /// Quantised keys: each component normalised to the per-axis range.
        keys: Vec<[u16; 3]>,
        /// Per-axis minimum of the stored keys' range.
        min: Vec3,
        /// Per-axis maximum of the stored keys' range.
        max: Vec3,
    },
}

/// A quantised rotation track: smallest-three packed into `u64`
/// (2-bit dropped-component index + 3 × 16-bit).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum CompressedQuatTrack {
    /// Constant rotation, quantised.
    Static(u64),
    /// Timed quantised keys.
    Animated {
        /// Key times in seconds.
        times: Vec<f32>,
        /// Packed keys.
        keys: Vec<u64>,
    },
}

/// One compressed joint track.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressedJointTrack {
    /// Target joint id.
    pub joint: u32,
    /// Translation keys.
    pub translations: CompressedVec3Track,
    /// Rotation keys.
    pub rotations: CompressedQuatTrack,
    /// Scale keys.
    pub scales: CompressedVec3Track,
}

/// A compressed morph-weight track.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressedMorphTrack {
    /// Morph target index.
    pub target: u32,
    /// Times.
    pub times: Vec<f32>,
    /// 16-bit keys normalised to 0..=1 weights.
    pub keys: Vec<u16>,
    /// The node whose mesh owns the target (`None` applies to every morph mesh; ADR-297).
    pub node: Option<MorphNode>,
}

impl CompressedMorphTrack {
    /// The weight at `time`.
    fn sample(&self, time: f32) -> f32 {
        if self.times.len() < 2 || self.keys.len() < 2 {
            self.keys.first().map(|&k| k as f32 / 65535.0).unwrap_or(0.0)
        } else {
            let (lo, hi, w) = bracket_u16(&self.times, self.keys.len(), time);
            let a = self.keys[lo] as f32 / 65535.0;
            let b = self.keys[hi] as f32 / 65535.0;
            a + (b - a) * w
        }
    }
}

/// A compressed animation clip. Notifies, curves and property tracks are copied unchanged
/// (they are tiny compared to joint data).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompressedClip {
    /// Clip name.
    pub name: String,
    /// Duration in seconds.
    pub duration: f32,
    /// Nominal author sample rate.
    pub sample_rate: f32,
    /// Compressed joint tracks.
    pub tracks: Vec<CompressedJointTrack>,
    /// Compressed morph tracks.
    pub morph_tracks: Vec<CompressedMorphTrack>,
    /// Notifies (copied).
    pub notifies: Vec<Notify>,
    /// Named curves (copied).
    pub curves: Vec<NamedCurve>,
    /// Property tracks (copied).
    pub property_tracks: Vec<PropertyTrack>,
    /// Root motion source (copied).
    pub root_motion: Option<RootMotionTrack>,
    /// Source-node tracks, retained losslessly.
    #[serde(default)]
    pub node_animation: Option<crate::node_tracks::NodeAnimation>,
}

// ---------------------------------------------------------------------------
// Quantisation primitives
// ---------------------------------------------------------------------------

/// Pack a unit quaternion into `u64`: drop the component with the largest magnitude (2-bit
/// index), store the other three as signed 16-bit. The dropped component is made positive
/// first (`q` and `−q` are the same rotation), which makes reconstruction unambiguous.
pub fn pack_quat(q: Quat) -> u64 {
    // A zero or non-finite quaternion has no direction: store identity, not NaN.
    let q = if q.length_squared() > 0.0 && q.is_finite() {
        q.normalize()
    } else {
        Quat::IDENTITY
    };
    let a = [q.x, q.y, q.z, q.w];
    let mut drop = 0usize;
    let mut best = -1.0f32;
    for (i, &c) in a.iter().enumerate() {
        if c.abs() > best {
            best = c.abs();
            drop = i;
        }
    }
    // Sign-normalise so the dropped component is non-negative.
    let sign = if a[drop] < 0.0 { -1.0 } else { 1.0 };
    let mut packed = (drop as u64) << 48;
    let mut comps = [0u64; 3];
    let mut ci = 0;
    for (i, component) in a.iter().enumerate() {
        if i == drop {
            continue;
        }
        let v = (component * sign * 32767.0).round().clamp(-32767.0, 32767.0) as i16;
        comps[ci] = (v as u16 as u64) & 0xFFFF;
        ci += 1;
    }
    packed |= comps[0] | (comps[1] << 16) | (comps[2] << 32);
    packed
}

/// Inverse of [`pack_quat`]. The dropped component is recovered from the unit-norm
/// constraint; error is bounded by the 16-bit rounding (≈1e-4 rad).
pub fn unpack_quat(packed: u64) -> Quat {
    let drop = ((packed >> 48) & 0x3) as usize;
    let unpack16 = |v: u64| (v as u16 as i16) as f32 / 32767.0;
    let mut a = [0.0f32; 4];
    let mut ci = 0;
    for (i, slot) in a.iter_mut().enumerate() {
        if i == drop {
            continue;
        }
        *slot = match ci {
            0 => unpack16(packed & 0xFFFF),
            1 => unpack16((packed >> 16) & 0xFFFF),
            _ => unpack16((packed >> 32) & 0xFFFF),
        };
        ci += 1;
    }
    // The three stored components determine the fourth through |q| = 1 (dropped ≥ 0).
    let kept_sq: f32 = a.iter().map(|c| c * c).sum();
    a[drop] = (1.0 - kept_sq).clamp(0.0, 1.0).sqrt();
    Quat::from_xyzw(a[0], a[1], a[2], a[3]).normalize()
}

/// Angle between two rotations, radians, accurate down to tiny angles.
///
/// `Quat::angle_between` takes the `acos` of a dot product, which in `f32` cannot resolve
/// angles below about 5e-4 rad (the dot rounds to 1), so a cook measuring with it could
/// under-report errors by that much. This measures `2 · atan2(|v|, |w|)` of the relative
/// rotation instead.
pub fn rotation_error(a: Quat, b: Quat) -> f32 {
    let d = a.conjugate() * b;
    2.0 * det_atan2(d.xyz().length(), d.w.abs())
}

/// Whether `2 * atan2(|v|, |w|)` of relative quaternion `d` is at most `tolerance`,
/// decided without rounding that angle to `f32` first.
fn rotation_within_relative(d: Quat, tolerance: f32) -> bool {
    if tolerance.is_nan() || tolerance < 0.0 {
        return false;
    }
    let (x, y, z, w) = (
        f64::from(d.x),
        f64::from(d.y),
        f64::from(d.z),
        f64::from(d.w).abs(),
    );
    if !(x.is_finite() && y.is_finite() && z.is_finite() && w.is_finite()) {
        return false;
    }
    let vector = (x * x + y * y + z * z).sqrt();
    if tolerance >= std::f32::consts::PI {
        return true;
    }
    // On [0, pi], angle <= tolerance iff |v| cos(tolerance/2) <= |w| sin(tolerance/2).
    // Both sides are non-negative here, so this also handles zero and 180-degree quaternions.
    let (sin, cos) = det_sin_cos_f64(f64::from(tolerance) * 0.5);
    vector * cos <= w * sin
}

fn rotation_within(a: Quat, b: Quat, tolerance: f32) -> bool {
    rotation_within_relative(a.conjugate() * b, tolerance)
}

/// The portable angle and the arithmetic threshold decision, from one relative quaternion.
fn rotation_error_and_within(a: Quat, b: Quat, tolerance: f32) -> (f32, bool) {
    let d = a.conjugate() * b;
    (
        2.0 * det_atan2(d.xyz().length(), d.w.abs()),
        rotation_within_relative(d, tolerance),
    )
}

/// Worst-case rotation error of [`pack_quat`] / [`unpack_quat`], radians: half a 16-bit step on
/// each stored component, plus what that does to the reconstructed one.
pub const QUAT_QUANTIZATION_ERROR: f32 = 1.1e-4;

/// Worst-case length of the error 16-bit quantisation adds to a vec3 key of a track whose
/// values span `values` (half a step on each axis).
pub fn vec3_quantization_error(values: &[Vec3]) -> f32 {
    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);
    for &v in values {
        min = min.min(v);
        max = max.max(v);
    }
    if values.is_empty() {
        return 0.0;
    }
    (max - min).max(Vec3::splat(2e-6)).length() / (2.0 * 65535.0)
}

fn quantize_vec3(values: &[Vec3]) -> (Vec<[u16; 3]>, Vec3, Vec3) {
    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);
    for &v in values {
        min = min.min(v);
        max = max.max(v);
    }
    // Degenerate range: centre it so 0.5 reconstructs the constant value.
    if (max - min).length_squared() < 1e-12 {
        min = values.first().copied().unwrap_or(Vec3::ZERO) - Vec3::splat(1e-6);
        max = values.first().copied().unwrap_or(Vec3::ZERO) + Vec3::splat(1e-6);
    }
    let span = max - min;
    let keys = values
        .iter()
        .map(|&v| {
            let n = (v - min) / span;
            [
                (n.x.clamp(0.0, 1.0) * 65535.0).round() as u16,
                (n.y.clamp(0.0, 1.0) * 65535.0).round() as u16,
                (n.z.clamp(0.0, 1.0) * 65535.0).round() as u16,
            ]
        })
        .collect();
    (keys, min, max)
}

fn dequantize_vec3(key: [u16; 3], min: Vec3, max: Vec3) -> Vec3 {
    let span = max - min;
    Vec3::new(
        min.x + span.x * key[0] as f32 / 65535.0,
        min.y + span.y * key[1] as f32 / 65535.0,
        min.z + span.z * key[2] as f32 / 65535.0,
    )
}

// ---------------------------------------------------------------------------
// Key reduction
// ---------------------------------------------------------------------------

/// Maximum distance between the original samples in `[start, end]` and the chord from the
/// stored (quantised) key `from` at `start` to `to` at `end`: the runtime curve's error over
/// that span, quantisation included, and the sample index where it peaks. Both curves are
/// linear between the original keys, so key times are sufficient.
fn vec3_span_error(
    times: &[f32],
    values: &[Vec3],
    from: Vec3,
    to: Vec3,
    start: usize,
    end: usize,
) -> (f32, usize) {
    let span = times[end] - times[start];
    let mut worst = (0.0f32, start);
    for k in start..=end {
        let w = if span > 0.0 {
            (times[k] - times[start]) / span
        } else {
            0.0
        };
        let err = (values[k] - from.lerp(to, w)).length();
        if err > worst.0 {
            worst = (err, k);
        }
    }
    worst
}

/// Maximum rotation error (radians) of the chord from the stored (unpacked) key `from` at
/// `start` to `to` at `end` against the original curve, measured at every original key of the
/// span (its endpoints included, so the packing error counts) and at the midpoints between
/// them (nlerp is not linear in angle), and the sample index nearest to where it peaks. The
/// boolean uses the unrounded relative-quaternion threshold against `tolerance`.
fn quat_span_error(
    times: &[f32],
    values: &[Quat],
    from: Quat,
    to: Quat,
    start: usize,
    end: usize,
    tolerance: f32,
) -> ((f32, bool), usize) {
    let span = times[end] - times[start];
    let lerp_at = |t: f32| -> Quat {
        let w = if span > 0.0 {
            (t - times[start]) / span
        } else {
            0.0
        };
        from.lerp(to, w)
    };
    let mut worst = (0.0f32, start);
    let mut within = true;
    for k in start..=end {
        let (angle, fits) = rotation_error_and_within(values[k], lerp_at(times[k]), tolerance);
        within &= fits;
        if angle > worst.0 {
            worst = (angle, k);
        }
    }
    // Midpoints of every original interval, the last one (end − 1, end) included.
    for k in (start + 1)..=end {
        let mid = (times[k - 1] + times[k]) * 0.5;
        let orig = sample_quat_raw(times, values, mid);
        let (angle, fits) = rotation_error_and_within(orig, lerp_at(mid), tolerance);
        within &= fits;
        if angle > worst.0 {
            worst = (angle, if k - 1 > start { k - 1 } else { k });
        }
    }
    ((worst.0, within), worst.1)
}

/// Least-squares knot values for a piecewise-linear curve through `values` (sampled at
/// `times`) with its knots at the sample indices `knots` (ascending, first `0`, last
/// `values.len() − 1`).
///
/// The two end knots keep their samples, so a looping clip's seam stays exact; the inner knots
/// minimise the summed squared error over every sample. A chord through an arc's samples errs
/// on one side only; the fit centres it on the arc, so a span can grow for the same peak error.
/// The normal equations of the hat basis are tridiagonal and symmetric positive definite (every
/// inner knot has its own sample at weight 1), solved by the Thomas algorithm. With every
/// sample a knot the system is the identity and the samples come back unchanged.
fn fit_knot_values<V>(times: &[f32], values: &[V], knots: &[usize]) -> Vec<V>
where
    V: Copy + Default + Add<Output = V> + Sub<Output = V> + Mul<f32, Output = V> + AddAssign + SubAssign,
{
    let m = knots.len();
    let mut out: Vec<V> = knots.iter().map(|&k| values[k]).collect();
    if m <= 2 {
        return out;
    }
    // Row j of the normal equations: upper[j − 1]·v[j − 1] + diag[j]·v[j] + upper[j]·v[j + 1].
    let mut diag = vec![0.0f32; m];
    let mut upper = vec![0.0f32; m];
    let mut rhs = vec![V::default(); m];
    for s in 0..m - 1 {
        let (a, b) = (knots[s], knots[s + 1]);
        let span = times[b] - times[a];
        // Each sample counts once: a knot's own sample opens the span that starts there.
        for (&t, &y) in times[a..b].iter().zip(&values[a..b]) {
            let w = if span > 0.0 { (t - times[a]) / span } else { 0.0 };
            let u = 1.0 - w;
            diag[s] += u * u;
            upper[s] += u * w;
            diag[s + 1] += w * w;
            rhs[s] += y * u;
            rhs[s + 1] += y * w;
        }
    }
    // The fixed end knots move to the right-hand side; the unknowns are 1..=m − 2.
    let (lo, hi) = (1, m - 2);
    rhs[lo] -= out[0] * upper[0];
    rhs[hi] -= out[m - 1] * upper[m - 2];
    let mut c = vec![0.0f32; m];
    let mut d = vec![V::default(); m];
    for j in lo..=hi {
        let below = if j > lo { upper[j - 1] } else { 0.0 };
        let inv = 1.0 / (diag[j] - below * c[j - 1]);
        c[j] = if j < hi { upper[j] * inv } else { 0.0 };
        d[j] = (rhs[j] - d[j - 1] * below) * inv;
    }
    for j in (lo..=hi).rev() {
        out[j] = if j < hi { d[j] - out[j + 1] * c[j] } else { d[j] };
    }
    out
}

/// Where a least-squares fit starts: the interpolating greedy pass run at these multiples of
/// the tolerance, whose spans the fit then splits wherever they still miss it.
const FIT_START_FACTORS: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

/// Fits knot values to `knots` with `fit` and, while `over` reports a span past the tolerance
/// (with the sample index where its error peaks), adds knots there and fits again.
///
/// A peak on a knot itself (the fit pulled it) splits the spans on both sides of it. Knots only
/// ever grow, so this ends; it returns `None` when a span still fails with no sample left to
/// add (the caller keeps its interpolating keys).
fn refine_knots<F>(
    mut knots: Vec<usize>,
    fit: impl Fn(&[usize]) -> F,
    over: impl Fn(&[usize], &F, usize) -> Option<usize>,
) -> Option<(Vec<usize>, F)> {
    loop {
        let fitted = fit(&knots);
        let last = knots.len() - 1;
        let mut failed = false;
        let mut add = Vec::new();
        for s in 0..last {
            let Some(at) = over(&knots, &fitted, s) else {
                continue;
            };
            failed = true;
            if at > knots[s] && at < knots[s + 1] {
                add.push(at);
            } else {
                for (a, b) in [
                    (knots[s.saturating_sub(1)], knots[s]),
                    (knots[s], knots[s + 1]),
                    (knots[s + 1], knots[(s + 2).min(last)]),
                ] {
                    if b > a + 1 {
                        add.push(a + (b - a) / 2);
                    }
                }
            }
        }
        if !failed {
            return Some((knots, fitted));
        }
        if add.is_empty() {
            return None;
        }
        knots.extend(add);
        knots.sort_unstable();
        knots.dedup();
    }
}

fn sample_quat_raw(times: &[f32], values: &[Quat], t: f32) -> Quat {
    if t <= times[0] {
        return values[0];
    }
    let last = times.len() - 1;
    if t >= times[last] {
        return values[last];
    }
    let mut lo = 0;
    let mut hi = last;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if times[mid] <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let span = times[hi] - times[lo];
    let w = if span > 0.0 { (t - times[lo]) / span } else { 0.0 };
    values[lo].lerp(values[hi], w)
}

/// Longest chord from key 0 that stays inside `within`. Returns the indices to keep.
///
/// Every dropped key is re-checked against the full span back to the last kept key, so the
/// error bound holds for the whole segment and not only for adjacent keys.
fn reduce_keys(n: usize, within: impl Fn(usize, usize) -> bool) -> Vec<usize> {
    if n <= 2 {
        return (0..n).collect();
    }
    let mut keep = vec![0usize];
    let mut anchor = 0usize;
    let mut end = 2usize;
    while end < n {
        if within(anchor, end) {
            end += 1;
        } else {
            keep.push(end - 1);
            anchor = end - 1;
            end = anchor + 2;
        }
    }
    if keep.last().copied() != Some(n - 1) {
        keep.push(n - 1);
    }
    keep
}

/// True when every sample is within `tolerance` of the first (the track can collapse).
fn vec3_is_constant(values: &[Vec3], tolerance: f32) -> bool {
    let Some(&first) = values.first() else {
        return true;
    };
    values.iter().all(|v| (*v - first).length() <= tolerance)
}

fn quat_is_constant(values: &[Quat], angle_tolerance: f32) -> bool {
    let Some(&first) = values.first() else {
        return true;
    };
    values.iter().all(|q| rotation_within(*q, first, angle_tolerance))
}

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// Byte size of a clip's joint tracks stored as f32 keys (time + value), before reduction
/// and quantisation. Constant curves count as one value. This is the denominator of the
/// "compressed ≤ 25% of raw" target in `docs/specs/17-animation.md` § 1.
pub fn raw_joint_bytes(clip: &AnimationClip) -> usize {
    let mut bytes = 0usize;
    for track in &clip.tracks {
        bytes += 4;
        bytes += vec3_raw_bytes(&track.translations);
        bytes += quat_raw_bytes(&track.rotations);
        bytes += vec3_raw_bytes(&track.scales);
    }
    bytes
}

fn vec3_raw_bytes(curve: &TrackCurve<Vec3>) -> usize {
    match curve {
        TrackCurve::Static(_) => 12,
        TrackCurve::Animated { times, .. } => times.len() * (4 + 12),
    }
}

fn quat_raw_bytes(curve: &TrackCurve<Quat>) -> usize {
    match curve {
        TrackCurve::Static(_) => 16,
        TrackCurve::Animated { times, .. } => times.len() * (4 + 16),
    }
}

/// Compress a clip. `skeleton` supplies the per-joint chain length used to propagate rotation
/// error to a virtual end-effector; `None` treats every chain as 1 m.
pub fn compress_clip(
    clip: &AnimationClip,
    skeleton: Option<&Skeleton>,
    settings: &CompressionSettings,
) -> CompressedClip {
    let chain: Vec<f32> = match skeleton {
        Some(s) => s.max_descendant_distance(),
        None => Vec::new(),
    };
    let chain_of = |joint: u32| -> f32 { chain.get(joint as usize).copied().unwrap_or(1.0).max(0.05) };

    let tracks = clip
        .tracks
        .iter()
        .map(|track| compress_joint_track(track, chain_of(track.joint), settings))
        .collect();
    let morph_tracks = clip
        .morph_tracks
        .iter()
        .map(|t| match &t.weights {
            TrackCurve::Static(v) => CompressedMorphTrack {
                target: t.target,
                times: Vec::new(),
                keys: vec![((*v).clamp(0.0, 1.0) * 65535.0).round() as u16],
                node: t.node.clone(),
            },
            TrackCurve::Animated { times, values, .. } => CompressedMorphTrack {
                target: t.target,
                times: times.clone(),
                keys: values
                    .iter()
                    .map(|&v| (v.clamp(0.0, 1.0) * 65535.0).round() as u16)
                    .collect(),
                node: t.node.clone(),
            },
        })
        .collect();

    CompressedClip {
        name: clip.name.clone(),
        duration: clip.duration,
        sample_rate: clip.sample_rate,
        tracks,
        morph_tracks,
        notifies: clip.notifies.clone(),
        curves: clip.curves.clone(),
        property_tracks: clip.property_tracks.clone(),
        root_motion: clip.root_motion.clone(),
        node_animation: clip.node_animation.clone(),
    }
}

fn compress_vec3(curve: &TrackCurve<Vec3>, tolerance: f32) -> CompressedVec3Track {
    match curve {
        TrackCurve::Static(v) => CompressedVec3Track::Static(*v),
        TrackCurve::Animated { values, .. } if vec3_is_constant(values, tolerance) => {
            CompressedVec3Track::Static(values.first().copied().unwrap_or(Vec3::ZERO))
        }
        TrackCurve::Animated { times, values, .. } => {
            // Quantise every key over the track's range first, then reduce against what the
            // runtime reconstructs (ADR-1387).
            let (quantized, min, max) = quantize_vec3(values);
            let recon: Vec<Vec3> = quantized.iter().map(|&k| dequantize_vec3(k, min, max)).collect();
            let keep = reduce_keys(times.len(), |a, b| {
                vec3_span_error(times, values, recon[a], recon[b], a, b).0 <= tolerance
            });
            let mut best = CompressedVec3Track::Animated {
                times: keep.iter().map(|&i| times[i]).collect(),
                keys: keep.iter().map(|&i| quantized[i]).collect(),
                min,
                max,
            };
            // Least-squares knot values need fewer keys on curved motion (ADR-1389); the
            // interpolating keys stay when they are not beaten.
            let mut best_len = keep.len();
            for factor in FIT_START_FACTORS {
                let start = reduce_keys(times.len(), |a, b| {
                    vec3_span_error(times, values, values[a], values[b], a, b).0 <= tolerance * factor
                });
                if start.len() >= best_len {
                    continue;
                }
                let fitted = refine_knots(
                    start,
                    |knots| fit_vec3_keys(times, values, knots),
                    |knots, fit, s| {
                        let (err, at) =
                            vec3_span_error(times, values, fit.3[s], fit.3[s + 1], knots[s], knots[s + 1]);
                        (err > tolerance).then_some(at)
                    },
                );
                if let Some((knots, (keys, min, max, _))) = fitted
                    && knots.len() < best_len
                {
                    best_len = knots.len();
                    best = CompressedVec3Track::Animated {
                        times: knots.iter().map(|&i| times[i]).collect(),
                        keys,
                        min,
                        max,
                    };
                }
            }
            best
        }
    }
}

/// Least-squares vec3 knot values at `knots`, quantised over their own range: the stored keys,
/// the range, and what the runtime reconstructs from them.
fn fit_vec3_keys(times: &[f32], values: &[Vec3], knots: &[usize]) -> FittedVec3 {
    let fitted = fit_knot_values(times, values, knots);
    let (keys, min, max) = quantize_vec3(&fitted);
    let recon = keys.iter().map(|&k| dequantize_vec3(k, min, max)).collect();
    (keys, min, max, recon)
}

/// Stored keys, range and reconstructed values of a fitted vec3 track.
type FittedVec3 = (Vec<[u16; 3]>, Vec3, Vec3, Vec<Vec3>);

/// Least-squares rotation knot values at `knots` (`aligned` holds the samples as one
/// hemisphere-continuous 4-vector curve), normalised and packed: the stored keys and what the
/// runtime unpacks from them.
fn fit_quat_keys(times: &[f32], values: &[Quat], aligned: &[Vec4], knots: &[usize]) -> (Vec<u64>, Vec<Quat>) {
    let fitted = fit_knot_values(times, aligned, knots);
    let packed: Vec<u64> = fitted
        .iter()
        .zip(knots)
        .map(|(v, &k)| pack_quat(v.try_normalize().map_or(values[k], Quat::from_vec4)))
        .collect();
    let recon = packed.iter().map(|&p| unpack_quat(p)).collect();
    (packed, recon)
}

/// The curve with unequal time and value counts cut to the keys that have both, and an
/// animated curve left without keys read as `fallback` (the same reading the sampler gives).
fn well_formed<T: Copy>(curve: &TrackCurve<T>, fallback: T) -> std::borrow::Cow<'_, TrackCurve<T>> {
    use std::borrow::Cow;
    match curve {
        TrackCurve::Animated {
            times,
            values,
            exact_rotation,
        } if times.len() != values.len() || times.is_empty() => {
            let n = times.len().min(values.len());
            if n == 0 {
                return Cow::Owned(TrackCurve::Static(fallback));
            }
            Cow::Owned(TrackCurve::Animated {
                times: times[..n].to_vec(),
                values: values[..n].to_vec(),
                exact_rotation: *exact_rotation,
            })
        }
        _ => Cow::Borrowed(curve),
    }
}

fn compress_joint_track(
    track: &JointTrack,
    chain_len: f32,
    settings: &CompressionSettings,
) -> CompressedJointTrack {
    let translations = compress_vec3(
        &well_formed(&track.translations, Vec3::ZERO),
        settings.position_tolerance.min(settings.propagated_tolerance),
    );
    let scales = compress_vec3(&well_formed(&track.scales, Vec3::ONE), settings.scale_tolerance);
    // A track collapsed to one packed key leaves room for the smallest-three quantisation error.
    let rot_budget = (settings.rotation_tolerance - QUAT_QUANTIZATION_ERROR).max(0.0);
    let propagated_budget = (settings.propagated_tolerance - QUAT_QUANTIZATION_ERROR * chain_len).max(0.0);
    let rot_tol = rot_budget.min(propagated_budget / chain_len);
    let rotations = match &*well_formed(&track.rotations, Quat::IDENTITY) {
        TrackCurve::Static(v) => CompressedQuatTrack::Static(pack_quat(*v)),
        TrackCurve::Animated { values, .. } if quat_is_constant(values, rot_tol) => {
            CompressedQuatTrack::Static(pack_quat(values.first().copied().unwrap_or(Quat::IDENTITY)))
        }
        TrackCurve::Animated { times, values, .. } => {
            // Reduce against the unpacked keys the runtime reconstructs (ADR-1387).
            let packed: Vec<_> = values.iter().map(|&q| pack_quat(q)).collect();
            let recon: Vec<Quat> = packed.iter().map(|&p| unpack_quat(p)).collect();
            let limit = if settings.rotation_tolerance.is_nan() || settings.propagated_tolerance.is_nan() {
                f32::NAN
            } else {
                settings
                    .rotation_tolerance
                    .min(settings.propagated_tolerance / chain_len)
            };
            let keep = reduce_keys(times.len(), |a, b| {
                quat_span_error(times, values, recon[a], recon[b], a, b, limit)
                    .0
                    .1
            });
            let mut best_times: Vec<f32> = keep.iter().map(|&i| times[i]).collect();
            let mut best_keys: Vec<u64> = keep.iter().map(|&i| packed[i]).collect();
            // Least-squares knot values, fitted on the samples as one hemisphere-continuous
            // 4-vector curve (ADR-1389); the interpolating keys stay when they are not beaten.
            let mut aligned: Vec<Vec4> = Vec::with_capacity(values.len());
            for &q in values {
                let v = Vec4::from(q);
                aligned.push(match aligned.last() {
                    Some(prev) if prev.dot(v) < 0.0 => -v,
                    _ => v,
                });
            }
            for factor in FIT_START_FACTORS {
                let start = reduce_keys(times.len(), |a, b| {
                    quat_span_error(times, values, values[a], values[b], a, b, limit * factor)
                        .0
                        .1
                });
                if start.len() >= best_keys.len() {
                    continue;
                }
                let fitted = refine_knots(
                    start,
                    |knots| fit_quat_keys(times, values, &aligned, knots),
                    |knots, fit, s| {
                        let ((_, within), at) = quat_span_error(
                            times,
                            values,
                            fit.1[s],
                            fit.1[s + 1],
                            knots[s],
                            knots[s + 1],
                            limit,
                        );
                        (!within).then_some(at)
                    },
                );
                if let Some((knots, (keys, _))) = fitted
                    && knots.len() < best_keys.len()
                {
                    best_times = knots.iter().map(|&i| times[i]).collect();
                    best_keys = keys;
                }
            }
            CompressedQuatTrack::Animated {
                times: best_times,
                keys: best_keys,
            }
        }
    };
    CompressedJointTrack {
        joint: track.joint,
        translations,
        rotations,
        scales,
    }
}

/// Decompress into a raw clip (editor, inspection, exact re-cook). Quantisation error remains
/// (the compressed form is lossy within tolerance).
pub fn decompress_clip(compressed: &CompressedClip) -> AnimationClip {
    let tracks = compressed.tracks.iter().map(decompress_joint_track).collect();
    let morph_tracks = compressed
        .morph_tracks
        .iter()
        .map(|t| {
            let values: Vec<f32> = t.keys.iter().map(|&k| k as f32 / 65535.0).collect();
            MorphTrack {
                target: t.target,
                weights: if t.times.is_empty() {
                    TrackCurve::Static(values.first().copied().unwrap_or(0.0))
                } else {
                    TrackCurve::Animated {
                        times: t.times.clone(),
                        values,
                        exact_rotation: false,
                    }
                },
                node: t.node.clone(),
            }
        })
        .collect();
    AnimationClip {
        name: compressed.name.clone(),
        duration: compressed.duration,
        sample_rate: compressed.sample_rate,
        tracks,
        morph_tracks,
        notifies: compressed.notifies.clone(),
        curves: compressed.curves.clone(),
        property_tracks: compressed.property_tracks.clone(),
        root_motion: compressed.root_motion.clone(),
        node_animation: compressed.node_animation.clone(),
    }
}

/// Decompress one joint track (nlerp rotations; the cooked clip payload restores a slerp flag).
pub(crate) fn decompress_joint_track(t: &CompressedJointTrack) -> JointTrack {
    JointTrack {
        joint: t.joint,
        translations: decompress_vec3(&t.translations),
        rotations: match &t.rotations {
            CompressedQuatTrack::Static(q) => TrackCurve::Static(unpack_quat(*q)),
            CompressedQuatTrack::Animated { times, keys } => TrackCurve::Animated {
                times: times.clone(),
                values: keys.iter().map(|&k| unpack_quat(k)).collect(),
                exact_rotation: false,
            },
        },
        scales: decompress_vec3(&t.scales),
    }
}

fn decompress_vec3(track: &CompressedVec3Track) -> TrackCurve<Vec3> {
    match track {
        CompressedVec3Track::Static(v) => TrackCurve::Static(*v),
        CompressedVec3Track::Animated {
            times,
            keys,
            min,
            max,
        } => TrackCurve::Animated {
            times: times.clone(),
            values: keys.iter().map(|&k| dequantize_vec3(k, *min, *max)).collect(),
            exact_rotation: false,
        },
    }
}

/// A compressed vec3 track at `time`; `None` for an animated track without keys.
fn sample_compressed_vec3(track: &CompressedVec3Track, time: f32) -> Option<Vec3> {
    match track {
        CompressedVec3Track::Static(v) => Some(*v),
        CompressedVec3Track::Animated {
            times,
            keys,
            min,
            max,
        } => {
            if keys.is_empty() {
                return None;
            }
            let (lo, hi, w) = bracket_u16(times, keys.len(), time);
            Some(dequantize_vec3(keys[lo], *min, *max).lerp(dequantize_vec3(keys[hi], *min, *max), w))
        }
    }
}

impl CompressedClip {
    /// Sample all joint tracks into `pose` at `time`.
    pub fn sample_pose(&self, time: f32, pose: &mut Pose) {
        for track in &self.tracks {
            let i = track.joint as usize;
            if i >= pose.len() {
                continue;
            }
            // A track with no keys (a corrupt file) leaves the joint's value alone.
            if let Some(v) = sample_compressed_vec3(&track.translations, time) {
                pose.translations[i] = v;
            }
            if let Some(v) = sample_compressed_vec3(&track.scales, time) {
                pose.scales[i] = v;
            }
            match &track.rotations {
                CompressedQuatTrack::Static(q) => pose.rotations[i] = unpack_quat(*q),
                CompressedQuatTrack::Animated { times, keys } if !keys.is_empty() => {
                    let (lo, hi, w) = bracket_u16(times, keys.len(), time);
                    pose.rotations[i] = unpack_quat(keys[lo]).lerp(unpack_quat(keys[hi]), w);
                }
                CompressedQuatTrack::Animated { .. } => {}
            }
        }
    }

    /// Sample the morph weights that apply to every morph mesh (tracks without a node) at `time`
    /// into `(target index, weight)` pairs, as [`AnimationClip::sample_morphs`] does. Per-node
    /// tracks are sampled by [`CompressedClip::sample_node_morphs`].
    pub fn sample_morphs(&self, time: f32, out: &mut Vec<(u32, f32)>) {
        out.clear();
        for track in self.morph_tracks.iter().filter(|t| t.node.is_none()) {
            out.push((track.target, track.sample(time)));
        }
    }

    /// Sample the per-node morph tracks at `time` into `(node index, target index, weight)`, as
    /// [`AnimationClip::sample_node_morphs`] does.
    pub fn sample_node_morphs(&self, time: f32, out: &mut Vec<(u32, u32, f32)>) {
        out.clear();
        for track in &self.morph_tracks {
            if let Some(node) = &track.node {
                out.push((node.index, track.target, track.sample(time)));
            }
        }
    }

    /// Estimated size in bytes of the compressed joint data (what the cook writes).
    pub fn compressed_size(&self) -> usize {
        let mut bytes = 0usize;
        for track in &self.tracks {
            bytes += 4; // joint id
            bytes += match &track.translations {
                CompressedVec3Track::Static(_) => 12,
                CompressedVec3Track::Animated { times, keys, .. } => times.len() * 4 + keys.len() * 6 + 24,
            };
            bytes += match &track.scales {
                CompressedVec3Track::Static(_) => 12,
                CompressedVec3Track::Animated { times, keys, .. } => times.len() * 4 + keys.len() * 6 + 24,
            };
            bytes += match &track.rotations {
                CompressedQuatTrack::Static(_) => 8,
                CompressedQuatTrack::Animated { times, keys } => times.len() * 4 + keys.len() * 8,
            };
        }
        bytes
    }

    /// f32 size of the keys that survived reduction (not the original clip; see
    /// [`raw_joint_bytes`] for the acceptance comparison).
    pub fn raw_equivalent_size(&self) -> usize {
        // Statics count as one key; keys are the compressed key counts (reduction only removes
        // keys, so the raw clip had at least this many — this measures the same key set raw).
        let mut bytes = 0usize;
        for track in &self.tracks {
            bytes += 4;
            bytes += match &track.translations {
                CompressedVec3Track::Static(_) => 4 + 12,
                CompressedVec3Track::Animated { times, .. } => times.len() * (4 + 12),
            };
            bytes += match &track.scales {
                CompressedVec3Track::Static(_) => 4 + 12,
                CompressedVec3Track::Animated { times, .. } => times.len() * (4 + 12),
            };
            bytes += match &track.rotations {
                CompressedQuatTrack::Static(_) => 4 + 16,
                CompressedQuatTrack::Animated { times, .. } => times.len() * (4 + 16),
            };
        }
        bytes
    }

    /// Serialize to postcard bytes (the cooked payload).
    ///
    /// # Errors
    ///
    /// Returns a postcard error for pathological sizes only.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize from postcard bytes.
    ///
    /// # Errors
    ///
    /// Returns a postcard error when the bytes are not a valid `CompressedClip`.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

/// The error a compressed clip actually has against its source, measured by
/// [`measure_compression_error`] (ADR-871).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompressionError {
    /// Largest local translation error of any joint, metres.
    pub max_position: f32,
    /// Largest local rotation error of any joint, radians.
    pub max_rotation: f32,
    /// Largest local scale error of any joint (length of the per-axis difference).
    pub max_scale: f32,
    /// Largest object-space joint position error with every ancestor's error accumulated,
    /// metres: what a viewer sees. Zero when measured without a skeleton.
    pub max_object_position: f32,
    /// The joint with the largest object-space error (the largest local position error when
    /// measured without a skeleton); `None` for a clip without joint tracks.
    pub worst_joint: Option<u32>,
    /// Clip time of that worst error, seconds.
    pub worst_time: f32,
    /// How many clip times were measured.
    pub samples: usize,
}

impl CompressionError {
    /// True when the measured local errors respect `settings`: translations within
    /// `min(position_tolerance, propagated_tolerance)`, rotations within `rotation_tolerance`
    /// and scales within `scale_tolerance`, each plus `slack` for float rounding.
    pub fn within(&self, settings: &CompressionSettings, slack: f32) -> bool {
        self.max_position <= settings.position_tolerance.min(settings.propagated_tolerance) + slack
            && self.max_rotation <= settings.rotation_tolerance + slack
            && self.max_scale <= settings.scale_tolerance + slack
    }
}

/// The most clip times [`measure_compression_error`] samples; longer key sets are thinned
/// evenly.
pub const MAX_ERROR_SAMPLES: usize = 4096;

/// Without a skeleton, [`measure_compression_error`] skips tracks whose joint index is at or
/// past this (a corrupt index); with one, it skips tracks past the skeleton's joints.
pub const MAX_MEASURED_JOINTS: usize = 1 << 16;

/// Measure how far `compressed` strays from `original`.
///
/// The clip is sampled at every key time of either clip plus the midpoint of every interval
/// between them (piecewise-linear errors peak at keys; the midpoints catch the nlerp bulge),
/// up to [`MAX_ERROR_SAMPLES`] times. With a `skeleton`, poses start from its bind pose and
/// the object-space error of every joint is measured too; without one, poses start from
/// identity and only the local errors are reported.
pub fn measure_compression_error(
    original: &AnimationClip,
    compressed: &CompressedClip,
    skeleton: Option<&Skeleton>,
) -> CompressionError {
    let mut times: Vec<f32> = vec![0.0, original.duration.max(0.0)];
    for track in &original.tracks {
        for curve_times in [
            curve_times_vec3(&track.translations),
            curve_times_quat(&track.rotations),
            curve_times_vec3(&track.scales),
        ] {
            times.extend_from_slice(curve_times);
        }
    }
    for track in &compressed.tracks {
        for curve_times in [
            compressed_times_vec3(&track.translations),
            compressed_times_quat(&track.rotations),
            compressed_times_vec3(&track.scales),
        ] {
            times.extend_from_slice(curve_times);
        }
    }
    times.retain(|t| t.is_finite());
    times.sort_by(f32::total_cmp);
    times.dedup();
    let mut samples: Vec<f32> = Vec::with_capacity(times.len() * 2);
    for pair in times.windows(2) {
        samples.push(pair[0]);
        samples.push((pair[0] + pair[1]) * 0.5);
    }
    samples.extend(times.last().copied());
    if samples.len() > MAX_ERROR_SAMPLES {
        let stride = samples.len().div_ceil(MAX_ERROR_SAMPLES);
        samples = samples.into_iter().step_by(stride).collect();
    }

    // Tracks at or past the skeleton (or, without one, past MAX_MEASURED_JOINTS) cannot play
    // and are not measured: sizing the pose from a corrupt joint id (u32::MAX) allocated
    // hundreds of GB and aborted the process.
    let limit = skeleton.map_or(MAX_MEASURED_JOINTS, Skeleton::len);
    let joints = original
        .tracks
        .iter()
        .map(|t| t.joint as usize)
        .chain(compressed.tracks.iter().map(|t| t.joint as usize))
        .filter(|&joint| joint < limit)
        .map(|joint| joint + 1)
        .max()
        .unwrap_or(0);
    let base = match skeleton {
        Some(s) => s.bind_pose_pose(),
        None => Pose::from_identity(joints),
    };
    let use_skeleton = skeleton.filter(|s| s.len() == base.len());
    let tracked: Vec<usize> = original.tracks.iter().map(|t| t.joint as usize).collect();

    let mut report = CompressionError {
        samples: samples.len(),
        ..CompressionError::default()
    };
    let mut worst = -1.0f32;
    let mut raw = base.clone();
    let mut packed = base.clone();
    for &time in &samples {
        raw.copy_from(&base);
        packed.copy_from(&base);
        original.sample_pose(time, &mut raw);
        compressed.sample_pose(time, &mut packed);
        for &j in &tracked {
            if j >= raw.len() {
                continue;
            }
            let position = (raw.translations[j] - packed.translations[j]).length();
            let rotation = rotation_error(raw.rotations[j], packed.rotations[j]);
            let scale = (raw.scales[j] - packed.scales[j]).length();
            report.max_position = report.max_position.max(position);
            report.max_rotation = report.max_rotation.max(rotation);
            report.max_scale = report.max_scale.max(scale);
            if use_skeleton.is_none() && position > worst {
                worst = position;
                report.worst_joint = Some(j as u32);
                report.worst_time = time;
            }
        }
        if let Some(skeleton) = use_skeleton {
            let a = skeleton.globals(&raw, None);
            let b = skeleton.globals(&packed, None);
            for (j, (ga, gb)) in a.iter().zip(&b).enumerate() {
                let error = (ga.w_axis - gb.w_axis).truncate().length();
                report.max_object_position = report.max_object_position.max(error);
                if error > worst {
                    worst = error;
                    report.worst_joint = Some(j as u32);
                    report.worst_time = time;
                }
            }
        }
    }
    report
}

fn curve_times_vec3(curve: &TrackCurve<Vec3>) -> &[f32] {
    match curve {
        TrackCurve::Static(_) => &[],
        TrackCurve::Animated { times, .. } => times,
    }
}

fn curve_times_quat(curve: &TrackCurve<Quat>) -> &[f32] {
    match curve {
        TrackCurve::Static(_) => &[],
        TrackCurve::Animated { times, .. } => times,
    }
}

fn compressed_times_vec3(track: &CompressedVec3Track) -> &[f32] {
    match track {
        CompressedVec3Track::Static(_) => &[],
        CompressedVec3Track::Animated { times, .. } => times,
    }
}

fn compressed_times_quat(track: &CompressedQuatTrack) -> &[f32] {
    match track {
        CompressedQuatTrack::Static(_) => &[],
        CompressedQuatTrack::Animated { times, .. } => times,
    }
}

/// Find the bracketing key pair for `time`; clamps at the ends. `len` is the key count.
///
/// A key count past `times` (a corrupt file) is cut to the times there are, and a NaN `time`
/// reads the first key.
fn bracket_u16(times: &[f32], len: usize, time: f32) -> (usize, usize, f32) {
    let len = len.min(times.len());
    if len == 0 {
        return (0, 0, 0.0);
    }
    if len == 1 || time.is_nan() || time <= times[0] {
        return (0, 0, 0.0);
    }
    let last = len - 1;
    if time >= times[last] {
        return (last, last, 0.0);
    }
    let mut lo = 0;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Notify;

    fn dense_clip() -> AnimationClip {
        // 20 joints; 8 animate smooth sines at 60 keys/s over 2 s, the rest are constant.
        let n = 20usize;
        let keys = 120usize;
        let mut clip = AnimationClip::empty("dense", 2.0);
        clip.sample_rate = 60.0;
        for joint in 0..n {
            let animated = joint % 3 != 0; // ~13 joints animated, 7 constant
            let mut track = JointTrack {
                joint: joint as u32,
                translations: TrackCurve::Static(Vec3::new(0.0, joint as f32 * 0.1, 0.0)),
                rotations: TrackCurve::Static(Quat::IDENTITY),
                scales: TrackCurve::Static(Vec3::ONE),
            };
            if animated {
                let mut times = Vec::with_capacity(keys);
                let mut pos = Vec::with_capacity(keys);
                let mut rot = Vec::with_capacity(keys);
                for k in 0..keys {
                    let t = k as f32 * (2.0 / (keys - 1) as f32);
                    times.push(t);
                    let phase = t * std::f32::consts::PI; // one half sine over the clip
                    pos.push(Vec3::new(
                        0.4 * phase.sin(),
                        joint as f32 * 0.1,
                        0.1 * (phase * 3.0).sin(),
                    ));
                    rot.push(Quat::from_rotation_y(0.6 * (phase + joint as f32).sin()));
                }
                track.translations = TrackCurve::Animated {
                    times,
                    values: pos,
                    exact_rotation: false,
                };
                track.rotations = TrackCurve::Animated {
                    times: (0..keys).map(|k| k as f32 * (2.0 / (keys - 1) as f32)).collect(),
                    values: rot,
                    exact_rotation: false,
                };
            }
            clip.tracks.push(track);
        }
        clip
    }

    #[test]
    fn pack_unpack_quat_round_trip() {
        for &q in &[
            Quat::IDENTITY,
            Quat::from_rotation_y(1.0),
            Quat::from_rotation_x(-2.5),
            Quat::from_rotation_z(3.0),
            Quat::from_xyzw(
                std::f32::consts::FRAC_1_SQRT_2,
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
            ),
        ] {
            let packed = pack_quat(q);
            let unpacked = unpack_quat(packed);
            assert!(unpacked.angle_between(q) < 2e-4, "quat error too large");
        }
    }

    #[test]
    fn pack_handles_negative_dominant_component() {
        // Rotations near 180° have a dominant negative component in some frames.
        let q = Quat::from_xyzw(-0.99, 0.0, 0.0, 0.14106736);
        let unpacked = unpack_quat(pack_quat(q));
        // q and -q are the same rotation; compare rotations, not components.
        let same = unpacked.angle_between(q) < 2e-4 || unpacked.angle_between(-q) < 2e-4;
        assert!(same);
    }

    #[test]
    fn compression_hits_size_target_on_dense_clip() {
        let clip = dense_clip();
        // A 20-joint chain skeleton supplies propagation distances.
        let names: Vec<String> = (0..20).map(|i| format!("j{i}")).collect();
        let parents: Vec<Option<u32>> = (0..20)
            .map(|i| if i == 0 { None } else { Some(i as u32 - 1) })
            .collect();
        let bind: Vec<_> = (0..20)
            .map(|i| crate::pose::translated(Vec3::new(0.0, 0.1 * i as f32, 0.0)))
            .collect();
        let sk = Skeleton::from_local_bind(names, parents, bind).expect("valid");

        let compressed = compress_clip(&clip, Some(&sk), &CompressionSettings::default());
        let c = compressed.compressed_size();
        let r = raw_joint_bytes(&clip);
        eprintln!(
            "dense clip: {c} of {r} raw bytes ({:.1} %)",
            100.0 * c as f64 / r as f64
        );
        assert!(
            (c as f64) <= 0.25 * r as f64,
            "compressed {c} bytes vs raw {r} bytes (ratio {:.3})",
            c as f64 / r as f64
        );
    }

    #[test]
    fn compression_error_stays_within_tolerance() {
        let clip = dense_clip();
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        let decompressed = decompress_clip(&compressed);

        let mut pose_raw = Pose::from_identity(20);
        let mut pose_dec = Pose::from_identity(20);
        let mut max_pos_err = 0.0f32;
        let mut max_rot_err = 0.0f32;
        for step in 0..=400 {
            let t = step as f32 * 0.005;
            clip.sample_pose(t, &mut pose_raw);
            decompressed.sample_pose(t, &mut pose_dec);
            for j in 0..20 {
                max_pos_err = max_pos_err.max((pose_raw.translations[j] - pose_dec.translations[j]).length());
                max_rot_err = max_rot_err.max(pose_raw.rotations[j].angle_between(pose_dec.rotations[j]));
            }
        }
        // Tolerances + quantisation slack.
        assert!(max_pos_err < 0.0065, "max position error {max_pos_err}");
        assert!(max_rot_err < 0.0065, "max rotation error {max_rot_err}");
    }

    #[test]
    fn compressed_sampling_matches_decompressed() {
        let clip = dense_clip();
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        let decompressed = decompress_clip(&compressed);
        let mut a = Pose::from_identity(20);
        let mut b = Pose::from_identity(20);
        for step in 0..=100 {
            let t = step as f32 * 0.02;
            compressed.sample_pose(t, &mut a);
            decompressed.sample_pose(t, &mut b);
            for j in 0..20 {
                assert!((a.translations[j] - b.translations[j]).length() < 1e-5);
                let rot_err = a.rotations[j].angle_between(b.rotations[j]);
                assert!(rot_err < 2e-3, "joint {j} t {t}: rotation differs by {rot_err}");
                assert!(
                    (a.scales[j] - b.scales[j]).length() < 1e-4,
                    "joint {j} t {t}: scale {:?} vs {:?}",
                    a.scales[j],
                    b.scales[j]
                );
            }
        }
    }

    #[test]
    fn postcard_round_trip() {
        let clip = dense_clip();
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        let bytes = compressed.to_postcard().expect("encode");
        let back = CompressedClip::from_postcard(&bytes).expect("decode");
        assert_eq!(back, compressed);
    }

    #[test]
    fn constant_tracks_collapse_to_static() {
        let mut clip = AnimationClip::empty("c", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 0.5, 1.0],
                values: vec![Vec3::ZERO, Vec3::ZERO, Vec3::ZERO],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        match &compressed.tracks[0].translations {
            CompressedVec3Track::Static(v) => {
                assert!(
                    v.abs_diff_eq(Vec3::ZERO, 1e-6),
                    "constant track collapses to its value"
                );
            }
            other => panic!("expected a collapsed static track, got {other:?}"),
        }
    }

    /// A node-bound morph track is not sampled as a weight for every morph mesh (grok27
    /// diff-core #4): the compressed clip splits its tracks like `AnimationClip` does.
    #[test]
    fn compressed_morphs_keep_node_tracks_separate() {
        let mut clip = AnimationClip::empty("face", 1.0);
        clip.morph_tracks
            .push(MorphTrack::new(1, TrackCurve::Static(1.0)));
        clip.morph_tracks.push(MorphTrack::for_node(
            MorphNode {
                index: 7,
                name: "head".into(),
            },
            2,
            TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![0.0, 1.0],
                exact_rotation: false,
            },
        ));
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        let mut plain = Vec::new();
        compressed.sample_morphs(0.5, &mut plain);
        assert_eq!(plain, vec![(1, 1.0)]);
        let mut reference = Vec::new();
        clip.sample_morphs(0.5, &mut reference);
        assert_eq!(plain, reference);
        let mut nodes = Vec::new();
        compressed.sample_node_morphs(0.5, &mut nodes);
        assert_eq!(nodes.len(), 1);
        assert_eq!((nodes[0].0, nodes[0].1), (7, 2));
        assert!((nodes[0].2 - 0.5).abs() < 1e-3, "{nodes:?}");
    }

    #[test]
    fn notifies_survive_compression() {
        let mut clip = AnimationClip::empty("c", 1.0);
        clip.notifies.push(Notify {
            time: 0.5,
            name: "step".into(),
            payload: None,
        });
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        assert_eq!(compressed.notifies.len(), 1);
    }

    fn chain_skeleton(n: usize, segment: f32) -> Skeleton {
        let names: Vec<String> = (0..n).map(|i| format!("j{i}")).collect();
        let parents: Vec<Option<u32>> = (0..n).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
        let bind: Vec<_> = (0..n)
            .map(|i| crate::pose::translated(Vec3::new(0.0, if i == 0 { 0.0 } else { segment }, 0.0)))
            .collect();
        Skeleton::from_local_bind(names, parents, bind).expect("valid")
    }

    #[test]
    fn measured_error_respects_the_tolerances() {
        let clip = dense_clip();
        let settings = CompressionSettings::default();
        let skeleton = chain_skeleton(20, 0.1);
        let compressed = compress_clip(&clip, Some(&skeleton), &settings);
        let report = measure_compression_error(&clip, &compressed, Some(&skeleton));
        assert!(report.samples > 200, "{report:?}");
        assert!(report.within(&settings, 5e-5), "{report:?}");
        assert!(report.worst_joint.is_some());
        // Object space accumulates every ancestor's error. Per joint the cook bounds its own
        // translation error by min(position, propagated) and its rotation's reach by the
        // propagated tolerance, so a joint at depth d is off by at most (d + 1) times their
        // sum.
        let per_joint =
            settings.position_tolerance.min(settings.propagated_tolerance) + settings.propagated_tolerance;
        assert!(
            report.max_object_position <= 20.0 * per_joint,
            "{} m over a 20-joint chain",
            report.max_object_position
        );
    }

    fn stray_track(joint: u32) -> JointTrack {
        JointTrack {
            joint,
            translations: TrackCurve::Static(Vec3::X),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        }
    }

    /// Audit (corr-animation): a track past the skeleton (it cannot play) made the measure drop
    /// the skeleton and report no object-space error at all.
    #[test]
    fn a_track_past_the_skeleton_does_not_hide_the_object_space_error() {
        let settings = CompressionSettings::default();
        let skeleton = chain_skeleton(20, 0.1);
        let clip = dense_clip();
        let clean = measure_compression_error(
            &clip,
            &compress_clip(&clip, Some(&skeleton), &settings),
            Some(&skeleton),
        );
        assert!(clean.max_object_position > 0.0, "{clean:?}");
        let mut stray = clip.clone();
        stray.tracks.push(stray_track(1000));
        let report = measure_compression_error(
            &stray,
            &compress_clip(&stray, Some(&skeleton), &settings),
            Some(&skeleton),
        );
        assert_eq!(report, clean);
    }

    /// Audit (corr-animation): the pose was sized from the largest joint id, so a corrupt id of
    /// `u32::MAX` asked for hundreds of GB and aborted the agent's measure.
    #[test]
    fn a_corrupt_joint_id_is_skipped_without_sizing_the_pose_from_it() {
        let settings = CompressionSettings::default();
        let mut clip = dense_clip();
        clip.tracks.push(stray_track(u32::MAX));
        let compressed = compress_clip(&clip, None, &settings);
        let report = measure_compression_error(&clip, &compressed, None);
        assert!(report.samples > 0);
        assert!(report.within(&settings, 5e-5), "{report:?}");
    }

    /// A root translation track spanning `range` metres, keyed at 60 Hz over 4 s, curving so
    /// key reduction works up to its tolerance.
    fn long_track_clip(range: f32) -> AnimationClip {
        let mut clip = AnimationClip::empty("long", 4.0);
        let times: Vec<f32> = (0..=240).map(|k| k as f32 / 60.0).collect();
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                values: times
                    .iter()
                    .map(|t| {
                        let u = t / 4.0;
                        Vec3::new(range * u, 0.0, 0.5 * (u * std::f32::consts::TAU * 3.0).sin())
                    })
                    .collect(),
                times,
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip
    }

    #[test]
    fn quantisation_is_part_of_the_translation_budget() {
        // 200 m of travel: a 16-bit step is 3 mm, so the old cook (reduce to 5 mm, then
        // quantise) could end ~1.5 mm past the tolerance. The budget now leaves room for it.
        let settings = CompressionSettings::default();
        let clip = long_track_clip(200.0);
        let compressed = compress_clip(&clip, None, &settings);
        let report = measure_compression_error(&clip, &compressed, None);
        assert!(
            report.max_position <= settings.position_tolerance + 1e-4,
            "{report:?}"
        );
        assert!(matches!(
            compressed.tracks[0].translations,
            CompressedVec3Track::Animated { ref times, .. } if times.len() < 241
        ));
    }

    #[test]
    fn a_track_too_long_for_16_bits_is_reported_not_hidden() {
        // 2 km at 5 mm: half a 16-bit step alone is 15 mm. Every key is kept, and the report
        // shows the overshoot instead of pretending the tolerance held.
        let settings = CompressionSettings::default();
        let clip = long_track_clip(2000.0);
        let compressed = compress_clip(&clip, None, &settings);
        let report = measure_compression_error(&clip, &compressed, None);
        assert!(report.max_position.is_finite());
        assert!(report.max_position > settings.position_tolerance, "{report:?}");
        assert!(!report.within(&settings, 5e-5));
        assert!(vec3_quantization_error(&[Vec3::ZERO, Vec3::X * 2000.0]) > settings.position_tolerance);
    }

    /// Key reduction judges the reconstructed chord against the full tolerance (ADR-1387).
    /// An interior error that sits between `tolerance - half a 16-bit step` and `tolerance`
    /// loses that key. A rotation just under `rotation_tolerance` and just over
    /// `rotation_tolerance - QUAT_QUANTIZATION_ERROR` loses its interior key the same way;
    /// constant-collapse is the path that still subtracts the smallest-three bound.
    #[test]
    fn key_reduction_uses_the_full_tolerance_on_the_reconstructed_chord() {
        let tolerance = 0.005f32;
        let settings = CompressionSettings {
            position_tolerance: tolerance,
            rotation_tolerance: tolerance,
            scale_tolerance: tolerance,
            propagated_tolerance: 1.0,
        };
        let values = [
            Vec3::ZERO,
            Vec3::new(100.0, 0.004, 0.0),
            Vec3::new(200.0, 0.0, 0.0),
        ];
        let half_step = vec3_quantization_error(&values);
        let interior = 0.004f32;
        assert!(
            interior > tolerance - half_step && interior <= tolerance,
            "half step {half_step} must leave {interior} inside the old subtracted budget and inside the full tolerance"
        );
        let mut clip = AnimationClip::empty("span", 2.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0, 2.0],
                values: values.to_vec(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let compressed = compress_clip(&clip, None, &settings);
        match &compressed.tracks[0].translations {
            CompressedVec3Track::Animated { keys, .. } => {
                assert_eq!(
                    keys.len(),
                    2,
                    "the 4 mm interior sample is inside the full tolerance"
                );
            }
            other => panic!("expected a reduced animated translation, got {other:?}"),
        }

        let angle = 0.00495f32;
        assert!(angle > tolerance - QUAT_QUANTIZATION_ERROR && angle <= tolerance);
        let mut spinning = AnimationClip::empty("spin", 1.0);
        spinning.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 0.5, 1.0],
                values: vec![Quat::IDENTITY, Quat::from_rotation_y(angle), Quat::IDENTITY],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let compressed = compress_clip(&spinning, None, &settings);
        match &compressed.tracks[0].rotations {
            CompressedQuatTrack::Animated { keys, .. } => {
                assert_eq!(
                    keys.len(),
                    2,
                    "the interior rotation is under the full tolerance and over the constant-collapse reserve"
                );
            }
            other => panic!("expected a reduced animated rotation, got {other:?}"),
        }
    }

    #[test]
    fn compressed_morph_sampling_leaves_out_per_node_tracks() {
        let mut clip = AnimationClip::empty("m", 1.0);
        clip.morph_tracks
            .push(MorphTrack::new(0, TrackCurve::Static(0.5)));
        clip.morph_tracks.push(MorphTrack::for_node(
            MorphNode {
                index: 2,
                name: "head".into(),
            },
            1,
            TrackCurve::Static(1.0),
        ));
        let compressed = compress_clip(&clip, None, &CompressionSettings::default());
        let mut out = Vec::new();
        compressed.sample_morphs(0.5, &mut out);
        // Like `AnimationClip::sample_morphs`: the head's target 1 is not applied to every mesh.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 0);
        assert!((out[0].1 - 0.5).abs() < 1e-4);
    }

    #[test]
    fn rotation_error_resolves_tiny_angles() {
        let a = Quat::from_rotation_y(0.3);
        for angle in [1e-5f32, 1e-4, 1e-3, 0.5] {
            let b = a * Quat::from_rotation_x(angle);
            let e = rotation_error(a, b);
            assert!((e - angle).abs() < angle * 0.02 + 5e-7, "{angle}: {e}");
            // Sign of the quaternion does not matter.
            assert!((rotation_error(a, -b) - e).abs() < 1e-7);
        }
    }

    #[test]
    fn a_fit_with_every_sample_a_knot_returns_the_samples() {
        let times: Vec<f32> = (0..9).map(|k| k as f32 * 0.25).collect();
        let values: Vec<Vec3> = times.iter().map(|t| Vec3::new(t.sin(), t * t, -t)).collect();
        let knots: Vec<usize> = (0..9).collect();
        let fitted = fit_knot_values(&times, &values, &knots);
        for (f, v) in fitted.iter().zip(&values) {
            assert!(f.abs_diff_eq(*v, 1e-5), "{f:?} vs {v:?}");
        }
    }

    #[test]
    fn a_fitted_knot_centres_the_chord_on_an_arc() {
        // Two periods of a sine, 401 samples, a knot every 20: each interpolating chord errs by
        // its whole sagitta on the inside of the arc. The fitted knots move outwards so the
        // error splits both ways (about two thirds of the sagitta), and the end knots keep
        // their samples.
        let times: Vec<f32> = (0..=400)
            .map(|k| k as f32 * std::f32::consts::TAU / 200.0)
            .collect();
        let values: Vec<Vec3> = times.iter().map(|t| Vec3::new(0.0, t.sin(), 0.0)).collect();
        let knots: Vec<usize> = (0..=20).map(|k| k * 20).collect();
        let fitted = fit_knot_values(&times, &values, &knots);
        assert_eq!(fitted[0], values[0]);
        assert_eq!(fitted[20], values[400]);
        let worst = |keys: &dyn Fn(usize) -> Vec3| {
            (0..20)
                .map(|s| vec3_span_error(&times, &values, keys(s), keys(s + 1), knots[s], knots[s + 1]).0)
                .fold(0.0f32, f32::max)
        };
        let interpolating = worst(&|s| values[knots[s]]);
        let least_squares = worst(&|s| fitted[s]);
        assert!(interpolating > 0.04, "{interpolating}");
        assert!(
            least_squares < 0.8 * interpolating,
            "{least_squares} vs {interpolating}"
        );
    }

    #[test]
    fn fitted_tracks_keep_their_end_keys_and_beat_interpolation_on_the_dense_clip() {
        // Loops depend on the first and last stored keys being the clip's own end samples.
        let clip = dense_clip();
        let settings = CompressionSettings::default();
        let compressed = compress_clip(&clip, None, &settings);
        for (track, source) in compressed.tracks.iter().zip(&clip.tracks) {
            if let (
                CompressedVec3Track::Animated { keys, min, max, .. },
                TrackCurve::Animated { values, .. },
            ) = (&track.translations, &source.translations)
            {
                let (first, last) = (keys[0], keys[keys.len() - 1]);
                let step = vec3_quantization_error(&[*min, *max]);
                assert!((dequantize_vec3(first, *min, *max) - values[0]).length() <= step + 1e-6);
                assert!(
                    (dequantize_vec3(last, *min, *max) - values[values.len() - 1]).length() <= step + 1e-6
                );
            }
            if let (CompressedQuatTrack::Animated { keys, .. }, TrackCurve::Animated { values, .. }) =
                (&track.rotations, &source.rotations)
            {
                assert!(rotation_error(unpack_quat(keys[0]), values[0]) <= QUAT_QUANTIZATION_ERROR);
                let (last_key, last_value) = (keys[keys.len() - 1], values[values.len() - 1]);
                assert!(rotation_error(unpack_quat(last_key), last_value) <= QUAT_QUANTIZATION_ERROR);
            }
        }
        // The fit never keeps more keys than the interpolating pass on any track.
        let interpolating_keys = |curve: &TrackCurve<Vec3>| match curve {
            TrackCurve::Animated { times, values, .. } => {
                let (quantized, min, max) = quantize_vec3(values);
                let recon: Vec<Vec3> = quantized.iter().map(|&k| dequantize_vec3(k, min, max)).collect();
                reduce_keys(times.len(), |a, b| {
                    vec3_span_error(times, values, recon[a], recon[b], a, b).0 <= settings.position_tolerance
                })
                .len()
            }
            TrackCurve::Static(_) => 1,
        };
        let mut fewer = 0;
        for (track, source) in compressed.tracks.iter().zip(&clip.tracks) {
            if let CompressedVec3Track::Animated { times, .. } = &track.translations {
                let before = interpolating_keys(&source.translations);
                assert!(
                    times.len() <= before,
                    "joint {}: {} > {before}",
                    track.joint,
                    times.len()
                );
                fewer += usize::from(times.len() < before);
            }
        }
        assert!(fewer > 0, "the fit should beat interpolation on curved tracks");
    }

    #[test]
    fn quantisation_error_bound_holds() {
        // Sweep rotations, including the near-tie case where three components are equal.
        let mut worst = 0.0f32;
        for i in 0..2000 {
            let t = i as f32 * 0.0137;
            let q = Quat::from_xyzw(t.sin(), (t * 1.7).cos(), (t * 2.3).sin(), (t * 0.9).cos()).normalize();
            worst = worst.max(rotation_error(q, unpack_quat(pack_quat(q))));
        }
        let tie = Quat::from_xyzw(0.5, 0.5, 0.5, 0.5);
        worst = worst.max(rotation_error(tie, unpack_quat(pack_quat(tie))));
        assert!(worst <= QUAT_QUANTIZATION_ERROR, "{worst}");
    }

    #[test]
    fn zero_quaternion_packs_as_identity() {
        let back = unpack_quat(pack_quat(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)));
        assert!(back.is_finite());
        assert!(back.abs_diff_eq(Quat::IDENTITY, 1.0e-3), "got {back:?}");
    }
}
