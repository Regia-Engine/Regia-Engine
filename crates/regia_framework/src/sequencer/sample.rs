// SPDX-License-Identifier: MPL-2.0
//! Sampling a [`Sequence`] at a time: a function from `(sequence, seconds)` to the value of
//! every enabled track, plus the markers a step of playback crossed.
//!
//! No clocks and no randomness: the same sequence and the same time give the same
//! [`SequenceFrame`] on every call, machine and frame rate, like `ShotSequence::sample`
//! (ADR-268). [`Sequence::duration`] remembers its result in an atomic cache that is not part of
//! the frame or of `.seq.ron`; a cache hit returns the same length the key fold would. A player
//! advances a clock and feeds it here; applying the values to whatever the tracks' `target`s
//! resolve to is the host's (the engine's is [`super::SequencePlayer`]).
//!
//! Version 2 (ADR-1880) adds three section channels. [`Sequence::sample_with`] follows
//! sub-sequences through a [`SequenceLibrary`] and reports, besides the keyed values, the clip
//! blends ([`ClipSample`]) and the sounds ([`SoundSample`]) of the instant;
//! [`Sequence::flat_markers`] lists every marker of the tree on this sequence's clock.
//!
//! ```
//! use regia_framework::sequencer::Sequence;
//!
//! let sequence = Sequence::from_ron(r#"(
//!     version: 2,
//!     tracks: [(name: "fade", target: "ui", property: "opacity",
//!         keys: Float([(time: 0.0, value: 0.0), (time: 2.0, value: 1.0)]))],
//!     markers: [(name: "half", time: 1.0)],
//! )"#).expect("valid");
//! let frame = sequence.sample(1.0);
//! assert!(!frame.finished);
//! assert_eq!(frame.values.len(), 1);
//! assert_eq!(frame.values[0].value, regia_framework::sequencer::ChannelValue::Float(0.5));
//! assert_eq!(sequence.markers_between(0.0, 1.0).len(), 1, "the marker at 1.0 is included");
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::{Resource, bevy_ecs};

use super::asset::{
    AudioSection, AudioSpatial, BoolKey, Channel, ClipSection, DEFAULT_WEIGHT, Ease, MAX_LOOP_DEPTH,
    MAX_SUB_DEPTH, MAX_TREE_SECTIONS, MAX_TREE_SEQUENCES, MIN_LOOP_LENGTH, Marker, ScalarKey, Sequence,
    SequenceError, SoundSource, SubRepeat, SubSection, Tangent, TimeScaleMode, Vec3Key, VideoTarget,
};

/// Most markers [`Sequence::flat_markers`] lists: looping sub-sequences repeat their markers once
/// per pass, so a long looping section of a short sub-sequence is cut off here.
pub const MAX_FLAT_MARKERS: usize = 65_536;

/// Most loop passes one [`Sequence::flat_markers`] walk visits, kept or not: a looping section
/// nested in a short window of an outer one may have far more passes than the window shows, and
/// the walk stops here instead of visiting them all.
const MARKER_STEPS: usize = 1 << 20;

/// The value of one track's channel at an instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChannelValue {
    /// A scalar.
    Float(f32),
    /// A vec3 (position, color, Euler triple — whatever the property means).
    Vec3([f32; 3]),
    /// A bool.
    Bool(bool),
}

/// One track's sampled value: what to write, where.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackSample<'a> {
    /// Index of the track in the [`Sequence::tracks`] of the sequence that holds it (this one at
    /// `depth` 0, a sub-sequence below).
    pub track: usize,
    /// The track's name.
    pub name: &'a str,
    /// The track's binding name (see [`target`](super::asset::Track::target)), after the renames
    /// of the sub-sequence sections above it.
    pub target: &'a str,
    /// The track's property path (see [`property`](super::asset::Track::property)).
    pub property: &'a str,
    /// The sampled value.
    pub value: ChannelValue,
    /// How many sub-sequence sections deep the track sits (0: this sequence's own track).
    pub depth: usize,
}

/// One clip section's share of a [`ClipSample`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipWeight<'a> {
    /// Index of the section in its track.
    pub section: usize,
    /// The clip's library name.
    pub clip: &'a str,
    /// Clip time, seconds (not wrapped: the player wraps it over the clip's length when
    /// `looping`).
    pub clip_time: f64,
    /// Wrap the clip time over the clip's length.
    pub looping: bool,
    /// The section's weight at this instant, `0..=1`.
    pub weight: f32,
}

/// What one clip track plays at an instant: the sections in effect and their weights.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipSample<'a> {
    /// Index of the track in the sequence that holds it.
    pub track: usize,
    /// The track's name.
    pub name: &'a str,
    /// The rig's binding name, after sub-sequence renames.
    pub target: &'a str,
    /// The sections in effect, in section order, each with a weight above zero.
    pub blends: Vec<ClipWeight<'a>>,
    /// Sub-sequence depth (0: this sequence's own track).
    pub depth: usize,
}

impl ClipSample<'_> {
    /// The summed weight of the sections. Below one, the rig's own pose shows through by the
    /// rest; above one, the sections are normalized.
    #[must_use]
    pub fn total_weight(&self) -> f32 {
        self.blends.iter().map(|b| b.weight).sum()
    }
}

/// One sound section in effect at an instant.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundSample<'a> {
    /// The section's place in the tree: `[track, section]` for this sequence's own tracks, then a
    /// `[track, section]` pair per sub-sequence level. Stable from frame to frame, so a player
    /// keeps one voice per path.
    pub path: Vec<usize>,
    /// The audio track's name.
    pub track: &'a str,
    /// The sound's library name.
    pub sound: &'a str,
    /// Seconds into the sound.
    pub position: f64,
    /// Sound seconds per sequence second: the section's `time_scale` times the speeds of the
    /// sub-sequence sections above it (v3; 1 on a v2 section of this sequence's own tracks). The
    /// player plays the sound at this rate by varispeed: its pitch ratio is this rate (ADR-2405).
    pub rate: f64,
    /// Volume, dB.
    pub volume_db: f32,
    /// The fade gain at this instant, `0..=1` (fade in and fade out).
    pub fade: f32,
    /// Sequence seconds left until the section ends (on this sequence's clock).
    pub remaining: f64,
    /// Bus (`None`: `Master`).
    pub bus: Option<&'a str>,
    /// The section's 3D placement (v3; `None`: a flat sound).
    pub spatial: Option<&'a AudioSpatial>,
    /// The binding name a [`SoundSource::Binding`] placement emits from, after the renames of the
    /// sub-sequence sections above it (`None` for a flat sound or a fixed position).
    pub emitter: Option<&'a str>,
    /// How the voice follows [`rate`](Self::rate) (v5, ADR-3197): varispeed, or pitch-preserving
    /// through a stretch rendered ahead.
    pub time_scale_mode: TimeScaleMode,
    /// Seconds into the sound at the section's start (its `offset`): with
    /// [`section_source_secs`](Self::section_source_secs), the range of the sound the section
    /// plays, which a pitch-preserving section stretches as one piece (v5).
    pub section_offset: f64,
    /// Seconds of the sound the section plays from start to end, `(end - start) × time_scale`
    /// (v5).
    pub section_source_secs: f64,
}

/// Everything a sequence says about one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct SequenceFrame<'a> {
    /// The (clamped) sequence time this frame is for, seconds.
    pub time: f64,
    /// `true` once the time has reached the end of the sequence (a zero-length sequence is
    /// always finished).
    pub finished: bool,
    /// The value of every enabled keyed track, in track order (a sub-sequence's tracks where its
    /// track sits). Two tracks naming the same target and property both appear; the later one
    /// wins when applied.
    pub values: Vec<TrackSample<'a>>,
    /// Every enabled clip track with a section in effect.
    pub clips: Vec<ClipSample<'a>>,
    /// Every sound section in effect.
    pub sounds: Vec<SoundSample<'a>>,
    /// Every video section in effect (v4).
    pub videos: Vec<VideoSample<'a>>,
}

/// One video section in effect at an instant (v4).
#[derive(Clone, Debug, PartialEq)]
pub struct VideoSample<'a> {
    /// The section's place in the tree: a `[track, section]` pair per level.
    pub path: Vec<usize>,
    /// The name of the track it is on.
    pub track: &'a str,
    /// The source's project path.
    pub video: &'a str,
    /// Where the picture goes.
    pub target: &'a VideoTarget,
    /// The target's binding name, after the renames of the sub-sequence sections above it (`None`
    /// for a fullscreen layer).
    pub binding: Option<&'a str>,
    /// Seconds into the source (not wrapped, not clamped to its length: the player knows it).
    pub position: f64,
    /// Source seconds per sequence second: the speeds of the sub-sequence sections above it.
    pub rate: f64,
    /// Volume of the source's own audio, dB.
    pub volume_db: f32,
    /// Wrap over the source's length.
    pub looping: bool,
}

/// One sub-sequence section in effect at an instant and the pass it plays (v3,
/// [`Sequence::sub_passes`]).
#[derive(Clone, Debug, PartialEq)]
pub struct SubPass<'a> {
    /// The section's place in the tree: a `[track, section]` pair per level.
    pub path: Vec<usize>,
    /// The sub-sequence track's name.
    pub track: &'a str,
    /// The sub-sequence's library name.
    pub sequence: &'a str,
    /// The section's repeat.
    pub repeat: SubRepeat,
    /// Sub-sequence seconds per pass when the section loops (`None`: it plays once).
    pub pass_length: Option<f64>,
    /// The pass in effect, from 0.
    pub pass: u32,
    /// The sub-sequence time this instant samples, seconds.
    pub local_time: f64,
    /// Sub-sequence depth of the section (1: on this sequence's own track).
    pub depth: usize,
}

/// A marker on the clock of the sequence that holds it, possibly through sub-sequences
/// ([`Sequence::flat_markers`]).
#[derive(Clone, Debug, PartialEq)]
pub struct FlatMarker {
    /// Index in the flattened list (this sequence's own markers first, in declaration order,
    /// with their [`Sequence::markers`] index).
    pub index: usize,
    /// The name: the marker's own name for this sequence's markers, `track/name` for a marker of
    /// a sub-sequence on track `track` (nested: `outer/inner/name`).
    pub name: String,
    /// The time on this sequence's clock, seconds.
    pub time: f64,
    /// The payload.
    pub payload: String,
}

/// The sub-sequences a sequence's [`Channel::Sub`] tracks name, by name (usually the project
/// path). A resource: the player looks names up here. Each insert bumps the
/// [`revision`](Self::revision), so players pick changes up.
#[derive(Resource, Clone, Debug, Default)]
pub struct SequenceLibrary {
    sequences: BTreeMap<String, Arc<Sequence>>,
    revision: u64,
}

impl SequenceLibrary {
    /// Adds or replaces a sequence under `name`.
    pub fn insert(&mut self, name: impl Into<String>, sequence: Sequence) {
        self.sequences.insert(name.into(), Arc::new(sequence));
        self.revision += 1;
    }

    /// Removes a sequence; `true` when it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        let removed = self.sequences.remove(name).is_some();
        if removed {
            self.revision += 1;
        }
        removed
    }

    /// The sequence under `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Sequence> {
        self.sequences.get(name).map(Arc::as_ref)
    }

    /// The names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sequences.keys().map(String::as_str)
    }

    /// How many sequences it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sequences.len()
    }

    /// `true` when it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sequences.is_empty()
    }

    /// Counts inserts and removals.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Reads the sub-sequence tree under `root`: every sequence its enabled sub-sequence tracks
    /// name, followed down, each once, as `(name, sequence)` for [`insert`](Self::insert). `read`
    /// fetches a sequence's `.seq.ron` text by the name its sections use (a project path, usually)
    /// and its errors pass through; `invalid` turns the message of a sequence that does not parse
    /// or a tree [`check`](Self::check) refuses into the caller's error.
    pub fn read_tree<E>(
        root: &Sequence,
        mut read: impl FnMut(&str) -> Result<String, E>,
        invalid: impl Fn(String) -> E,
    ) -> Result<Vec<(String, Sequence)>, E> {
        let mut loaded: BTreeMap<String, Sequence> = BTreeMap::new();
        let mut pending: Vec<String> = root.sub_sequence_names().iter().map(|n| n.to_string()).collect();
        while let Some(name) = pending.pop() {
            if loaded.contains_key(&name) {
                continue;
            }
            if loaded.len() >= MAX_TREE_SEQUENCES {
                return Err(invalid(format!(
                    "the sub-sequence tree holds more than {MAX_TREE_SEQUENCES} sequences"
                )));
            }
            let sub = Sequence::from_ron(&read(&name)?)
                .map_err(|e| invalid(format!("sub-sequence `{name}`: {e}")))?;
            pending.extend(sub.sub_sequence_names().iter().map(|n| n.to_string()));
            loaded.insert(name, sub);
        }
        let mut library = Self::default();
        for (name, sub) in &loaded {
            library.insert(name.clone(), sub.clone());
        }
        library.check(root).map_err(|e| invalid(e.to_string()))?;
        Ok(loaded.into_iter().collect())
    }

    /// Checks the sub-sequence tree under `root`: every named sub-sequence is here, none
    /// contains itself (directly or further down), the tree is at most [`MAX_SUB_DEPTH`] deep and
    /// expands to at most [`MAX_TREE_SECTIONS`] sections. The first problem is the error. Each
    /// sequence is visited once, so the cost does not follow the size of the expanded tree.
    ///
    /// Looping sub-sequence sections ([`SubRepeat`], v3) may nest at most [`MAX_LOOP_DEPTH`] deep:
    /// a deeper one is [`SequenceError::LoopTooDeep`] (agent code `seq_loop_too_deep`).
    pub fn check(&self, root: &Sequence) -> Result<(), SequenceError> {
        let (_, sections, _) = self.check_level(root, &mut Vec::new(), 0, &mut BTreeMap::new())?;
        if sections > MAX_TREE_SECTIONS {
            return Err(SequenceError::Invalid(format!(
                "the sub-sequence tree expands to more than {MAX_TREE_SECTIONS} sections"
            )));
        }
        Ok(())
    }

    /// The levels below `sequence`, the sections it expands to and the looping levels below it;
    /// `seen` keeps all three per name. `loops` counts the looping sections above `sequence`.
    fn check_level<'a>(
        &'a self,
        sequence: &'a Sequence,
        stack: &mut Vec<&'a str>,
        loops: usize,
        seen: &mut BTreeMap<&'a str, (usize, usize, usize)>,
    ) -> Result<(usize, usize, usize), SequenceError> {
        let (mut height, mut expanded, mut loop_height) = (0usize, 0usize, 0usize);
        for track in sequence.tracks.iter().filter(|t| t.enabled) {
            let Channel::Sub(sections) = &track.keys else {
                continue;
            };
            let too_deep = || {
                SequenceError::Invalid(format!(
                    "track `{}`: sub-sequences nest deeper than {MAX_SUB_DEPTH} levels",
                    track.name
                ))
            };
            for section in sections {
                let name = section.sequence.as_str();
                let looping = usize::from(section.repeat.loops());
                if stack.contains(&name) {
                    return Err(SequenceError::Invalid(format!(
                        "track `{}`: sub-sequence `{name}` contains itself ({} -> {name})",
                        track.name,
                        stack.join(" -> ")
                    )));
                }
                let below = match seen.get(name) {
                    Some(&below) => below,
                    None => {
                        if stack.len() >= MAX_SUB_DEPTH {
                            return Err(too_deep());
                        }
                        let Some(sub) = self.get(name) else {
                            return Err(SequenceError::Invalid(format!(
                                "track `{}`: no sub-sequence `{name}` in the library",
                                track.name
                            )));
                        };
                        stack.push(name);
                        let below = self.check_level(sub, stack, loops + looping, seen)?;
                        stack.pop();
                        seen.insert(name, below);
                        below
                    }
                };
                if stack.len() + 1 + below.0 > MAX_SUB_DEPTH {
                    return Err(too_deep());
                }
                let depth = loops + looping + below.2;
                if depth > MAX_LOOP_DEPTH {
                    return Err(SequenceError::LoopTooDeep {
                        track: track.name.clone(),
                        depth,
                    });
                }
                height = height.max(1 + below.0);
                expanded = expanded.saturating_add(1).saturating_add(below.1);
                loop_height = loop_height.max(looping + below.2);
            }
        }
        Ok((height, expanded, loop_height))
    }
}

/// Read access to the keys of a float or vec3 channel, one component at a time (the Bezier
/// code is written once for both).
trait CurveKeys {
    fn count(&self) -> usize;
    fn time(&self, i: usize) -> f32;
    fn value(&self, i: usize, c: usize) -> f32;
    fn ease(&self, i: usize) -> Ease;
    fn tangent(&self, i: usize, c: usize) -> Tangent<f32>;
    fn weights(&self, i: usize) -> (f32, f32);
}

impl CurveKeys for [ScalarKey] {
    fn count(&self) -> usize {
        self.len()
    }
    fn time(&self, i: usize) -> f32 {
        self[i].time
    }
    fn value(&self, i: usize, _c: usize) -> f32 {
        self[i].value
    }
    fn ease(&self, i: usize) -> Ease {
        self[i].ease
    }
    fn tangent(&self, i: usize, _c: usize) -> Tangent<f32> {
        self[i].tangent
    }
    fn weights(&self, i: usize) -> (f32, f32) {
        self[i].weights.unwrap_or((DEFAULT_WEIGHT, DEFAULT_WEIGHT))
    }
}

impl CurveKeys for [Vec3Key] {
    fn count(&self) -> usize {
        self.len()
    }
    fn time(&self, i: usize) -> f32 {
        self[i].time
    }
    fn value(&self, i: usize, c: usize) -> f32 {
        self[i].value[c]
    }
    fn ease(&self, i: usize) -> Ease {
        self[i].ease
    }
    fn tangent(&self, i: usize, c: usize) -> Tangent<f32> {
        match self[i].tangent {
            Tangent::Auto => Tangent::Auto,
            Tangent::Flat => Tangent::Flat,
            Tangent::Linear => Tangent::Linear,
            Tangent::Aligned(s) => Tangent::Aligned(s[c]),
            Tangent::Broken(a, b) => Tangent::Broken(a[c], b[c]),
        }
    }
    fn weights(&self, i: usize) -> (f32, f32) {
        self[i].weights.unwrap_or((DEFAULT_WEIGHT, DEFAULT_WEIGHT))
    }
}

/// The secant slope from key `a` to key `b` of component `c`.
fn secant(keys: &(impl CurveKeys + ?Sized), a: usize, b: usize, c: usize) -> f32 {
    let dt = keys.time(b) - keys.time(a);
    if dt > 0.0 {
        (keys.value(b, c) - keys.value(a, c)) / dt
    } else {
        0.0
    }
}

/// The `(incoming, outgoing)` slopes of key `i`, component `c`, value units per second, with the
/// computed tangent modes resolved.
///
/// `Auto`: the slope through the two neighbors (the Catmull-Rom slope, nonuniform times), zero
/// at a local extreme or a flat neighbor, and at most three times the smaller neighboring
/// secant, which keeps a Hermite segment between monotone keys monotone (F. N. Fritsch and
/// R. E. Carlson, "Monotone Piecewise Cubic Interpolation", SIAM J. Numer. Anal. 17(2), 1980).
/// The first and the last key are flat.
fn resolved_slopes(keys: &(impl CurveKeys + ?Sized), i: usize, c: usize) -> (f32, f32) {
    let (incoming, outgoing) = raw_slopes(keys, i, c);
    (finite_or_max(incoming), finite_or_max(outgoing))
}

/// `x` itself when finite; the largest finite `f32` of its sign for an infinity, 0 for NaN.
/// Extreme keys (values near the `f32` range, keys a few subnormals apart) overflow a secant; a
/// result that was finite before this guard is unchanged bit for bit (ADR-2135).
fn finite_or_max(x: f32) -> f32 {
    if x.is_finite() {
        x
    } else if x.is_nan() {
        0.0
    } else {
        f32::MAX.copysign(x)
    }
}

/// [`resolved_slopes`] before the finite guard.
fn raw_slopes(keys: &(impl CurveKeys + ?Sized), i: usize, c: usize) -> (f32, f32) {
    let n = keys.count();
    match keys.tangent(i, c) {
        Tangent::Flat => (0.0, 0.0),
        Tangent::Aligned(s) => (s, s),
        Tangent::Broken(a, b) => (a, b),
        Tangent::Linear => {
            let incoming = if i > 0 { secant(keys, i - 1, i, c) } else { 0.0 };
            let outgoing = if i + 1 < n { secant(keys, i, i + 1, c) } else { 0.0 };
            match (i > 0, i + 1 < n) {
                (true, true) => (incoming, outgoing),
                (true, false) => (incoming, incoming),
                (false, true) => (outgoing, outgoing),
                (false, false) => (0.0, 0.0),
            }
        }
        Tangent::Auto => {
            if i == 0 || i + 1 >= n {
                return (0.0, 0.0);
            }
            let left = secant(keys, i - 1, i, c);
            let right = secant(keys, i, i + 1, c);
            if left == 0.0 || right == 0.0 || left.signum() != right.signum() {
                return (0.0, 0.0);
            }
            let through =
                (keys.value(i + 1, c) - keys.value(i - 1, c)) / (keys.time(i + 1) - keys.time(i - 1));
            let limit = 3.0 * left.abs().min(right.abs());
            let slope = through.clamp(-limit, limit);
            (slope, slope)
        }
    }
}

/// The cubic Bezier of `p0..p3` at `u`.
fn cubic(p0: f64, p1: f64, p2: f64, p3: f64, u: f64) -> f64 {
    let v = 1.0 - u;
    v * v * v * p0 + 3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u * p3
}

/// Bisection steps of the weighted Bezier time solve: 2^-48 of a segment, below `f32`
/// resolution, in a fixed number of steps (deterministic).
const SOLVE_STEPS: u32 = 48;

/// The Bezier segment from key `i` to key `i + 1`, component `c`, at `time` (inside the
/// segment). The handles are `weight × segment` long along the two slopes. With both weights
/// at [`DEFAULT_WEIGHT`] the time axis is linear in the curve parameter and the segment is the
/// cubic Hermite curve of the slopes; otherwise the parameter of `time` is found by bisection
/// (the time axis is monotone because the two weights are scaled to sum to at most one).
fn bezier(keys: &(impl CurveKeys + ?Sized), i: usize, c: usize, time: f32) -> f32 {
    let (ta, tb) = (f64::from(keys.time(i)), f64::from(keys.time(i + 1)));
    let (va, vb) = (f64::from(keys.value(i, c)), f64::from(keys.value(i + 1, c)));
    let dt = tb - ta;
    if dt <= 0.0 {
        return keys.value(i + 1, c);
    }
    let m0 = f64::from(resolved_slopes(keys, i, c).1);
    let m1 = f64::from(resolved_slopes(keys, i + 1, c).0);
    let mut wa = f64::from(keys.weights(i).1);
    let mut wb = f64::from(keys.weights(i + 1).0);
    let sum = wa + wb;
    if sum > 1.0 {
        wa /= sum;
        wb /= sum;
    }
    let x = ((f64::from(time) - ta) / dt).clamp(0.0, 1.0);
    let third = f64::from(DEFAULT_WEIGHT);
    let u = if wa == third && wb == third {
        x
    } else {
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..SOLVE_STEPS {
            let mid = 0.5 * (lo + hi);
            if cubic(0.0, wa, 1.0 - wb, 1.0, mid) < x {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    };
    let out = cubic(va, va + m0 * wa * dt, vb - m1 * wb * dt, vb, u);
    // Finite slopes, weights and times keep `out` finite in f64; an overshoot past the f32 range
    // holds at its edge instead of becoming infinite (ADR-2135). In range, this is the plain cast.
    if out.is_finite() {
        out.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
    } else {
        (va + (vb - va) * x) as f32
    }
}

/// Component `c` of a float or vec3 channel at `time` (see [`Channel::sample`]).
fn sample_curve(keys: &(impl CurveKeys + ?Sized), c: usize, time: f32) -> Option<f32> {
    let n = keys.count();
    if n == 0 {
        return None;
    }
    if time.is_nan() || time <= keys.time(0) {
        return Some(keys.value(0, c));
    }
    if time >= keys.time(n - 1) {
        return Some(keys.value(n - 1, c));
    }
    let mut lo = 0;
    let mut hi = n;
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if keys.time(mid) <= time {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let i = lo;
    if i + 1 >= n {
        return Some(keys.value(i, c));
    }
    let (a, b) = (keys.value(i, c), keys.value(i + 1, c));
    Some(match keys.ease(i) {
        Ease::Step => a,
        Ease::Bezier => bezier(keys, i, c, time),
        ease => {
            let u = ease_u(ease, segment_u(keys.time(i), keys.time(i + 1), time));
            let v = a + (b - a) * u;
            if v.is_finite() {
                v
            } else {
                // `b - a` overflowed f32 (keys of opposite sign near the range's edges): the same
                // blend in f64 lies between the two keys, so it is finite (ADR-2135).
                (f64::from(a) + (f64::from(b) - f64::from(a)) * f64::from(u)) as f32
            }
        }
    })
}

impl Channel {
    /// The channel's value at `time`. Holds the first value before the first key and the last
    /// value after the last; `ease` on a key shapes the segment that starts at it (`Step` holds
    /// that key's value, `Bezier` follows the two keys' tangents). An empty channel (validation
    /// rejects one) samples its kind's zero. Section channels (`Clip`, `Audio`, `Sub`) have no
    /// value and sample as `Float(0.0)`; [`Sequence::sample_with`] reports them.
    #[must_use]
    pub fn sample(&self, time: f32) -> ChannelValue {
        match self {
            Channel::Float(keys) => {
                ChannelValue::Float(sample_curve(keys.as_slice(), 0, time).unwrap_or(0.0))
            }
            Channel::Vec3(keys) => {
                let mut out = [0.0; 3];
                for (c, out) in out.iter_mut().enumerate() {
                    *out = sample_curve(keys.as_slice(), c, time).unwrap_or(0.0);
                }
                ChannelValue::Vec3(out)
            }
            Channel::Bool(keys) => ChannelValue::Bool(sample_bool(keys, time).unwrap_or(false)),
            Channel::Clip(_)
            | Channel::Audio(_)
            | Channel::Sub(_)
            | Channel::Video(_)
            | Channel::Event(_) => ChannelValue::Float(0.0),
        }
    }

    /// Component `component` (0 for a float, 0..3 for a vec3) of a float or vec3 channel at
    /// `time`: what a curve editor draws. `None` for other channels, an empty channel or a
    /// component out of range.
    #[must_use]
    pub fn sample_component(&self, time: f32, component: usize) -> Option<f32> {
        match self {
            Channel::Float(keys) if component == 0 => sample_curve(keys.as_slice(), 0, time),
            Channel::Vec3(keys) if component < 3 => sample_curve(keys.as_slice(), component, time),
            _ => None,
        }
    }

    /// The `(incoming, outgoing)` slopes of key `key`, component `component`, in value units per
    /// second, with `Auto` and `Linear` resolved: where a curve editor draws the handles. `None`
    /// for other channels or an index out of range.
    #[must_use]
    pub fn tangent_slopes(&self, key: usize, component: usize) -> Option<(f32, f32)> {
        match self {
            Channel::Float(keys) if component == 0 && key < keys.len() => {
                Some(resolved_slopes(keys.as_slice(), key, 0))
            }
            Channel::Vec3(keys) if component < 3 && key < keys.len() => {
                Some(resolved_slopes(keys.as_slice(), key, component))
            }
            _ => None,
        }
    }

    /// The latest key time or section end, `None` for an empty channel.
    #[must_use]
    pub fn end_time(&self) -> Option<f32> {
        #[cfg(test)]
        sample_counts::add_end();
        let latest = |times: &mut dyn Iterator<Item = f32>| {
            times.fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))))
        };
        match self {
            Channel::Float(keys) => latest(&mut keys.iter().map(|k| k.time)),
            Channel::Vec3(keys) => latest(&mut keys.iter().map(|k| k.time)),
            Channel::Bool(keys) => latest(&mut keys.iter().map(|k| k.time)),
            Channel::Clip(s) => latest(&mut s.iter().map(|s| s.end)),
            Channel::Audio(s) => latest(&mut s.iter().map(|s| s.end)),
            Channel::Sub(s) => latest(&mut s.iter().map(|s| s.end)),
            Channel::Video(s) => latest(&mut s.iter().map(|s| s.end)),
            Channel::Event(k) => latest(&mut k.iter().map(|k| k.time)),
        }
    }
}

/// The bool at `time`, `None` for an empty key list. The value of the last key at or before
/// `time` (bool keys always step).
fn sample_bool(keys: &[BoolKey], time: f32) -> Option<bool> {
    let (first, last) = (keys.first()?, keys.last()?);
    if time.is_nan() || time <= first.time {
        return Some(first.value);
    }
    if time >= last.time {
        return Some(last.value);
    }
    let i = keys.partition_point(|k| k.time <= time);
    Some(keys[i.saturating_sub(1)].value)
}

/// Linear progress from `a_time` to `b_time` at `time`, clamped to `0..=1`.
fn segment_u(a_time: f32, b_time: f32, time: f32) -> f32 {
    let span = b_time - a_time;
    if span > 0.0 {
        ((time - a_time) / span).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// `Ease::Smooth` is smoothstep (`3u² - 2u³`, zero slope at both ends); the others pass `u`
/// through.
fn ease_u(ease: Ease, u: f32) -> f32 {
    match ease {
        Ease::Smooth => u * u * (3.0 - 2.0 * u),
        Ease::Linear | Ease::Step | Ease::Bezier => u,
    }
}

/// Whether a section `[start, end)` of a track is in effect at `time`: from its start up to its
/// end, and at the end instant itself unless another section of the track starts there (so the
/// last pose holds at the end of a sequence, and back-to-back sections never both count).
fn in_effect(start: f32, end: f32, time: f64, mut starts: impl Iterator<Item = f32>) -> bool {
    let (start, end) = (f64::from(start), f64::from(end));
    if time >= start && time < end {
        return true;
    }
    time == end && !starts.any(|s| f64::from(s) == end)
}

/// A linear ramp: 0 at `from`, 1 after `length` seconds (1 throughout for a zero length).
fn ramp(elapsed: f64, length: f32) -> f32 {
    if length > 0.0 {
        (elapsed / f64::from(length)).clamp(0.0, 1.0) as f32
    } else {
        1.0
    }
}

impl ClipSection {
    /// The section's weight at sequence time `time` when it is in effect: `weight`, ramped up
    /// over `blend_in` from the start and down over `blend_out` to the end (linear ramps, so a
    /// blend out over a blend in of the same length sums to a constant).
    #[must_use]
    pub fn weight_at(&self, time: f64) -> f32 {
        let rise = ramp(time - f64::from(self.start), self.blend_in);
        let fall = ramp(f64::from(self.end) - time, self.blend_out);
        self.weight * rise.min(fall)
    }

    /// The clip time at sequence time `time`, seconds (not wrapped).
    #[must_use]
    pub fn clip_time(&self, time: f64) -> f64 {
        f64::from(self.offset) + (time - f64::from(self.start)) * f64::from(self.speed)
    }
}

impl AudioSection {
    /// The fade gain at sequence time `time`: rising over `fade_in`, falling over `fade_out`.
    #[must_use]
    pub fn fade_at(&self, time: f64) -> f32 {
        let rise = ramp(time - f64::from(self.start), self.fade_in);
        let fall = ramp(f64::from(self.end) - time, self.fade_out);
        rise.min(fall)
    }
}

impl SubSection {
    /// The sub-sequence time at sequence time `time`, seconds, ignoring [`repeat`](Self::repeat)
    /// (one pass, as in v2; see [`looped_time`](Self::looped_time)).
    #[must_use]
    pub fn local_time(&self, time: f64) -> f64 {
        f64::from(self.offset) + (time - f64::from(self.start)) * f64::from(self.speed)
    }

    /// The length of one pass in sub-sequence seconds when the section loops (v3): `loop_length`,
    /// or the sub-sequence's length after `offset` (`sub_duration - offset`). `None` when
    /// [`repeat`](Self::repeat) does not loop or that default is shorter than
    /// [`MIN_LOOP_LENGTH`] (the section then plays once).
    #[must_use]
    pub fn pass_length(&self, sub_duration: f64) -> Option<f64> {
        if !self.repeat.loops() {
            return None;
        }
        let length = match self.loop_length {
            Some(length) => f64::from(length),
            None => sub_duration - f64::from(self.offset),
        };
        (length.is_finite() && length >= f64::from(MIN_LOOP_LENGTH)).then_some(length)
    }

    /// The sub-sequence time and the pass (from 0) at sequence time `time`, for a sub-sequence
    /// `sub_duration` seconds long (v3). A looping section wraps its sub-sequence time over
    /// `[offset, offset + pass)` every [`pass_length`](Self::pass_length) sub-sequence seconds;
    /// after the last pass of a [`SubRepeat::Count`] the end of that pass (`offset + pass`) holds.
    /// The wrap is an exact remainder, so the time at `t` and at `t` plus one pass on this
    /// sequence's clock are equal whenever the clock values are exact (pure, deterministic).
    #[must_use]
    pub fn looped_time(&self, time: f64, sub_duration: f64) -> (f64, u32) {
        let offset = f64::from(self.offset);
        let elapsed = (time - f64::from(self.start)) * f64::from(self.speed);
        let Some(length) = self.pass_length(sub_duration) else {
            return (offset + elapsed, 0);
        };
        if elapsed.is_nan() || elapsed <= 0.0 {
            return (offset + elapsed, 0);
        }
        let within = elapsed % length;
        let pass = ((elapsed - within) / length).round();
        let passes = match self.repeat {
            SubRepeat::Count(n) => f64::from(n),
            SubRepeat::Once | SubRepeat::UntilStop => f64::INFINITY,
        };
        if pass >= passes {
            return (offset + length, (passes - 1.0) as u32);
        }
        (offset + within, pass.min(f64::from(u32::MAX)) as u32)
    }

    /// The sequence time of sub-sequence time `local`.
    #[must_use]
    pub fn parent_time(&self, local: f64) -> f64 {
        f64::from(self.start) + (local - f64::from(self.offset)) / f64::from(self.speed)
    }
}

/// How often sampling walks a channel's keys for its end, and how often a frame is sampled.
#[cfg(test)]
pub(crate) mod sample_counts {
    use std::cell::Cell;

    thread_local! {
        static ENDS: Cell<u64> = const { Cell::new(0) };
        static SAMPLES: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        ENDS.with(|c| c.set(0));
        SAMPLES.with(|c| c.set(0));
    }

    pub(crate) fn ends() -> u64 {
        ENDS.with(|c| c.get())
    }

    pub(crate) fn samples() -> u64 {
        SAMPLES.with(|c| c.get())
    }

    pub(crate) fn add_end() {
        ENDS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn add_sample() {
        SAMPLES.with(|c| c.set(c.get() + 1));
    }
}

/// One level of a sub-sequence walk: renames to apply (innermost first) and the path so far.
struct Level<'a> {
    renames: Vec<&'a BTreeMap<String, String>>,
    path: Vec<usize>,
    rate: f64,
    depth: usize,
    /// The sub-sequences this level sits inside: a section naming one of them is skipped.
    stack: Vec<&'a str>,
    /// Root seconds until a section above this level ends.
    remaining: f64,
    /// Looping sub-sequence sections above this level.
    loops: usize,
}

/// Receives one root-clock time of a marker; answers `false` once the walk must stop.
type MarkerSink<'s> = dyn FnMut(f64) -> bool + 's;

/// Hands every root-clock time of a time of one sequence to a sink (see `flat_sub_markers`).
type ToRoot<'s> = dyn Fn(f64, &mut MarkerSink<'_>) -> bool + 's;

/// The state of a [`Sequence::flat_markers`] walk: what is left of its budgets and where it
/// writes.
struct MarkerWalk<'a, 'o> {
    library: &'a SequenceLibrary,
    /// The sub-sequences the current one sits inside.
    stack: Vec<&'a str>,
    /// Sub-sequence sections left to expand ([`MAX_TREE_SECTIONS`]).
    sections: usize,
    /// Markers left to list ([`MAX_FLAT_MARKERS`]).
    markers: usize,
    out: &'o mut Vec<FlatMarker>,
}

/// The state of a [`Sequence::sub_passes`] walk.
struct PassWalk<'a> {
    library: &'a SequenceLibrary,
    stack: Vec<&'a str>,
    budget: usize,
    out: Vec<SubPass<'a>>,
}

/// Mixes one value into a duration stamp. Never returns the inputs unchanged in a way that
/// collapses an empty sequence onto stamp 0 (the cache's empty sentinel).
fn mix_stamp(hash: u64, value: u64) -> u64 {
    hash.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(value) ^ value.rotate_left(17)
}

/// O(tracks + sections + markers). Not a fold of every key: validated key times strictly
/// increase, so the last key time is the channel's end. A middle key's value does not change the
/// length. Sections are not ordered by `end` (they may overlap and are edited in place), so a
/// section channel stamps its latest end, which a middle section can hold.
fn duration_stamp(sequence: &Sequence) -> u64 {
    fn latest_end(ends: impl Iterator<Item = f32>) -> Option<f32> {
        ends.fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))))
    }
    fn pack(n: usize, first: Option<f32>, last: Option<f32>) -> (u64, u64, u64) {
        (
            n as u64,
            first.map(|t| u64::from(t.to_bits())).unwrap_or(0),
            last.map(|t| u64::from(t.to_bits())).unwrap_or(0),
        )
    }
    let mut hash = 0xD1B5_4A32_D192_ED03u64;
    hash = mix_stamp(hash, sequence.tracks.len() as u64);
    for track in &sequence.tracks {
        let (count, first, last) = match &track.keys {
            Channel::Float(keys) => pack(
                keys.len(),
                keys.first().map(|k| k.time),
                keys.last().map(|k| k.time),
            ),
            Channel::Vec3(keys) => pack(
                keys.len(),
                keys.first().map(|k| k.time),
                keys.last().map(|k| k.time),
            ),
            Channel::Bool(keys) => pack(
                keys.len(),
                keys.first().map(|k| k.time),
                keys.last().map(|k| k.time),
            ),
            Channel::Clip(sections) => pack(
                sections.len(),
                sections.first().map(|s| s.end),
                latest_end(sections.iter().map(|s| s.end)),
            ),
            Channel::Audio(sections) => pack(
                sections.len(),
                sections.first().map(|s| s.end),
                latest_end(sections.iter().map(|s| s.end)),
            ),
            Channel::Sub(sections) => pack(
                sections.len(),
                sections.first().map(|s| s.end),
                latest_end(sections.iter().map(|s| s.end)),
            ),
            Channel::Video(sections) => pack(
                sections.len(),
                sections.first().map(|s| s.end),
                sections.last().map(|s| s.end),
            ),
            Channel::Event(keys) => pack(
                keys.len(),
                keys.first().map(|k| k.time),
                keys.last().map(|k| k.time),
            ),
        };
        hash = mix_stamp(hash, count);
        hash = mix_stamp(hash, first);
        hash = mix_stamp(hash, last);
    }
    hash = mix_stamp(hash, sequence.markers.len() as u64);
    for marker in &sequence.markers {
        hash = mix_stamp(hash, u64::from(marker.time.to_bits()));
    }
    hash = mix_stamp(hash, sequence.spawnables.len() as u64);
    for spawnable in &sequence.spawnables {
        hash = mix_stamp(hash, u64::from(spawnable.start.to_bits()));
        hash = mix_stamp(
            hash,
            spawnable.end.map_or(u64::MAX, |end| u64::from(end.to_bits())),
        );
    }
    if hash == 0 { 1 } else { hash }
}

/// A target name through the renames of the levels above it (innermost first).
fn renamed<'a>(target: &'a str, renames: &[&'a BTreeMap<String, String>]) -> &'a str {
    let mut name = target;
    for map in renames.iter().rev() {
        if let Some(to) = map.get(name) {
            name = to.as_str();
        }
    }
    name
}

impl Sequence {
    /// Length in seconds: the latest key time, section end, marker time or spawnable start or end,
    /// zero when there is none. A sequence has no explicit end; its last key, section or marker is it.
    ///
    /// The result is cached. The stamp is the track count, each channel's key count and its
    /// first and last time, every marker time and every spawnable range: validated keys are strictly increasing, so
    /// the last key is the end, and editing a middle key's value does not change the length.
    /// A miss still folds every key, so an unsorted channel stays correct until its stamp changes.
    #[must_use]
    pub fn duration(&self) -> f64 {
        let stamp = duration_stamp(self);
        if let Some(cached) = self.duration_cache.load(stamp) {
            return cached;
        }
        let mut end = 0.0f64;
        for track in &self.tracks {
            if let Some(latest) = track.keys.end_time() {
                end = end.max(f64::from(latest));
            }
        }
        for marker in &self.markers {
            end = end.max(f64::from(marker.time));
        }
        // A spawnable's range is part of the timeline too (v5).
        for spawnable in &self.spawnables {
            end = end.max(f64::from(spawnable.end.unwrap_or(spawnable.start)));
        }
        let end = end.max(0.0);
        self.duration_cache.store(stamp, end);
        end
    }

    /// The value of every enabled keyed track at sequence time `time` seconds. Time is clamped
    /// to `0..=duration`; a non-finite time reads as 0; the end holds the last values. Disabled
    /// tracks do not appear. Clip and audio sections of this sequence are reported too;
    /// sub-sequences are not followed (see [`sample_with`](Self::sample_with)). Deterministic: the
    /// frame is a function of `(self, time)`. The duration cache does not change it.
    #[must_use]
    pub fn sample(&self, time: f64) -> SequenceFrame<'_> {
        self.sample_tree(time, None)
    }

    /// Like [`sample`](Self::sample), and follows every enabled sub-sequence track through
    /// `library`: a sub-sequence section in effect samples its sequence at its local time, and its
    /// keyed values, clips and sounds join this frame with their targets renamed. A sub-sequence
    /// missing from the library, nested deeper than [`MAX_SUB_DEPTH`], inside itself or past
    /// [`MAX_TREE_SECTIONS`] is skipped ([`SequenceLibrary::check`] reports it).
    #[must_use]
    pub fn sample_with<'a>(&'a self, time: f64, library: &'a SequenceLibrary) -> SequenceFrame<'a> {
        self.sample_tree(time, Some(library))
    }

    fn sample_tree<'a>(&'a self, time: f64, library: Option<&'a SequenceLibrary>) -> SequenceFrame<'a> {
        #[cfg(test)]
        sample_counts::add_sample();
        let total = self.duration();
        let time = if time.is_finite() {
            time.clamp(0.0, total)
        } else {
            0.0
        };
        let mut frame = SequenceFrame {
            time,
            finished: time >= total,
            values: Vec::with_capacity(self.tracks.len()),
            clips: Vec::new(),
            sounds: Vec::new(),
            videos: Vec::new(),
        };
        let mut level = Level {
            renames: Vec::new(),
            path: Vec::new(),
            rate: 1.0,
            depth: 0,
            stack: Vec::new(),
            remaining: f64::INFINITY,
            loops: 0,
        };
        let mut budget = MAX_TREE_SECTIONS;
        self.sample_level(time, library, &mut level, &mut budget, &mut frame);
        frame
    }

    fn sample_level<'a>(
        &'a self,
        time: f64,
        library: Option<&'a SequenceLibrary>,
        level: &mut Level<'a>,
        budget: &mut usize,
        frame: &mut SequenceFrame<'a>,
    ) {
        let local = time as f32;
        for (index, track) in self.tracks.iter().enumerate() {
            if !track.enabled {
                continue;
            }
            match &track.keys {
                Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) => frame.values.push(TrackSample {
                    track: index,
                    name: &track.name,
                    target: renamed(&track.target, &level.renames),
                    property: &track.property,
                    value: track.keys.sample(local),
                    depth: level.depth,
                }),
                Channel::Clip(sections) => {
                    let blends: Vec<ClipWeight<'a>> = sections
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| in_effect(s.start, s.end, time, sections.iter().map(|o| o.start)))
                        .map(|(i, s)| ClipWeight {
                            section: i,
                            clip: &s.clip,
                            clip_time: s.clip_time(time),
                            looping: s.looping,
                            weight: s.weight_at(time),
                        })
                        .filter(|b| b.weight > 0.0)
                        .collect();
                    if !blends.is_empty() {
                        frame.clips.push(ClipSample {
                            track: index,
                            name: &track.name,
                            target: renamed(&track.target, &level.renames),
                            blends,
                            depth: level.depth,
                        });
                    }
                }
                Channel::Audio(sections) => {
                    for (i, s) in sections.iter().enumerate() {
                        let (start, end) = (f64::from(s.start), f64::from(s.end));
                        if !(time >= start && time < end) {
                            continue;
                        }
                        let mut path = level.path.clone();
                        path.extend([index, i]);
                        let scale = f64::from(s.time_scale);
                        // `* 1.0` is exact: a v2 section's position is bit for bit what it was.
                        let position = f64::from(s.offset) + (time - start) * scale;
                        let emitter = match s.spatial.as_ref().map(|p| &p.source) {
                            Some(SoundSource::Binding(name)) => Some(renamed(name, &level.renames)),
                            _ => None,
                        };
                        frame.sounds.push(SoundSample {
                            path,
                            track: &track.name,
                            sound: &s.sound,
                            position,
                            rate: level.rate * scale,
                            volume_db: s.volume_db,
                            fade: s.fade_at(time),
                            remaining: ((end - time) / level.rate).min(level.remaining),
                            bus: s.bus.as_deref(),
                            spatial: s.spatial.as_ref(),
                            emitter,
                            time_scale_mode: s.time_scale_mode,
                            section_offset: f64::from(s.offset),
                            section_source_secs: (end - start) * scale,
                        });
                    }
                }
                Channel::Video(sections) => {
                    for (i, s) in sections.iter().enumerate() {
                        // An edit point is the next frame's: a section ends before its `end`, and
                        // only the end of the sequence holds its last picture there.
                        let (start, end) = (f64::from(s.start), f64::from(s.end));
                        let holds = frame.finished
                            && in_effect(s.start, s.end, time, sections.iter().map(|o| o.start));
                        if !(time >= start && time < end) && !holds {
                            continue;
                        }
                        let mut path = level.path.clone();
                        path.extend([index, i]);
                        frame.videos.push(VideoSample {
                            path,
                            track: &track.name,
                            video: &s.video,
                            target: &s.target,
                            binding: s.target.binding().map(|name| renamed(name, &level.renames)),
                            position: s.source_time(time),
                            rate: level.rate,
                            volume_db: s.volume_db,
                            looping: s.looping,
                        });
                    }
                }
                // Event keys fire from the player's clock (`SequenceEventFired`); they sample nothing.
                Channel::Event(_) => {}
                Channel::Sub(sections) => {
                    let Some(library) = library else { continue };
                    if level.depth >= MAX_SUB_DEPTH {
                        continue;
                    }
                    for (i, s) in sections.iter().enumerate() {
                        if !in_effect(s.start, s.end, time, sections.iter().map(|o| o.start)) {
                            continue;
                        }
                        let Some(sub) = library.get(&s.sequence) else {
                            continue;
                        };
                        if *budget == 0 || level.stack.contains(&s.sequence.as_str()) {
                            continue;
                        }
                        *budget -= 1;
                        // Push this section onto the walk and pop it after the child, so the
                        // rename, path and stack vecs are reused instead of cloned per section.
                        let renames_len = level.renames.len();
                        let path_len = level.path.len();
                        let stack_len = level.stack.len();
                        let parent_rate = level.rate;
                        let parent_depth = level.depth;
                        let parent_remaining = level.remaining;
                        let parent_loops = level.loops;
                        level.renames.push(&s.bindings);
                        level.path.extend([index, i]);
                        level.stack.push(s.sequence.as_str());
                        // A looping section past the loop depth limit plays once.
                        let looping = s.repeat.loops() && parent_loops < MAX_LOOP_DEPTH;
                        let (local, _) = if looping {
                            s.looped_time(time, sub.duration())
                        } else {
                            (s.local_time(time), 0)
                        };
                        level.rate = parent_rate * f64::from(s.speed);
                        level.depth = parent_depth + 1;
                        level.remaining = parent_remaining.min((f64::from(s.end) - time) / parent_rate);
                        level.loops = parent_loops + usize::from(looping);
                        // Unclamped: before the sub-sequence's start and past its end its keys
                        // hold and its sounds and sections are out of range by themselves.
                        sub.sample_level(local, Some(library), level, budget, frame);
                        level.renames.truncate(renames_len);
                        level.path.truncate(path_len);
                        level.stack.truncate(stack_len);
                        level.rate = parent_rate;
                        level.depth = parent_depth;
                        level.remaining = parent_remaining;
                        level.loops = parent_loops;
                    }
                }
            }
        }
    }

    /// Every sub-sequence section in effect at `time` (clamped like [`sample`](Self::sample)),
    /// through `library`, in tree order: the pass each one plays and its local time. Sections are
    /// followed and skipped exactly as [`sample_with`](Self::sample_with) follows them; a looping
    /// section past [`MAX_LOOP_DEPTH`] reports `pass_length: None` (it plays once).
    #[must_use]
    pub fn sub_passes<'a>(&'a self, time: f64, library: &'a SequenceLibrary) -> Vec<SubPass<'a>> {
        let total = self.duration();
        let time = if time.is_finite() {
            time.clamp(0.0, total)
        } else {
            0.0
        };
        let mut walk = PassWalk {
            library,
            stack: Vec::new(),
            budget: MAX_TREE_SECTIONS,
            out: Vec::new(),
        };
        self.sub_passes_level(time, &[], 0, &mut walk);
        walk.out
    }

    fn sub_passes_level<'a>(&'a self, time: f64, path: &[usize], loops: usize, walk: &mut PassWalk<'a>) {
        if walk.stack.len() >= MAX_SUB_DEPTH {
            return;
        }
        for (index, track) in self.tracks.iter().enumerate() {
            let Channel::Sub(sections) = &track.keys else {
                continue;
            };
            if !track.enabled {
                continue;
            }
            for (i, s) in sections.iter().enumerate() {
                if !in_effect(s.start, s.end, time, sections.iter().map(|o| o.start)) {
                    continue;
                }
                let Some(sub) = walk.library.get(&s.sequence) else {
                    continue;
                };
                if walk.budget == 0 || walk.stack.contains(&s.sequence.as_str()) {
                    continue;
                }
                walk.budget -= 1;
                let looping = s.repeat.loops() && loops < MAX_LOOP_DEPTH;
                let (local_time, pass) = if looping {
                    s.looped_time(time, sub.duration())
                } else {
                    (s.local_time(time), 0)
                };
                let mut here = path.to_vec();
                here.extend([index, i]);
                walk.out.push(SubPass {
                    path: here.clone(),
                    track: &track.name,
                    sequence: &s.sequence,
                    repeat: s.repeat,
                    pass_length: if looping {
                        s.pass_length(sub.duration())
                    } else {
                        None
                    },
                    pass,
                    local_time,
                    depth: walk.stack.len() + 1,
                });
                walk.stack.push(&s.sequence);
                sub.sub_passes_level(local_time, &here, loops + usize::from(looping), walk);
                walk.stack.pop();
            }
        }
    }

    /// Every marker of this sequence and of the sub-sequences it plays (through `library`), on
    /// this sequence's clock, sorted by time then name. A sub-sequence marker counts when its
    /// time falls inside its section, `[start, end]`; it is named `track/marker`. Markers of a
    /// sub-sequence skipped as in [`sample_with`](Self::sample_with) are left out, and so is a
    /// marker of the instant a section ends when the next section of its track starts there.
    #[must_use]
    pub fn flat_markers(&self, library: Option<&SequenceLibrary>) -> Vec<FlatMarker> {
        let mut out: Vec<FlatMarker> = self
            .markers
            .iter()
            .enumerate()
            .map(|(index, m)| FlatMarker {
                index,
                name: m.name.clone(),
                time: f64::from(m.time),
                payload: m.payload.clone(),
            })
            .collect();
        if let Some(library) = library {
            let mut walk = MarkerWalk {
                library,
                stack: Vec::new(),
                sections: MAX_TREE_SECTIONS,
                markers: MAX_FLAT_MARKERS.saturating_sub(out.len()),
                out: &mut out,
            };
            let to_root = |here: f64, sink: &mut MarkerSink<'_>| sink(here);
            let steps = std::cell::Cell::new(MARKER_STEPS);
            self.flat_sub_markers(&mut walk, &to_root, &steps, "", 0);
        }
        for (index, marker) in out.iter_mut().enumerate().skip(self.markers.len()) {
            marker.index = index;
        }
        out.sort_by(|a, b| a.time.total_cmp(&b.time).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// Appends the markers of the sub-sequences this sequence plays. `to_root` hands every root
    /// clock time of a time of this sequence to its sink (none outside the sections above it,
    /// one per pass inside looping ones) and stops, answering `false`, as soon as the sink does
    /// (the marker budget is spent); `loops` counts the looping sections above.
    fn flat_sub_markers<'a>(
        &'a self,
        walk: &mut MarkerWalk<'a, '_>,
        to_root: &ToRoot<'_>,
        steps: &std::cell::Cell<usize>,
        prefix: &str,
        loops: usize,
    ) {
        if walk.stack.len() >= MAX_SUB_DEPTH {
            return;
        }
        for track in self.tracks.iter().filter(|t| t.enabled) {
            let Channel::Sub(sections) = &track.keys else {
                continue;
            };
            for s in sections {
                let Some(sub) = walk.library.get(&s.sequence) else {
                    continue;
                };
                if walk.sections == 0 || walk.markers == 0 || walk.stack.contains(&s.sequence.as_str()) {
                    continue;
                }
                walk.sections -= 1;
                let name = format!("{prefix}{}/", track.name);
                let looping = s.repeat.loops() && loops < MAX_LOOP_DEPTH;
                let pass = if looping {
                    s.pass_length(sub.duration())
                } else {
                    None
                };
                // Like sampling: back-to-back sections never both count at their shared instant.
                let map = |local: f64, sink: &mut MarkerSink<'_>| -> bool {
                    let mut at = |here: f64| {
                        !in_effect(s.start, s.end, here, sections.iter().map(|o| o.start))
                            || to_root(here, &mut *sink)
                    };
                    let Some(length) = pass else {
                        return at(s.parent_time(local));
                    };
                    // One time per pass for a local time inside the pass (its end included: the
                    // instant a pass ends is where the next begins).
                    let within = local - f64::from(s.offset);
                    if !(0.0..=length).contains(&within) {
                        return true;
                    }
                    let passes = match s.repeat {
                        SubRepeat::Count(n) => u64::from(n),
                        SubRepeat::Once | SubRepeat::UntilStop => u64::MAX,
                    };
                    let (start, end, speed) = (f64::from(s.start), f64::from(s.end), f64::from(s.speed));
                    let mut k = 0u64;
                    while k < passes {
                        let Some(left) = steps.get().checked_sub(1) else {
                            return false;
                        };
                        steps.set(left);
                        let here = start + (within + k as f64 * length) / speed;
                        if here > end {
                            break;
                        }
                        if !at(here) {
                            return false;
                        }
                        k += 1;
                    }
                    true
                };
                for m in &sub.markers {
                    let marker_name = format!("{name}{}", m.name);
                    let (out, budget) = (&mut *walk.out, &mut walk.markers);
                    map(f64::from(m.time), &mut |time| {
                        if *budget == 0 {
                            return false;
                        }
                        *budget -= 1;
                        out.push(FlatMarker {
                            index: 0,
                            name: marker_name.clone(),
                            time,
                            payload: m.payload.clone(),
                        });
                        true
                    });
                }
                walk.stack.push(&s.sequence);
                sub.flat_sub_markers(walk, &map, steps, &name, loops + usize::from(looping));
                walk.stack.pop();
            }
        }
    }

    /// The markers whose times fall in `(from, to]`, in time order (declaration order for equal
    /// times): what a step of playback from `from` to `to` fires. `to` included, `from` not, so a
    /// marker fires exactly once as the clock reaches it. A backward or paused step
    /// (`from >= to`) and non-finite bounds fire nothing. Seeking fires nothing by itself; a
    /// player chooses what a jump crosses, and covers a marker at the clock's start (time 0) by
    /// asking from before it, e.g. `markers_between(-1.0, 0.0)`.
    ///
    /// Markers are not required to be sorted in the document; the output is.
    #[must_use]
    pub fn markers_between(&self, from: f64, to: f64) -> Vec<&Marker> {
        if !from.is_finite() || !to.is_finite() || from >= to {
            return Vec::new();
        }
        let mut hits: Vec<&Marker> = self
            .markers
            .iter()
            .filter(|m| {
                let t = f64::from(m.time);
                from < t && t <= to
            })
            .collect();
        hits.sort_by(|a, b| a.time.total_cmp(&b.time));
        hits
    }

    /// The marker with this name.
    #[must_use]
    pub fn marker(&self, name: &str) -> Option<&Marker> {
        self.markers.iter().find(|m| m.name == name)
    }

    /// Every target name an enabled keyed or clip track binds, every binding a spatial sound
    /// section emits from (v3) and every binding a video section shows on (v4), through the
    /// sub-sequences `library` holds (renamed), sorted and deduplicated: the names a player
    /// resolves. Flat sound sections, fullscreen video sections and sub-sequence tracks bind
    /// nothing.
    #[must_use]
    pub fn target_names(&self, library: Option<&SequenceLibrary>) -> Vec<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut budget = MAX_TREE_SECTIONS;
        self.collect_targets(library, &mut Vec::new(), &mut Vec::new(), &mut budget, &mut out);
        out.into_iter().collect()
    }

    /// `true` when an enabled track holds video sections (v4), through the sub-sequences `library`
    /// holds.
    #[must_use]
    pub fn has_video_sections(&self, library: Option<&SequenceLibrary>) -> bool {
        let mut budget = MAX_TREE_SECTIONS;
        self.video_in_tree(library, &mut Vec::new(), &mut budget)
    }

    fn video_in_tree<'a>(
        &'a self,
        library: Option<&'a SequenceLibrary>,
        stack: &mut Vec<&'a str>,
        budget: &mut usize,
    ) -> bool {
        self.tracks
            .iter()
            .filter(|t| t.enabled)
            .any(|track| match &track.keys {
                Channel::Video(_) => true,
                Channel::Sub(sections) if stack.len() < MAX_SUB_DEPTH => sections.iter().any(|s| {
                    let Some(sub) = library.and_then(|l| l.get(&s.sequence)) else {
                        return false;
                    };
                    if *budget == 0 || stack.contains(&s.sequence.as_str()) {
                        return false;
                    }
                    *budget -= 1;
                    stack.push(&s.sequence);
                    let found = sub.video_in_tree(library, stack, budget);
                    stack.pop();
                    found
                }),
                _ => false,
            })
    }

    fn collect_targets<'a>(
        &'a self,
        library: Option<&'a SequenceLibrary>,
        renames: &mut Vec<&'a BTreeMap<String, String>>,
        stack: &mut Vec<&'a str>,
        budget: &mut usize,
        out: &mut std::collections::BTreeSet<String>,
    ) {
        for track in self.tracks.iter().filter(|t| t.enabled) {
            match &track.keys {
                Channel::Audio(sections) => {
                    for s in sections {
                        if let Some(SoundSource::Binding(name)) = s.spatial.as_ref().map(|p| &p.source) {
                            out.insert(renamed(name, renames).to_string());
                        }
                    }
                }
                Channel::Video(sections) => {
                    for s in sections {
                        if let Some(name) = s.target.binding() {
                            out.insert(renamed(name, renames).to_string());
                        }
                    }
                }
                Channel::Sub(sections) => {
                    let Some(library) = library else { continue };
                    if renames.len() >= MAX_SUB_DEPTH {
                        continue;
                    }
                    for s in sections {
                        let Some(sub) = library.get(&s.sequence) else {
                            continue;
                        };
                        if *budget == 0 || stack.contains(&s.sequence.as_str()) {
                            continue;
                        }
                        *budget -= 1;
                        renames.push(&s.bindings);
                        stack.push(&s.sequence);
                        sub.collect_targets(Some(library), renames, stack, budget, out);
                        stack.pop();
                        renames.pop();
                    }
                }
                _ => {
                    out.insert(renamed(&track.target, renames).to_string());
                }
            }
        }
    }

    /// The sub-sequence names this sequence's enabled sub-sequence tracks use (not followed
    /// further), sorted and deduplicated: what a loader must put in the [`SequenceLibrary`].
    #[must_use]
    pub fn sub_sequence_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .tracks
            .iter()
            .filter(|t| t.enabled)
            .filter_map(|t| match &t.keys {
                Channel::Sub(sections) => Some(sections.iter().map(|s| s.sequence.as_str())),
                _ => None,
            })
            .flatten()
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::super::asset::Track;
    use super::super::asset::fixtures::{float_track, marker, sequence};
    use super::*;

    const EPS: f32 = 1.0e-5;
    /// Sections per fan-out level in the tree-size test.
    const MAX_SECTIONS_USED: usize = 16;

    fn scalar(keys: &[(f32, f32, Ease)]) -> Channel {
        Channel::Float(
            keys.iter()
                .map(|&(time, value, ease)| ScalarKey::new(time, value, ease))
                .collect(),
        )
    }

    #[test]
    fn a_tree_that_fans_out_or_contains_itself_is_refused_and_every_walk_stays_bounded() {
        let fan = |to: &str, marker_time: Option<f32>| {
            let sections = (0..MAX_SECTIONS_USED)
                .map(|_| sub_section(to, 0.0, 1.0, 0.0, 1.0))
                .collect();
            let markers = marker_time.map_or_else(Vec::new, |t| vec![marker("m", t)]);
            sequence(vec![section_track("s", "", Channel::Sub(sections))], markers)
        };
        // Eight levels of 16 sections each: 16^8 expanded, every file small.
        let mut library = SequenceLibrary::default();
        for level in 0..7 {
            library.insert(format!("l{level}"), fan(&format!("l{}", level + 1), Some(0.5)));
        }
        library.insert("l7", sequence(vec![], vec![marker("leaf", 0.25)]));
        let root = fan("l0", None);
        let error = library.check(&root).expect_err("too big");
        assert!(error.to_string().contains("expands to more"), "{error}");
        // A library entry holding itself many times.
        let mut cycle = SequenceLibrary::default();
        cycle.insert("a", fan("a", Some(0.5)));
        let error = cycle.check(&fan("a", None)).expect_err("a cycle");
        assert!(error.to_string().contains("contains itself"), "{error}");
        // A player follows such a library anyway: the walks end, and a sequence inside itself
        // plays once, so nothing fires as `s/s/m`.
        let root = fan("a", None);
        let markers = root.flat_markers(Some(&cycle));
        assert!(markers.iter().all(|m| m.name == "s/m"), "{markers:?}");
        assert_eq!(markers.len(), MAX_SECTIONS_USED);
        assert!(root.target_names(Some(&cycle)).is_empty());
        let wide = fan("l0", None);
        assert!(wide.flat_markers(Some(&library)).len() <= MAX_TREE_SECTIONS);
        assert_eq!(wide.sample_with(0.5, &library).values.len(), 0);
    }

    #[test]
    fn a_split_sub_sequence_fires_its_marker_at_the_seam_once() {
        let inner = sequence(vec![], vec![marker("seam", 2.0)]);
        let mut library = SequenceLibrary::default();
        library.insert("inner", inner);
        let root = sequence(
            vec![section_track(
                "s",
                "",
                Channel::Sub(vec![
                    sub_section("inner", 0.0, 2.0, 0.0, 1.0),
                    sub_section("inner", 2.0, 4.0, 2.0, 1.0),
                ]),
            )],
            vec![],
        );
        assert_eq!(root.flat_markers(Some(&library)).len(), 1);
    }

    #[test]
    fn editing_a_middle_section_end_updates_the_cached_duration() {
        // Sections are not ordered by end: the latest end can sit in a middle section, and an
        // in-place edit of it must not leave a cloned sequence with the old cached length.
        let audio = |start: f32, end: f32| AudioSection {
            sound: "s".into(),
            start,
            end,
            offset: 0.0,
            volume_db: 0.0,
            fade_in: 0.0,
            fade_out: 0.0,
            bus: None,
            time_scale: 1.0,
            spatial: None,
            time_scale_mode: TimeScaleMode::Varispeed,
        };
        let seq = sequence(
            vec![section_track(
                "a",
                "",
                Channel::Audio(vec![audio(0.0, 1.0), audio(0.0, 10.0), audio(0.0, 2.0)]),
            )],
            vec![],
        );
        assert_eq!(seq.duration(), 10.0);
        let mut longer = seq.clone();
        if let Channel::Audio(sections) = &mut longer.tracks[0].keys {
            sections[1].end = 20.0;
        }
        assert_eq!(longer.duration(), 20.0);
        let mut shorter = seq.clone();
        if let Channel::Audio(sections) = &mut shorter.tracks[0].keys {
            sections[1].end = 1.5;
        }
        assert_eq!(shorter.duration(), 2.0);
    }

    #[test]
    fn a_negative_offset_delays_a_sub_sequence_sound_and_an_outer_end_limits_remaining() {
        let audio = |start: f32, end: f32| {
            section_track(
                "steps",
                "",
                Channel::Audio(vec![AudioSection {
                    sound: "step".into(),
                    start,
                    end,
                    offset: 0.0,
                    volume_db: 0.0,
                    fade_in: 0.0,
                    fade_out: 0.0,
                    bus: None,
                    time_scale: 1.0,
                    spatial: None,
                    time_scale_mode: TimeScaleMode::Varispeed,
                }]),
            )
        };
        let mut library = SequenceLibrary::default();
        library.insert("inner", sequence(vec![audio(0.0, 10.0)], vec![]));
        let root = sequence(
            vec![section_track(
                "s",
                "",
                Channel::Sub(vec![sub_section("inner", 1.0, 3.0, -1.0, 1.0)]),
            )],
            vec![],
        );
        assert!(
            root.sample_with(1.5, &library).sounds.is_empty(),
            "local -0.5: the sound has not started"
        );
        let frame = root.sample_with(2.5, &library);
        let sound = &frame.sounds[0];
        assert_eq!(sound.position, 0.5);
        assert_eq!(sound.remaining, 0.5, "the section above ends first");
    }

    #[test]
    fn a_nan_time_reads_the_first_key() {
        let one = scalar(&[(1.0, 7.0, Ease::Linear)]);
        assert_eq!(one.sample(f32::NAN), ChannelValue::Float(7.0));
        let flag = Channel::Bool(vec![
            BoolKey {
                time: 1.0,
                value: true,
            },
            BoolKey {
                time: 2.0,
                value: false,
            },
        ]);
        assert_eq!(flag.sample(f32::NAN), ChannelValue::Bool(true));
    }

    #[test]
    fn a_nested_marker_outside_an_outer_section_does_not_fire() {
        let leaf = sequence(vec![], vec![marker("early", 0.5), marker("late", 3.0)]);
        let middle = sequence(
            vec![section_track(
                "in",
                "",
                Channel::Sub(vec![sub_section("leaf", 0.0, 4.0, 0.0, 1.0)]),
            )],
            vec![],
        );
        let mut library = SequenceLibrary::default();
        library.insert("leaf", leaf);
        library.insert("middle", middle);
        // The outer section shows the first second of `middle` only.
        let root = sequence(
            vec![section_track(
                "out",
                "",
                Channel::Sub(vec![sub_section("middle", 2.0, 3.0, 0.0, 1.0)]),
            )],
            vec![],
        );
        let markers = root.flat_markers(Some(&library));
        let names: Vec<(&str, f64)> = markers.iter().map(|m| (m.name.as_str(), m.time)).collect();
        assert_eq!(names, vec![("out/in/early", 2.5)]);
    }

    #[test]
    fn a_tree_reads_each_sequence_once_and_refuses_what_check_refuses() {
        let sub = |to: &str| {
            format!(
                r#"(version: 2, tracks: [(name: "s", keys: Sub([(sequence: "{to}", start: 0.0, end: 1.0)]))])"#
            )
        };
        let files: BTreeMap<&str, String> = [
            ("a", sub("b")),
            ("b", sub("leaf")),
            ("leaf", "(version: 2)".to_string()),
            ("self", sub("self")),
            ("x", sub("y")),
            ("y", sub("x")),
            ("broken", "(version: 99)".to_string()),
            ("gap", sub("nowhere")),
        ]
        .into();
        let mut reads = Vec::new();
        let read = |name: &str| files.get(name).cloned().ok_or_else(|| format!("missing {name}"));
        let root = |to: &str| Sequence::from_ron(&sub(to)).expect("root");
        let tree = SequenceLibrary::read_tree(
            &root("a"),
            |name| {
                reads.push(name.to_string());
                read(name)
            },
            |m| m,
        )
        .expect("a tree");
        let names: Vec<&str> = tree.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b", "leaf"]);
        assert_eq!(reads.len(), 3, "each once");
        for (to, needle) in [
            ("self", "contains itself"),
            ("x", "contains itself"),
            ("broken", "sub-sequence `broken`"),
            ("gap", "missing nowhere"),
            ("nowhere", "missing nowhere"),
        ] {
            let error = SequenceLibrary::read_tree(&root(to), read, |m| m).expect_err(to);
            assert!(error.contains(needle), "{to}: {error}");
        }
    }

    fn float_of(frame: &SequenceFrame, track: usize) -> f32 {
        match frame.values[track].value {
            ChannelValue::Float(v) => v,
            other => panic!("expected a float, got {other:?}"),
        }
    }

    #[test]
    fn float_channels_hold_at_the_ends_and_ease_between_keys() {
        let channel = scalar(&[
            (1.0, 10.0, Ease::Linear),
            (3.0, 30.0, Ease::Smooth),
            (5.0, 50.0, Ease::Step),
        ]);
        let at = |t: f32| channel.sample(t);
        assert_eq!(at(0.0), ChannelValue::Float(10.0), "before the first key");
        assert_eq!(at(1.0), ChannelValue::Float(10.0));
        assert_eq!(at(2.0), ChannelValue::Float(20.0), "linear midpoint");
        assert_eq!(at(3.0), ChannelValue::Float(30.0));
        assert_eq!(
            at(4.0),
            ChannelValue::Float(40.0),
            "smoothstep is 0.5 at the midpoint"
        );
        assert_eq!(at(5.0), ChannelValue::Float(50.0));
        assert_eq!(at(9.0), ChannelValue::Float(50.0), "after the last key");
    }

    #[test]
    fn a_step_key_holds_its_value_until_the_next_key() {
        let channel = scalar(&[(0.0, 1.0, Ease::Step), (2.0, 5.0, Ease::Linear)]);
        assert_eq!(channel.sample(1.999), ChannelValue::Float(1.0));
        assert_eq!(channel.sample(2.0), ChannelValue::Float(5.0));
    }

    #[test]
    fn vec3_channels_ease_every_component() {
        let channel = Channel::Vec3(vec![
            Vec3Key::new(0.0, [0.0, 10.0, -4.0], Ease::Linear),
            Vec3Key::new(4.0, [8.0, 10.0, 0.0], Ease::Linear),
        ]);
        assert_eq!(channel.sample(2.0), ChannelValue::Vec3([4.0, 10.0, -2.0]));
        assert_eq!(
            channel.sample(-1.0),
            ChannelValue::Vec3([0.0, 10.0, -4.0]),
            "held"
        );
        let stepped = Channel::Vec3(vec![
            Vec3Key::new(0.0, [1.0, 2.0, 3.0], Ease::Step),
            Vec3Key::new(1.0, [9.0, 9.0, 9.0], Ease::Step),
        ]);
        assert_eq!(stepped.sample(0.5), ChannelValue::Vec3([1.0, 2.0, 3.0]));
    }

    #[test]
    fn bool_channels_step() {
        let channel = Channel::Bool(vec![
            BoolKey {
                time: 0.0,
                value: false,
            },
            BoolKey {
                time: 2.0,
                value: true,
            },
            BoolKey {
                time: 4.0,
                value: false,
            },
        ]);
        assert_eq!(channel.sample(0.0), ChannelValue::Bool(false));
        assert_eq!(channel.sample(1.9), ChannelValue::Bool(false));
        assert_eq!(channel.sample(2.0), ChannelValue::Bool(true));
        assert_eq!(channel.sample(3.9), ChannelValue::Bool(true));
        assert_eq!(channel.sample(4.0), ChannelValue::Bool(false));
        assert_eq!(channel.sample(9.0), ChannelValue::Bool(false), "held");
    }

    #[test]
    fn empty_channels_sample_their_kind_zero() {
        // Validation rejects these; direct construction is the host's risk and stays defined.
        assert_eq!(Channel::Float(Vec::new()).sample(1.0), ChannelValue::Float(0.0));
        assert_eq!(
            Channel::Vec3(Vec::new()).sample(1.0),
            ChannelValue::Vec3([0.0; 3])
        );
        assert_eq!(Channel::Bool(Vec::new()).sample(1.0), ChannelValue::Bool(false));
    }

    #[test]
    fn duration_is_the_latest_key_or_marker_time() {
        let seq = sequence(
            vec![
                float_track("a", "ui", "opacity", 2.0),
                float_track("b", "ui", "scale", 5.0),
            ],
            vec![marker("m", 1.0)],
        );
        assert_eq!(seq.duration(), 5.0);
        let marker_past = sequence(
            vec![float_track("a", "ui", "opacity", 2.0)],
            vec![marker("m", 8.0)],
        );
        assert_eq!(marker_past.duration(), 8.0, "a marker can be the last thing");
        assert_eq!(Sequence::default().duration(), 0.0);
    }

    #[test]
    fn sampling_clamps_and_reports_finished() {
        let seq = sequence(vec![float_track("fade", "ui", "opacity", 2.0)], vec![]);
        assert_eq!(seq.sample(0.0).time, 0.0);
        assert!(!seq.sample(0.0).finished);
        let mid = seq.sample(1.0);
        assert_eq!(mid.time, 1.0);
        assert!((float_of(&mid, 0) - 0.5).abs() < EPS);
        let end = seq.sample(2.0);
        assert!(end.finished, "the clock sits at the end");
        assert_eq!(float_of(&end, 0), 1.0);
        let past = seq.sample(100.0);
        assert_eq!(past.time, 2.0, "clamped");
        assert!(past.finished);
        assert_eq!(float_of(&past, 0), 1.0, "held");
        let nan = seq.sample(f64::NAN);
        assert_eq!(nan.time, 0.0, "a non-finite time reads as 0");
        let empty_sequence = Sequence::default();
        let empty = empty_sequence.sample(0.0);
        assert!(empty.finished, "a zero-length sequence is finished");
        assert!(empty.values.is_empty());
    }

    #[test]
    fn sampling_is_deterministic_and_skips_disabled_tracks() {
        let mut seq = sequence(
            vec![
                float_track("on", "ui", "opacity", 2.0),
                float_track("off", "ui", "scale", 2.0),
            ],
            vec![],
        );
        seq.tracks[1].enabled = false;
        let a = seq.sample(0.7);
        let b = seq.sample(0.7);
        assert_eq!(a, b, "same input, same output");
        assert_eq!(a.values.len(), 1);
        assert_eq!(a.values[0].name, "on");
        assert_eq!(a.values[0].track, 0);
        assert_eq!(a.values[0].target, "ui");
        assert_eq!(a.values[0].property, "opacity");
    }

    #[test]
    fn later_track_wins_for_the_same_target_and_property() {
        let seq = sequence(
            vec![
                float_track("first", "ui", "opacity", 2.0),
                float_track("second", "ui", "opacity", 2.0),
            ],
            vec![],
        );
        let frame = seq.sample(1.0);
        assert_eq!(frame.values.len(), 2, "both appear; the host applies the last");
        assert_eq!(float_of(&frame, 1), 0.5);
    }

    #[test]
    fn markers_fire_in_a_half_open_interval() {
        let seq = sequence(
            vec![],
            vec![
                marker("start", 0.0),
                marker("mid", 1.0),
                marker("end", 2.0),
                marker("late", 3.0),
            ],
        );
        let names = |from: f64, to: f64| {
            seq.markers_between(from, to)
                .iter()
                .map(|m| m.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(0.0, 1.0), vec!["mid"], "to is included, from is not");
        assert_eq!(names(0.0, 0.0), Vec::<&str>::new(), "a paused step fires nothing");
        assert_eq!(
            names(1.0, 0.5),
            Vec::<&str>::new(),
            "a backward step fires nothing"
        );
        assert_eq!(
            names(-1.0, 0.0),
            vec!["start"],
            "a player covers the start marker by asking from before 0"
        );
        assert_eq!(names(0.5, 3.5), vec!["mid", "end", "late"], "time order");
        assert_eq!(names(f64::NAN, 4.0), Vec::<&str>::new());
        assert_eq!(names(0.0, 100.0), vec!["mid", "end", "late"]);
    }

    #[test]
    fn markers_are_reported_in_time_order_even_when_declared_later() {
        let seq = sequence(vec![], vec![marker("second", 2.0), marker("first", 1.0)]);
        let hits = seq.markers_between(0.0, 3.0);
        assert_eq!(
            hits.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn markers_and_tracks_lookup_by_name() {
        let seq = sequence(
            vec![float_track("fade", "ui", "opacity", 2.0)],
            vec![marker("title", 1.0)],
        );
        assert_eq!(seq.marker("title").map(|m| m.time), Some(1.0));
        assert!(seq.marker("nope").is_none());
        assert_eq!(seq.tracks[0].name, "fade");
    }

    // ---- v2: Bezier tangents (ADR-1880) ----

    fn bezier_key(time: f32, value: f32, tangent: Tangent<f32>) -> ScalarKey {
        ScalarKey {
            tangent,
            ..ScalarKey::new(time, value, Ease::Bezier)
        }
    }

    #[test]
    fn an_unweighted_bezier_segment_is_the_hermite_curve_of_its_slopes() {
        let channel = Channel::Float(vec![
            bezier_key(1.0, 2.0, Tangent::Aligned(3.0)),
            bezier_key(3.0, 6.0, Tangent::Broken(-1.0, 4.0)),
        ]);
        for i in 0..=20 {
            let t = 1.0 + 2.0 * i as f32 / 20.0;
            let u = (t - 1.0) / 2.0;
            // Hermite with slopes per second scaled by the 2 s span.
            let expect = regia_animation::curve::hermite(2.0, 6.0, 3.0 * 2.0, -2.0, u);
            let got = channel.sample_component(t, 0).expect("float");
            assert!((got - expect).abs() < 1.0e-5, "t {t}: {got} vs {expect}");
        }
        assert_eq!(channel.tangent_slopes(1, 0), Some((-1.0, 4.0)));
    }

    #[test]
    fn bezier_slopes_are_the_derivative_at_both_keys() {
        let channel = Channel::Float(vec![
            bezier_key(0.0, 0.0, Tangent::Aligned(2.0)),
            bezier_key(1.0, 1.0, Tangent::Aligned(-3.0)),
        ]);
        let h = 1.0e-3f32;
        let d0 =
            (channel.sample_component(h, 0).expect("v") - channel.sample_component(0.0, 0).expect("v")) / h;
        let d1 = (channel.sample_component(1.0, 0).expect("v")
            - channel.sample_component(1.0 - h, 0).expect("v"))
            / h;
        assert!((d0 - 2.0).abs() < 1.0e-2, "start slope {d0}");
        assert!((d1 + 3.0).abs() < 1.0e-2, "end slope {d1}");
    }

    #[test]
    fn auto_tangents_are_flat_at_extremes_and_never_overshoot_monotone_keys() {
        // A peak at 1.0: flat there, so the curve never rises above it.
        let peak = Channel::Float(vec![
            bezier_key(0.0, 0.0, Tangent::Auto),
            bezier_key(1.0, 1.0, Tangent::Auto),
            bezier_key(2.0, 0.0, Tangent::Auto),
        ]);
        assert_eq!(peak.tangent_slopes(1, 0), Some((0.0, 0.0)));
        // Monotone keys with a steep jump: every sample stays inside its segment's values.
        let steps = [(0.0, 0.0), (1.0, 0.1), (1.2, 5.0), (3.0, 5.2), (4.0, 9.0)];
        let mono = Channel::Float(
            steps
                .iter()
                .map(|&(t, v)| bezier_key(t, v, Tangent::Auto))
                .collect(),
        );
        let mut last = f32::NEG_INFINITY;
        for i in 0..=4000 {
            let t = 4.0 * i as f32 / 4000.0;
            let v = mono.sample_component(t, 0).expect("v");
            assert!(v >= last - 1.0e-5, "not monotone at {t}: {v} after {last}");
            last = v;
        }
        for i in 0..=400 {
            let t = i as f32 / 400.0 * 4.0;
            let v = peak.sample_component(t, 0).expect("v");
            assert!((-1.0e-6..=1.0 + 1.0e-6).contains(&v), "overshoot at {t}: {v}");
        }
        // Ends are flat: a two-key auto curve is smoothstep.
        let two = Channel::Float(vec![
            bezier_key(0.0, 0.0, Tangent::Auto),
            bezier_key(2.0, 4.0, Tangent::Auto),
        ]);
        let smooth = scalar(&[(0.0, 0.0, Ease::Smooth), (2.0, 4.0, Ease::Linear)]);
        for i in 0..=10 {
            let t = i as f32 * 0.2;
            let (a, b) = (
                two.sample_component(t, 0).expect("v"),
                smooth.sample_component(t, 0).expect("v"),
            );
            assert!((a - b).abs() < 1.0e-5, "t {t}: {a} vs {b}");
        }
    }

    #[test]
    fn linear_and_flat_tangents_resolve_from_the_neighbors() {
        let keys = |tangent| {
            Channel::Float(vec![
                bezier_key(0.0, 0.0, tangent),
                bezier_key(1.0, 2.0, tangent),
                bezier_key(3.0, 3.0, tangent),
            ])
        };
        assert_eq!(keys(Tangent::Linear).tangent_slopes(1, 0), Some((2.0, 0.5)));
        assert_eq!(keys(Tangent::Linear).tangent_slopes(0, 0), Some((2.0, 2.0)));
        assert_eq!(keys(Tangent::Linear).tangent_slopes(2, 0), Some((0.5, 0.5)));
        assert_eq!(keys(Tangent::Flat).tangent_slopes(1, 0), Some((0.0, 0.0)));
        // Linear tangents on every key make every segment a straight line.
        let lin = keys(Tangent::Linear);
        assert!((lin.sample_component(0.5, 0).expect("v") - 1.0).abs() < 1.0e-6);
        assert!((lin.sample_component(2.0, 0).expect("v") - 2.5).abs() < 1.0e-6);
    }

    #[test]
    fn weighted_handles_bend_the_timing_but_keep_the_ends_and_stay_monotone_in_time() {
        let mut a = bezier_key(0.0, 0.0, Tangent::Flat);
        let mut b = bezier_key(1.0, 1.0, Tangent::Flat);
        let unweighted = Channel::Float(vec![a.clone(), b.clone()]);
        a.weights = Some((0.5, 0.9));
        b.weights = Some((0.9, 0.5));
        let weighted = Channel::Float(vec![a, b]);
        assert_eq!(weighted.sample_component(0.0, 0), Some(0.0));
        assert_eq!(weighted.sample_component(1.0, 0), Some(1.0));
        // The 0.9 + 0.9 handles are scaled to fit, so time stays monotone: the value too.
        let mut last = -1.0;
        let mut differs = false;
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let v = weighted.sample_component(t, 0).expect("v");
            assert!(v >= last - 1.0e-6, "t {t}");
            last = v;
            differs |= (v - unweighted.sample_component(t, 0).expect("v")).abs() > 1.0e-3;
        }
        assert!(differs, "the weights change the curve");
        // Symmetric weights keep the midpoint.
        assert!((weighted.sample_component(0.5, 0).expect("v") - 0.5).abs() < 1.0e-5);
        // Deterministic.
        assert_eq!(weighted.sample(0.37), weighted.sample(0.37));
    }

    #[test]
    fn vec3_bezier_keys_use_one_slope_per_component() {
        let channel = Channel::Vec3(vec![
            Vec3Key {
                tangent: Tangent::Aligned([1.0, 0.0, -1.0]),
                ..Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Bezier)
            },
            Vec3Key {
                tangent: Tangent::Flat,
                ..Vec3Key::new(1.0, [1.0, 1.0, 1.0], Ease::Linear)
            },
        ]);
        for c in 0..3 {
            let slope = [1.0, 0.0, -1.0][c];
            let expect = regia_animation::curve::hermite(0.0, 1.0, slope, 0.0, 0.25);
            assert!((channel.sample_component(0.25, c).expect("v") - expect).abs() < 1.0e-6);
        }
        assert_eq!(channel.tangent_slopes(0, 2), Some((-1.0, -1.0)));
        assert_eq!(channel.sample_component(0.5, 3), None);
    }

    // ---- v2: section channels ----

    fn clip(clip: &str, start: f32, end: f32, blend_in: f32, blend_out: f32) -> ClipSection {
        ClipSection {
            clip: clip.into(),
            start,
            end,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in,
            blend_out,
            weight: 1.0,
        }
    }

    fn section_track(name: &str, target: &str, keys: Channel) -> super::super::asset::Track {
        super::super::asset::Track {
            name: name.into(),
            target: target.into(),
            property: String::new(),
            keys,
            enabled: true,
            locked: false,
        }
    }

    #[test]
    fn overlapping_clip_sections_crossfade_and_the_end_instant_holds() {
        let mut walk = clip("walk", 0.0, 2.0, 0.0, 0.5);
        walk.speed = 2.0;
        walk.offset = 0.25;
        let seq = sequence(
            vec![section_track(
                "body",
                "hero",
                Channel::Clip(vec![walk, clip("run", 1.5, 4.0, 0.5, 0.0)]),
            )],
            vec![],
        );
        assert_eq!(seq.duration(), 4.0, "sections end the sequence");
        let at = |t: f64| seq.sample(t).clips;
        let one = at(1.0);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].target, "hero");
        assert_eq!(one[0].blends.len(), 1);
        assert_eq!(one[0].blends[0].clip_time, 2.25, "offset + elapsed × speed");
        let mid = &at(1.75)[0];
        assert_eq!(mid.blends.len(), 2);
        assert!((mid.blends[0].weight - 0.5).abs() < 1.0e-6 && (mid.blends[1].weight - 0.5).abs() < 1.0e-6);
        assert!(
            (mid.total_weight() - 1.0).abs() < 1.0e-6,
            "a linear crossfade sums to one"
        );
        let end_of_walk = &at(2.0)[0];
        assert_eq!(end_of_walk.blends.len(), 1, "walk's blend out reached zero");
        assert_eq!(end_of_walk.blends[0].clip, "run");
        let last = &at(4.0)[0];
        assert_eq!(
            last.blends[0].clip, "run",
            "the last section holds its end instant"
        );
        assert_eq!(last.blends[0].weight, 1.0);
        // A blend in starts from zero: nothing in effect at the start instant.
        let fade_in = sequence(
            vec![section_track(
                "b",
                "hero",
                Channel::Clip(vec![clip("idle", 1.0, 2.0, 0.5, 0.0)]),
            )],
            vec![],
        );
        assert!(fade_in.sample(1.0).clips.is_empty());
        assert!((fade_in.sample(1.25).clips[0].total_weight() - 0.5).abs() < 1.0e-6);
        assert!(fade_in.sample(0.5).clips.is_empty(), "before the section");
    }

    #[test]
    fn back_to_back_sections_never_both_count() {
        let seq = sequence(
            vec![section_track(
                "b",
                "hero",
                Channel::Clip(vec![clip("a", 0.0, 1.0, 0.0, 0.0), clip("b", 1.0, 2.0, 0.0, 0.0)]),
            )],
            vec![],
        );
        let at_cut = &seq.sample(1.0).clips[0];
        assert_eq!(at_cut.blends.len(), 1);
        assert_eq!(at_cut.blends[0].clip, "b");
    }

    #[test]
    fn videos_report_their_source_time_target_and_path_through_a_sub_sequence() {
        use super::super::asset::{Track, VideoScreenSlot, VideoSection};
        let mut screen = VideoSection::new("movies/a.webm", 1.0, 3.0);
        screen.offset = 0.5;
        screen.volume_db = -6.0;
        screen.target = VideoTarget::Screen {
            binding: "tv".into(),
            slot: VideoScreenSlot::Emissive,
        };
        let inner = Sequence {
            tracks: vec![Track {
                name: "wall".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Video(vec![screen, VideoSection::new("movies/b.webm", 3.0, 4.0)]),
                enabled: true,
                locked: false,
            }],
            ..Sequence::default()
        };
        let mut library = SequenceLibrary::default();
        library.insert("inner", inner.clone());
        assert!(inner.sample(0.5).videos.is_empty(), "before the section");
        let own = inner.sample(2.0);
        assert_eq!(own.videos[0].position, 1.5);
        assert_eq!(own.videos[0].binding, Some("tv"));
        assert_eq!(own.videos[0].path, [0, 0]);
        assert_eq!(inner.duration(), 4.0, "a video section's end counts");
        // A seam: the next section starts where this one ends, so only it shows.
        assert_eq!(inner.sample(3.0).videos[0].video, "movies/b.webm");
        assert_eq!(inner.sample(3.0).videos.len(), 1);
        // The last section holds at the end of the sequence.
        assert_eq!(inner.sample(4.0).videos[0].position, 1.0);

        let mut section = SubSection::new("inner", 10.0, 14.0);
        section.speed = 2.0;
        section.bindings.insert("tv".into(), "billboard".into());
        let outer = Sequence {
            tracks: vec![Track {
                name: "fx".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Sub(vec![section]),
                enabled: true,
                locked: false,
            }],
            ..Sequence::default()
        };
        let frame = outer.sample_with(11.0, &library);
        let video = &frame.videos[0];
        assert_eq!(
            video.binding,
            Some("billboard"),
            "renamed by the section above it"
        );
        assert_eq!(video.rate, 2.0, "the speed above it");
        assert_eq!(video.position, 1.5, "local time 2.0, offset 0.5, start 1.0");
        assert_eq!(video.path, [0, 0, 0, 0]);
        assert_eq!(
            outer.target_names(Some(&library)),
            ["billboard"],
            "fullscreen binds nothing"
        );
    }

    #[test]
    fn sounds_report_their_position_fade_and_time_left() {
        let seq = sequence(
            vec![section_track(
                "music",
                "",
                Channel::Audio(vec![AudioSection {
                    sound: "theme".into(),
                    start: 1.0,
                    end: 5.0,
                    offset: 2.0,
                    volume_db: -6.0,
                    fade_in: 1.0,
                    fade_out: 2.0,
                    bus: Some("Music".into()),
                    time_scale: 1.0,
                    spatial: None,
                    time_scale_mode: TimeScaleMode::Varispeed,
                }]),
            )],
            vec![],
        );
        assert!(seq.sample(0.5).sounds.is_empty());
        let s = &seq.sample(1.5).sounds[0];
        assert_eq!(s.path, vec![0, 0]);
        assert_eq!(
            (s.sound, s.position, s.rate, s.remaining),
            ("theme", 2.5, 1.0, 3.5)
        );
        assert_eq!(s.fade, 0.5);
        assert_eq!(s.bus, Some("Music"));
        assert_eq!(seq.sample(4.0).sounds[0].fade, 0.5, "fading out");
        assert!(seq.sample(5.0).sounds.is_empty(), "a sound stops at its end");
    }

    fn sub_section(sequence: &str, start: f32, end: f32, offset: f32, speed: f32) -> SubSection {
        SubSection {
            sequence: sequence.into(),
            start,
            end,
            offset,
            speed,
            bindings: BTreeMap::new(),
            repeat: Default::default(),
            loop_length: None,
        }
    }

    #[test]
    fn a_sub_sequence_plays_on_the_parent_clock_with_renamed_targets() {
        let mut inner = sequence(
            vec![float_track("glow", "lamp", "light.intensity", 2.0)],
            vec![marker("flash", 1.0)],
        );
        inner.tracks.push(section_track(
            "steps",
            "",
            Channel::Audio(vec![AudioSection {
                sound: "step".into(),
                start: 0.0,
                end: 2.0,
                offset: 0.0,
                volume_db: 0.0,
                fade_in: 0.0,
                fade_out: 0.0,
                bus: None,
                time_scale: 1.0,
                spatial: None,
                time_scale_mode: TimeScaleMode::Varispeed,
            }]),
        ));
        let mut library = SequenceLibrary::default();
        library.insert("fx/glow.seq.ron", inner);
        let mut section = sub_section("fx/glow.seq.ron", 3.0, 4.0, 0.0, 2.0);
        section.bindings.insert("lamp".into(), "porch_light".into());
        let parent = sequence(
            vec![
                float_track("own", "ui", "opacity", 1.0),
                section_track("fx", "", Channel::Sub(vec![section])),
            ],
            vec![marker("start", 0.0)],
        );
        library.check(&parent).expect("tree is complete");
        assert_eq!(parent.sub_sequence_names(), vec!["fx/glow.seq.ron"]);
        assert_eq!(parent.duration(), 4.0);
        // 3.5 s on the parent is 1.0 s in the sub-sequence (speed 2).
        let frame = parent.sample_with(3.5, &library);
        assert_eq!(frame.values.len(), 2);
        let glow = &frame.values[1];
        assert_eq!(
            (glow.target, glow.depth, glow.property),
            ("porch_light", 1, "light.intensity")
        );
        assert_eq!(glow.value, ChannelValue::Float(0.5));
        let step = &frame.sounds[0];
        assert_eq!(step.path, vec![1, 0, 1, 0]);
        assert_eq!((step.position, step.rate, step.remaining), (1.0, 2.0, 0.5));
        // Without the library the sub-sequence is not followed.
        assert_eq!(parent.sample(3.5).values.len(), 1);
        assert!(
            parent.sample_with(2.0, &library).values.len() == 1,
            "before the section"
        );
        // The sub-sequence's marker at 1.0 lands at 3.5 on the parent clock.
        let markers = parent.flat_markers(Some(&library));
        let names: Vec<(&str, f64)> = markers.iter().map(|m| (m.name.as_str(), m.time)).collect();
        assert_eq!(names, vec![("start", 0.0), ("fx/flash", 3.5)]);
        assert_eq!(markers[1].index, 1);
        assert_eq!(parent.flat_markers(None).len(), 1);
    }

    #[test]
    fn nested_sub_sequences_compose_time_and_renames_and_cycles_are_refused() {
        let mut library = SequenceLibrary::default();
        library.insert(
            "leaf",
            sequence(
                vec![float_track("v", "a", "opacity", 1.0)],
                vec![marker("m", 0.5)],
            ),
        );
        let mut mid_section = sub_section("leaf", 1.0, 2.0, 0.0, 1.0);
        mid_section.bindings.insert("a".into(), "b".into());
        library.insert(
            "mid",
            sequence(
                vec![section_track("inner", "", Channel::Sub(vec![mid_section]))],
                vec![],
            ),
        );
        let mut top_section = sub_section("mid", 10.0, 12.0, 0.0, 1.0);
        top_section.bindings.insert("b".into(), "c".into());
        let top = sequence(
            vec![section_track("outer", "", Channel::Sub(vec![top_section]))],
            vec![],
        );
        library.check(&top).expect("complete");
        let frame = top.sample_with(11.5, &library);
        assert_eq!(frame.values.len(), 1);
        assert_eq!((frame.values[0].target, frame.values[0].depth), ("c", 2));
        assert_eq!(frame.values[0].value, ChannelValue::Float(0.5));
        let markers = top.flat_markers(Some(&library));
        assert_eq!(markers.len(), 1);
        assert_eq!(
            (markers[0].name.as_str(), markers[0].time),
            ("outer/inner/m", 11.5)
        );

        // A cycle and a missing name are refused by the check, and sampling still terminates.
        library.insert(
            "loop",
            sequence(
                vec![section_track(
                    "me",
                    "",
                    Channel::Sub(vec![sub_section("loop", 0.0, 1.0, 0.0, 1.0)]),
                )],
                vec![],
            ),
        );
        let looping = library.get("loop").expect("inserted").clone();
        assert!(
            library
                .check(&looping)
                .expect_err("cycle")
                .to_string()
                .contains("contains itself")
        );
        let _ = looping.sample_with(0.5, &library);
        let _ = looping.flat_markers(Some(&library));
        let missing = sequence(
            vec![section_track(
                "x",
                "",
                Channel::Sub(vec![sub_section("nope", 0.0, 1.0, 0.0, 1.0)]),
            )],
            vec![],
        );
        assert!(
            library
                .check(&missing)
                .expect_err("missing")
                .to_string()
                .contains("no sub-sequence `nope`")
        );
        let revision = library.revision();
        assert!(library.remove("loop"));
        assert!(library.revision() > revision);
    }

    /// Extreme but valid keys (ADR-2135): values at the edges of the f32 range and keys a few
    /// subnormals apart sample finite values with every ease and tangent mode.
    #[test]
    fn extreme_valid_keys_sample_finite_values() {
        let big = f32::MAX;
        let tiny = f32::from_bits(1); // the smallest subnormal
        let cases: Vec<Vec<(f32, f32)>> = vec![
            vec![(0.0, -big), (1.0, big)],
            vec![(0.0, -big), (1.0, big), (2.0, -big)],
            vec![(0.0, 0.0), (tiny, big), (2.0 * tiny, -big), (1.0, big)],
            vec![(0.0, big), (1.0e-30, -big), (1.0, 0.0)],
        ];
        for keys in &cases {
            for ease in [Ease::Linear, Ease::Smooth, Ease::Step, Ease::Bezier] {
                for tangent in [
                    Tangent::Auto,
                    Tangent::Linear,
                    Tangent::Flat,
                    Tangent::Aligned(big),
                ] {
                    for weights in [None, Some((1.0, 1.0)), Some((tiny, 0.5))] {
                        let channel = Channel::Float(
                            keys.iter()
                                .map(|&(time, value)| ScalarKey {
                                    tangent,
                                    weights,
                                    ..ScalarKey::new(time, value, ease)
                                })
                                .collect(),
                        );
                        let end = keys.last().map_or(1.0, |k| k.0);
                        for step in 0..=64 {
                            let t = end * step as f32 / 64.0;
                            for t in [t, tiny, 0.5 * tiny + tiny] {
                                let v = channel.sample_component(t, 0).expect("float");
                                assert!(
                                    v.is_finite(),
                                    "{keys:?} {ease:?} {tangent:?} {weights:?} at {t}: {v}"
                                );
                            }
                        }
                        for i in 0..keys.len() {
                            let (a, b) = channel.tangent_slopes(i, 0).expect("slopes");
                            assert!(a.is_finite() && b.is_finite(), "{keys:?} key {i}: {a} {b}");
                        }
                    }
                }
            }
        }
    }

    /// The Bezier segment the way it was before the finite guard (ADR-2135): unguarded slopes and
    /// the plain cast.
    fn unguarded_bezier(keys: &[ScalarKey], i: usize, time: f32) -> f32 {
        let (ta, tb) = (f64::from(keys.time(i)), f64::from(keys.time(i + 1)));
        let (va, vb) = (f64::from(keys.value(i, 0)), f64::from(keys.value(i + 1, 0)));
        let dt = tb - ta;
        let m0 = f64::from(raw_slopes(keys, i, 0).1);
        let m1 = f64::from(raw_slopes(keys, i + 1, 0).0);
        let mut wa = f64::from(keys.weights(i).1);
        let mut wb = f64::from(keys.weights(i + 1).0);
        let sum = wa + wb;
        if sum > 1.0 {
            wa /= sum;
            wb /= sum;
        }
        let x = ((f64::from(time) - ta) / dt).clamp(0.0, 1.0);
        let third = f64::from(DEFAULT_WEIGHT);
        let u = if wa == third && wb == third {
            x
        } else {
            let (mut lo, mut hi) = (0.0f64, 1.0f64);
            for _ in 0..SOLVE_STEPS {
                let mid = 0.5 * (lo + hi);
                if cubic(0.0, wa, 1.0 - wb, 1.0, mid) < x {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            0.5 * (lo + hi)
        };
        cubic(va, va + m0 * wa * dt, vb - m1 * wb * dt, vb, u) as f32
    }

    /// The finite guard leaves ordinary curves bit-identical: the f32 lerp and the unguarded
    /// Bezier, recomputed here the way they were before the guard.
    #[test]
    fn the_finite_guard_keeps_ordinary_curves_bit_identical() {
        let keys = vec![
            ScalarKey::new(0.0, -1.25, Ease::Linear),
            ScalarKey::new(0.7, 3.5, Ease::Smooth),
            ScalarKey::new(1.9, 0.125, Ease::Bezier),
            ScalarKey {
                weights: Some((0.6, 0.2)),
                ..ScalarKey::new(3.0, 2.0, Ease::Bezier)
            },
            ScalarKey::new(4.0, -0.5, Ease::Linear),
        ];
        let channel = Channel::Float(keys.clone());
        for step in 0..=400 {
            let t = step as f32 * 0.01;
            let got = channel.sample_component(t, 0).expect("float");
            let i = keys
                .iter()
                .rposition(|k| k.time <= t)
                .unwrap_or(0)
                .min(keys.len() - 2);
            let (a, b) = (keys[i].value, keys[i + 1].value);
            let expected = match keys[i].ease {
                Ease::Bezier => unguarded_bezier(keys.as_slice(), i, t),
                ease => a + (b - a) * ease_u(ease, segment_u(keys[i].time, keys[i + 1].time, t)),
            };
            let last = &keys[keys.len() - 1];
            let expected = if t <= keys[0].time {
                keys[0].value
            } else if t >= last.time {
                last.value
            } else {
                expected
            };
            assert_eq!(got.to_bits(), expected.to_bits(), "at {t}");
        }
    }

    #[test]
    fn a_nan_key_time_does_not_abort_sampling() {
        let one = [ScalarKey::new(f32::NAN, 7.0, Ease::Linear)];
        assert_eq!(sample_curve(&one[..], 0, 0.0), Some(7.0));
        let bools = [BoolKey {
            time: f32::NAN,
            value: true,
        }];
        assert_eq!(sample_bool(&bools, 0.0), Some(true));
        let bools = [
            BoolKey {
                time: f32::NAN,
                value: true,
            },
            BoolKey {
                time: 5.0,
                value: false,
            },
        ];
        assert_eq!(sample_bool(&bools, 1.0), Some(true));
    }

    /// The looping child of the loop tests: `x` slides 0 → 1 over two seconds, a marker at 0.5
    /// and a sound from 1.0.
    fn loop_child() -> Sequence {
        let mut slide = float_track("slide", "lamp", "x", 2.0);
        if let Channel::Float(keys) = &mut slide.keys {
            keys[1].ease = Ease::Linear;
        }
        sequence(
            vec![
                slide,
                section_track(
                    "hum",
                    "",
                    Channel::Audio(vec![AudioSection::new("hum", 1.0, 2.0)]),
                ),
            ],
            vec![marker("tick", 0.5)],
        )
    }

    fn looping(repeat: SubRepeat, start: f32, end: f32, speed: f32) -> SubSection {
        SubSection {
            repeat,
            ..sub_section("child", start, end, 0.0, speed)
        }
    }

    fn values_of(frame: &SequenceFrame<'_>) -> Vec<(String, ChannelValue)> {
        frame
            .values
            .iter()
            .map(|v| (v.target.to_string(), v.value))
            .collect()
    }

    #[test]
    fn a_looping_sub_sequence_evaluates_the_same_at_t_and_t_plus_a_period() {
        let mut library = SequenceLibrary::default();
        library.insert("child", loop_child());
        // Speed 2: one two-second pass of the child is one second of the parent.
        let root = sequence(
            vec![section_track(
                "loop",
                "",
                Channel::Sub(vec![looping(SubRepeat::UntilStop, 1.0, 9.0, 2.0)]),
            )],
            vec![],
        );
        library.check(&root).expect("a valid tree");
        let period = 1.0;
        for t in [1.0, 1.125, 1.25, 1.5, 1.75, 1.875] {
            let first = root.sample_with(t, &library);
            for k in 1..7 {
                let later = root.sample_with(t + f64::from(k) * period, &library);
                assert_eq!(values_of(&later), values_of(&first), "t = {t}, pass {k}");
                assert_eq!(
                    later.sounds.iter().map(|s| s.position).collect::<Vec<_>>(),
                    first.sounds.iter().map(|s| s.position).collect::<Vec<_>>(),
                    "t = {t}, pass {k}"
                );
            }
        }
        // Mid-pass the slide is where one pass puts it: 0.25 s in at speed 2 is child time 0.5.
        assert_eq!(
            values_of(&root.sample_with(3.25, &library))[0].1,
            ChannelValue::Float(0.25)
        );
        assert_eq!(
            SubSection::looped_time(&looping(SubRepeat::UntilStop, 1.0, 9.0, 2.0), 3.25, 2.0),
            (0.5, 2)
        );
        // The marker fires once per pass, inside the section.
        let ticks: Vec<f64> = root
            .flat_markers(Some(&library))
            .into_iter()
            .filter(|m| m.name == "loop/tick")
            .map(|m| m.time)
            .collect();
        assert_eq!(ticks, [1.25, 2.25, 3.25, 4.25, 5.25, 6.25, 7.25, 8.25]);
        // The sampled values are a pure function of the time.
        assert_eq!(root.sample_with(5.5, &library), root.sample_with(5.5, &library));
    }

    #[test]
    fn a_counted_loop_holds_its_last_pass_and_a_short_period_repeats_within_rounding() {
        let mut library = SequenceLibrary::default();
        library.insert("child", loop_child());
        let root = sequence(
            vec![section_track(
                "loop",
                "",
                Channel::Sub(vec![looping(SubRepeat::Count(2), 0.0, 10.0, 1.0)]),
            )],
            vec![],
        );
        assert_eq!(
            values_of(&root.sample_with(3.0, &library))[0].1,
            ChannelValue::Float(0.5)
        );
        // After two passes (4 s) the end of the last pass holds.
        for t in [4.0, 5.0, 9.5] {
            assert_eq!(
                values_of(&root.sample_with(t, &library))[0].1,
                ChannelValue::Float(1.0),
                "t = {t}"
            );
        }
        let ticks = root.flat_markers(Some(&library));
        assert_eq!(ticks.iter().map(|m| m.time).collect::<Vec<_>>(), [0.5, 2.5]);
        // Count(1) plays once, exactly like v2.
        let once = sequence(
            vec![section_track(
                "loop",
                "",
                Channel::Sub(vec![looping(SubRepeat::Count(1), 0.0, 10.0, 1.0)]),
            )],
            vec![],
        );
        let v2 = sequence(
            vec![section_track(
                "loop",
                "",
                Channel::Sub(vec![sub_section("child", 0.0, 10.0, 0.0, 1.0)]),
            )],
            vec![],
        );
        for t in [0.0, 1.0, 2.5, 7.0] {
            assert_eq!(once.sample_with(t, &library), v2.sample_with(t, &library));
        }
        // A pass that is not a binary fraction repeats within rounding of the parent clock.
        let mut short = looping(SubRepeat::UntilStop, 0.0, 10.0, 1.0);
        short.loop_length = Some(0.3);
        let root = sequence(vec![section_track("loop", "", Channel::Sub(vec![short]))], vec![]);
        for t in [0.05, 0.1, 0.2] {
            let ChannelValue::Float(a) = values_of(&root.sample_with(t, &library))[0].1 else {
                panic!("float")
            };
            for k in 1..20 {
                let ChannelValue::Float(b) =
                    values_of(&root.sample_with(t + 0.3 * f64::from(k), &library))[0].1
                else {
                    panic!("float")
                };
                assert!((a - b).abs() < 1.0e-5, "t = {t}, pass {k}: {a} vs {b}");
            }
        }
        // A degenerate default pass (offset at the child's end) does not loop.
        let mut stuck = looping(SubRepeat::UntilStop, 0.0, 10.0, 1.0);
        stuck.offset = 2.0;
        assert!(stuck.pass_length(2.0).is_none());
    }

    #[test]
    fn looping_sections_nest_at_most_the_loop_depth_and_answer_their_code() {
        // `l{i}` loops `l{i+1}`; the leaf is the slide.
        let chain = |levels: usize| {
            let mut library = SequenceLibrary::default();
            library.insert(format!("l{levels}"), loop_child());
            for i in (0..levels).rev() {
                let s = SubSection {
                    repeat: SubRepeat::UntilStop,
                    ..sub_section(&format!("l{}", i + 1), 0.0, 4.0, 0.0, 1.0)
                };
                library.insert(
                    format!("l{i}"),
                    sequence(vec![section_track("s", "", Channel::Sub(vec![s]))], vec![]),
                );
            }
            let root = sequence(
                vec![section_track(
                    "s",
                    "",
                    Channel::Sub(vec![sub_section("l0", 0.0, 4.0, 0.0, 1.0)]),
                )],
                vec![],
            );
            (library, root)
        };
        let (library, root) = chain(MAX_LOOP_DEPTH);
        library.check(&root).expect("four looping levels are allowed");
        let (library, root) = chain(MAX_LOOP_DEPTH + 1);
        let error = library.check(&root).expect_err("five are not");
        assert_eq!(error.code(), "seq_loop_too_deep");
        assert!(
            matches!(error, SequenceError::LoopTooDeep { depth: 5, .. }),
            "{error}"
        );
        // A looping section above a chain already at the limit takes it one past.
        let mut library = SequenceLibrary::default();
        let (deep, _) = chain(MAX_LOOP_DEPTH);
        for name in deep.names() {
            library.insert(name, deep.get(name).expect("named").clone());
        }
        let outer = SubSection {
            repeat: SubRepeat::Count(2),
            ..sub_section("l0", 0.0, 4.0, 0.0, 1.0)
        };
        let root = sequence(vec![section_track("s", "", Channel::Sub(vec![outer]))], vec![]);
        assert_eq!(
            library.check(&root).expect_err("five").code(),
            "seq_loop_too_deep"
        );
        // Sampling such a tree anyway plays the loop past the limit once: still finite and bounded.
        let frame = root.sample_with(3.5, &library);
        assert_eq!(frame.values.len(), 1);
        assert!(root.flat_markers(Some(&library)).len() <= MAX_FLAT_MARKERS);
    }

    #[test]
    fn a_v2_document_samples_the_same_values_after_migration() {
        // A v2 file with every section kind and a sub-sequence, sampled at fixed times. Every value
        // is the closed form of the v2 rules (linear keys, offsets, speeds, fades).
        let child = Sequence::from_ron(
            r#"(version: 2, tracks: [
                (name: "glow", target: "lamp", property: "intensity",
                    keys: Float([(time: 0.0, value: 0.0), (time: 4.0, value: 8.0)])),
                (name: "hum", keys: Audio([(sound: "hum", start: 0.0, end: 4.0, offset: 0.5, fade_in: 1.0)])),
            ], markers: [(name: "flash", time: 1.0)])"#,
        )
        .expect("v2 child");
        let root = Sequence::from_ron(
            r#"(version: 2, tracks: [
                (name: "move", target: "hero", property: "transform.position",
                    keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 2.0, value: (2.0, 4.0, 6.0))])),
                (name: "music", keys: Audio([(sound: "theme", start: 1.0, end: 3.0, offset: 0.25, volume_db: -6.0, fade_out: 1.0)])),
                (name: "fx", keys: Sub([(sequence: "child", start: 1.0, end: 3.0, offset: 1.0, speed: 2.0, bindings: {"lamp": "porch"})])),
            ])"#,
        )
        .expect("v2 root");
        let current = super::super::asset::SEQUENCE_FORMAT_VERSION;
        assert_eq!((root.version, child.version), (current, current));
        let mut library = SequenceLibrary::default();
        library.insert("child", child);
        let at = |t: f64| root.sample_with(t, &library);
        let f = at(1.5);
        assert_eq!(f.values[0].value, ChannelValue::Vec3([1.5, 3.0, 4.5]));
        // Child time 1.0 + 0.5 * 2 = 2.0: glow 4.0 on the renamed target.
        assert_eq!(
            (f.values[1].target, f.values[1].value),
            ("porch", ChannelValue::Float(4.0))
        );
        let music = &f.sounds[0];
        assert_eq!(
            (music.position, music.rate, music.volume_db, music.fade),
            (0.75, 1.0, -6.0, 1.0)
        );
        assert!(music.spatial.is_none() && music.emitter.is_none());
        let hum = &f.sounds[1];
        assert_eq!((hum.position, hum.rate, hum.fade), (2.5, 2.0, 1.0));
        let f = at(2.5);
        assert_eq!(f.sounds[0].fade, 0.5);
        assert_eq!(f.values[1].value, ChannelValue::Float(8.0));
        let markers = root.flat_markers(Some(&library));
        assert_eq!(markers.len(), 1);
        assert_eq!((markers[0].name.as_str(), markers[0].time), ("fx/flash", 1.0));
    }

    /// A long channel's end time is folded once; the next sample reuses it.
    #[test]
    fn sampling_uses_the_cached_end_time() {
        let keys: Vec<ScalarKey> = (0..5000)
            .map(|i| ScalarKey::new(i as f32 * 0.01, 1.0, Ease::Linear))
            .collect();
        let seq = sequence(
            vec![Track {
                name: "fade".into(),
                target: "ui".into(),
                property: "opacity".into(),
                keys: Channel::Float(keys),
                enabled: true,
                locked: false,
            }],
            vec![],
        );
        sample_counts::reset();
        let first = seq.sample(0.0);
        let folded = sample_counts::ends();
        assert_eq!(folded, 1, "one channel folds its keys once");
        assert!(!first.finished);
        let second = seq.sample(1.0);
        assert_eq!(
            sample_counts::ends(),
            folded,
            "the second sample folded the channel again ({})",
            sample_counts::ends()
        );
        assert_eq!(first.values[0].value, ChannelValue::Float(1.0));
        assert_eq!(second.values[0].value, ChannelValue::Float(1.0));
        assert!((seq.duration() - 49.99).abs() < 1.0e-3, "{}", seq.duration());
    }
}
