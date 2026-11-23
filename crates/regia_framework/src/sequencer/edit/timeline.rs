// SPDX-License-Identifier: MPL-2.0
//! The batch edits of the editor's sequencer timeline (`regia_editor::seq_timeline`, ADR-7322):
//! keys of many tracks moved, scaled, deleted, copied and pasted at once; a marker's time and
//! payload; and the camera cuts and fades of a `.shots.ron` shot list.
//!
//! Every edit works on a copy and validates the whole copy before it replaces the original, so a
//! refused edit changes nothing and says why with a lowercase agent error code
//! ([`EditError::code`]). Nothing here changes a file format: keys and markers are `.seq.ron` v5
//! data (event keys v6), cuts and fades `.shots.ron` data (ADR-268 keeps them there).
//!
//! The keys these edits move, copy and paste are those of the float, vec3, bool and event tracks
//! (`has_keys`); an event track's keys are keys too, though it writes no property
//! (`Channel::is_keyed` is `false` for it). Event keys may share a time (they fire in file order),
//! so a move, scale or ripple never refuses two of them at one time, and a pasted event key goes
//! after the keys already at its time instead of replacing one (ADR-8628).
//!
//! ```
//! use regia_framework::sequencer::edit::timeline::{KeyBatch, KeyRef, apply_key_batch};
//! use regia_framework::sequencer::{Channel, Ease, ScalarKey, Sequence, Track};
//!
//! let mut sequence = Sequence::default();
//! sequence.tracks.push(Track {
//!     name: "fade".into(),
//!     target: "ui".into(),
//!     property: "opacity".into(),
//!     keys: Channel::Float(vec![ScalarKey::new(0.0, 0.0, Ease::Linear), ScalarKey::new(1.0, 1.0, Ease::Linear)]),
//!     enabled: true,
//!     locked: false,
//! });
//! let keys = vec![KeyRef { track: 0, index: 0 }, KeyRef { track: 0, index: 1 }];
//! apply_key_batch(&mut sequence, &KeyBatch::Move { keys, delta: 0.5 }).expect("moved");
//! assert_eq!(sequence.tracks[0].keys.sample_component(1.5, 0), Some(1.0));
//! ```

use std::collections::{BTreeMap, BTreeSet};

/// A stand-in channel for a track index that names no track.
const EMPTY_CHANNEL: crate::sequencer::asset::Channel = crate::sequencer::asset::Channel::Bool(Vec::new());

use super::EditError;
use crate::sequencer::asset::{BoolKey, Channel, EventKey, ScalarKey, Sequence, Tangent, Vec3Key};
use crate::sequencer::marker_sheet::apply_marker_csv;
use crate::shots::{
    CameraPath, LookAt, PathInterp, PathKey, RotationKey, Shot, ShotCamera, ShotSequence, Track as ShotTrack,
    sample_rotation,
};

/// Whether `channel` holds keys (float, vec3, bool and event tracks) rather than sections.
fn has_keys(channel: &Channel) -> bool {
    channel.is_keyed() || matches!(channel, Channel::Event(_))
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// One key of one keyed track: the track's index in [`Sequence::tracks`] and the key's index in
/// its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyRef {
    /// Track index.
    pub track: usize,
    /// Key index in the track's channel.
    pub index: usize,
}

/// One selectable mark of the dope sheet: a key of a keyed track, or the start or end of a
/// section of a clip, audio, sub-sequence or video track (ADR-8640). A box selection or a drag
/// mixes both; [`apply_mark_move`] moves them together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MarkRef {
    /// A key of a float, vec3, bool or event track.
    Key(KeyRef),
    /// The start (`end` is `false`) or end of section `index` of a section track.
    SectionEnd {
        /// Track index.
        track: usize,
        /// Section index in the track's channel.
        index: usize,
        /// `false` for the start, `true` for the end.
        end: bool,
    },
}

/// A mark's time, `None` for a reference that names no key or section.
#[must_use]
pub fn mark_time(sequence: &Sequence, mark: MarkRef) -> Option<f32> {
    match mark {
        MarkRef::Key(key) => key_time(sequence, key),
        MarkRef::SectionEnd { track, index, end } => {
            let spans = crate::sequencer::sections::section_spans(&sequence.tracks.get(track)?.keys)?;
            let span = spans.get(index)?;
            Some(if end { span.end } else { span.start })
        }
    }
}

/// The keys and section starts and ends of `tracks` (every track when empty) whose times are in
/// `[from, to]`: a box selection over the dope sheet, keys and section marks together.
#[must_use]
pub fn marks_in_box(sequence: &Sequence, tracks: &[usize], from: f32, to: f32) -> Vec<MarkRef> {
    let (lo, hi) = (from.min(to), from.max(to));
    let all: Vec<usize> = (0..sequence.tracks.len()).collect();
    let tracks: &[usize] = if tracks.is_empty() { &all } else { tracks };
    let mut out: Vec<MarkRef> = tracks
        .iter()
        .flat_map(|&track| {
            let keys: Vec<MarkRef> = key_times(sequence, track)
                .map(|times| {
                    times
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| **t >= lo && **t <= hi)
                        .map(|(index, _)| MarkRef::Key(KeyRef { track, index }))
                        .collect()
                })
                .unwrap_or_default();
            let section_marks = crate::sequencer::sections::section_spans(
                sequence.tracks.get(track).map_or(&EMPTY_CHANNEL, |t| &t.keys),
            )
            .unwrap_or_default()
            .into_iter()
            .flat_map(|s| {
                [
                    (s.start >= lo && s.start <= hi).then_some(false),
                    (s.end >= lo && s.end <= hi).then_some(true),
                ]
                .into_iter()
                .flatten()
                .map(move |end| MarkRef::SectionEnd {
                    track,
                    index: s.index,
                    end,
                })
            });
            keys.into_iter().chain(section_marks).collect::<Vec<_>>()
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Moves `marks` by `delta` seconds on one validated copy (ADR-8640): keys like
/// [`KeyBatch::Move`]; a section moves whole when both its marks are selected, else the selected
/// end trims (the content stays on the clock, as the section strip's edits do). A moved section
/// that would overlap a section of its track it did not overlap before is refused
/// (`seq_section_overlap`; an overlap the track already had, such as a cross-fade, may grow or
/// shrink), as is a key that would leave its range or land on another key: a refused move
/// changes nothing. Returns
/// the marks at their new places (a key may re-sort to a new index; a section keeps its index).
///
/// Errors: `seq_keys_empty`, `seq_keys_bad_track`, `seq_keys_bad_key`, `seq_keys_not_keyed` (a
/// section mark on a keyed track or a key on a section track), `seq_keys_out_of_range`,
/// `seq_keys_time_taken`, `seq_section_bad_index`, `seq_section_out_of_range`,
/// `seq_section_overlap`, `seq_keys_invalid`.
pub fn apply_mark_move(
    sequence: &mut Sequence,
    marks: &[MarkRef],
    delta: f32,
) -> Result<Vec<MarkRef>, EditError> {
    if marks.is_empty() {
        return refuse("seq_keys_empty", "no marks given");
    }
    if !delta.is_finite() {
        return refuse("seq_keys_out_of_range", format!("delta {delta} must be finite"));
    }
    apply_mark_map(sequence, marks, &|time| time + delta, None)
}

/// Scales `marks` about `pivot` by `factor` (`t' = pivot + (t - pivot) * factor`) on one validated
/// copy (w11, after ADR-8640): keys like [`KeyBatch::Scale`] (tangent slopes divided by `factor`);
/// a section with both its marks selected gets both ends scaled (its content stays on the clock
/// from its new start: the speed is unchanged, the section plays more or less of its content), a
/// section with one mark selected trims that end, as [`apply_mark_move`] does. The overlap and
/// range rules are [`apply_mark_move`]'s. Returns the marks at their new places.
///
/// Errors: `seq_keys_bad_scale` (a factor not greater than zero), `seq_keys_out_of_range` (a
/// non-finite pivot, a key that would go below zero), `seq_section_out_of_range` (a section end
/// that would go below zero), and those of [`apply_mark_move`].
pub fn apply_mark_scale(
    sequence: &mut Sequence,
    marks: &[MarkRef],
    pivot: f32,
    factor: f32,
) -> Result<Vec<MarkRef>, EditError> {
    if marks.is_empty() {
        return refuse("seq_keys_empty", "no marks given");
    }
    if !(factor.is_finite() && factor > 0.0) {
        return refuse(
            "seq_keys_bad_scale",
            format!("factor {factor} must be greater than zero"),
        );
    }
    if !pivot.is_finite() {
        return refuse("seq_keys_out_of_range", format!("pivot {pivot} must be finite"));
    }
    apply_mark_map(
        sequence,
        marks,
        &|time| pivot + (time - pivot) * factor,
        Some(factor),
    )
}

/// The shared body of [`apply_mark_move`] and [`apply_mark_scale`]: every selected mark goes to
/// `map(t)`; `slope` is the time scale (key tangents and a whole section's end), `None` for a move.
fn apply_mark_map(
    sequence: &mut Sequence,
    marks: &[MarkRef],
    map: &dyn Fn(f32) -> f32,
    slope: Option<f32>,
) -> Result<Vec<MarkRef>, EditError> {
    let mut next = sequence.clone();
    // Validate and split the marks; sections collect their selected ends per track.
    let mut keys: Vec<KeyRef> = Vec::new();
    let mut sections: BTreeMap<usize, BTreeMap<usize, (bool, bool)>> = BTreeMap::new();
    for mark in marks {
        match *mark {
            MarkRef::Key(key) => {
                let Some(track) = next.tracks.get(key.track) else {
                    return refuse(
                        "seq_keys_bad_track",
                        format!("no track {}: the sequence has {}", key.track, next.tracks.len()),
                    );
                };
                if !has_keys(&track.keys) {
                    return refuse(
                        "seq_keys_not_keyed",
                        format!(
                            "track `{}` is a {} track: it has sections, not keys",
                            track.name,
                            track.keys.kind()
                        ),
                    );
                }
                if key.index >= track.keys.len() {
                    return refuse(
                        "seq_keys_bad_key",
                        format!("no key {} on track `{}`", key.index, track.name),
                    );
                }
                keys.push(key);
            }
            MarkRef::SectionEnd { track, index, end } => {
                let Some(t) = next.tracks.get(track) else {
                    return refuse(
                        "seq_keys_bad_track",
                        format!("no track {}: the sequence has {}", track, next.tracks.len()),
                    );
                };
                if has_keys(&t.keys) {
                    return refuse(
                        "seq_keys_not_keyed",
                        format!(
                            "track `{}` is a {} track: it has keys, not sections",
                            t.name,
                            t.keys.kind()
                        ),
                    );
                }
                if index >= t.keys.len() {
                    return refuse(
                        "seq_section_bad_index",
                        format!(
                            "no section {index} on track `{}`: it has {}",
                            t.name,
                            t.keys.len()
                        ),
                    );
                }
                let ends = sections
                    .entry(track)
                    .or_default()
                    .entry(index)
                    .or_insert((false, false));
                if end {
                    ends.1 = true;
                } else {
                    ends.0 = true;
                }
            }
        }
    }
    // The keys first (the same path a plain key move takes).
    let mut moved: Vec<MarkRef> = Vec::new();
    if !keys.is_empty() {
        for (track, indices) in group(&next, &keys)? {
            let t = &mut next.tracks[track];
            let retimed = retime_channel(&mut t.keys, &t.name, &indices, map, slope)?;
            moved.extend(
                retimed
                    .into_iter()
                    .map(|index| MarkRef::Key(KeyRef { track, index })),
            );
        }
    }
    // Then the sections, one edit each on the same copy.
    for (track, ends) in &sections {
        for (&index, &(start, end)) in ends {
            let spans =
                crate::sequencer::sections::section_spans(&next.tracks[*track].keys).unwrap_or_default();
            let span = spans.iter().find(|s| s.index == index).map(|s| (s.start, s.end));
            let Some((start_at, end_at)) = span else {
                return refuse("seq_section_bad_index", format!("no section {index}"));
            };
            let edit = match (start, end) {
                (true, true) => crate::sequencer::sections::SectionEdit::Move {
                    index,
                    start: map(start_at),
                },
                (true, false) => crate::sequencer::sections::SectionEdit::TrimStart {
                    index,
                    start: map(start_at),
                },
                (false, true) => crate::sequencer::sections::SectionEdit::TrimEnd {
                    index,
                    end: map(end_at),
                },
                (false, false) => continue,
            };
            crate::sequencer::sections::apply_section_edit(&mut next.tracks[*track].keys, edit)?;
            // A scale stretches a whole section: its end goes to its own mapped time too.
            if start && end && slope.is_some() {
                crate::sequencer::sections::apply_section_edit(
                    &mut next.tracks[*track].keys,
                    crate::sequencer::sections::SectionEdit::TrimEnd {
                        index,
                        end: map(end_at),
                    },
                )?;
            }
            if start {
                moved.push(MarkRef::SectionEnd {
                    track: *track,
                    index,
                    end: false,
                });
            }
            if end {
                moved.push(MarkRef::SectionEnd {
                    track: *track,
                    index,
                    end: true,
                });
            }
        }
    }
    // A moved section may not overlap a section of its track it did not overlap before
    // (touching is fine). Sections may overlap (cross-fades, layered sounds), so an overlap the
    // track already had is not the move's to refuse, even when the move changes its length.
    let overlaps = |a: &crate::sequencer::sections::SectionSpan,
                    b: &crate::sequencer::sections::SectionSpan| {
        a.start < b.end && b.start < a.end
    };
    for (track, ends) in &sections {
        let spans = crate::sequencer::sections::section_spans(&next.tracks[*track].keys).unwrap_or_default();
        let old_spans =
            crate::sequencer::sections::section_spans(&sequence.tracks[*track].keys).unwrap_or_default();
        let overlapped = |a: usize, b: usize| {
            let find = |i: usize| old_spans.iter().find(|s| s.index == i);
            find(a).zip(find(b)).is_some_and(|(a, b)| overlaps(a, b))
        };
        for &index in ends.keys() {
            let Some(a) = spans.iter().find(|s| s.index == index) else {
                continue;
            };
            if spans
                .iter()
                .any(|b| b.index != index && overlaps(a, b) && !overlapped(index, b.index))
            {
                let name = &next.tracks[*track].name;
                return refuse(
                    "seq_section_overlap",
                    format!("the move would overlap two sections of `{name}`"),
                );
            }
        }
    }
    moved.sort_unstable();
    moved.dedup();
    if let Err(e) = next.validate() {
        return refuse("seq_keys_invalid", e.to_string());
    }
    *sequence = next;
    Ok(moved)
}

/// Keys (and sections) copied from a sequence ([`copy_keys`], [`copy_marks`]): per track, the keys
/// or whole sections with times relative to the earliest copied key or section start, so a paste
/// puts that mark at the paste time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyClip {
    /// One entry per source track, in track order.
    pub tracks: Vec<ClipTrack>,
}

impl KeyClip {
    /// The number of keys in the clip.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.iter().map(|t| t.keys.len()).sum()
    }

    /// `true` with no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The copied keys of one track.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipTrack {
    /// The source track's name (a paste looks for a track of this name first).
    pub name: String,
    /// The source track's binding.
    pub target: String,
    /// The source track's property path.
    pub property: String,
    /// The keys (a `Float`, `Vec3`, `Bool` or `Event` channel) or the whole sections (a `Clip`, `Audio`,
    /// `Sub` or `Video` channel, [`copy_marks`]), times relative to the clip's start.
    pub keys: Channel,
}

/// One batch edit of keys across tracks.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyBatch {
    /// Move every key in `keys` by `delta` seconds.
    Move {
        /// The keys.
        keys: Vec<KeyRef>,
        /// Seconds (negative moves earlier).
        delta: f32,
    },
    /// Scale every key's time in `keys` about `pivot` by `factor` (`t' = pivot + (t - pivot) *
    /// factor`). Tangent slopes of float and vec3 keys are divided by `factor`, so the curve
    /// keeps its shape stretched in time.
    Scale {
        /// The keys.
        keys: Vec<KeyRef>,
        /// Seconds on the sequence clock (the playhead in the editor).
        pivot: f32,
        /// Greater than zero.
        factor: f32,
    },
    /// Remove every key in `keys` (a track keeps at least one key).
    Delete {
        /// The keys.
        keys: Vec<KeyRef>,
    },
    /// Insert `clip`'s keys at `at` seconds (the earliest clip key lands at `at`). A clip track
    /// goes to the track of the same name and kind, else to the one with the same binding,
    /// property and kind. A pasted key at the time of an existing key replaces it (an event key
    /// goes after the keys at its time instead: events may share a time). A clip's
    /// sections paste as [`paste_marks`] does (only the keys are reported).
    Paste {
        /// The keys ([`copy_keys`]).
        clip: KeyClip,
        /// Seconds on the sequence clock.
        at: f32,
    },
}

/// A key's time, `None` for a reference that names no key of a track with keys (float, vec3,
/// bool or event).
#[must_use]
pub fn key_time(sequence: &Sequence, key: KeyRef) -> Option<f32> {
    match &sequence.tracks.get(key.track)?.keys {
        Channel::Float(k) => k.get(key.index).map(|k| k.time),
        Channel::Vec3(k) => k.get(key.index).map(|k| k.time),
        Channel::Bool(k) => k.get(key.index).map(|k| k.time),
        Channel::Event(k) => k.get(key.index).map(|k| k.time),
        _ => None,
    }
}

/// The key times of a track with keys, event tracks included (`None` for a section track or a bad
/// index).
#[must_use]
pub fn key_times(sequence: &Sequence, track: usize) -> Option<Vec<f32>> {
    match &sequence.tracks.get(track)?.keys {
        Channel::Float(k) => Some(k.iter().map(|k| k.time).collect()),
        Channel::Vec3(k) => Some(k.iter().map(|k| k.time).collect()),
        Channel::Bool(k) => Some(k.iter().map(|k| k.time).collect()),
        Channel::Event(k) => Some(k.iter().map(|k| k.time).collect()),
        _ => None,
    }
}

/// `time` rounded to the nearest frame of `fps` (unchanged for a non-positive or non-finite
/// `fps`).
#[must_use]
pub fn snap_to_frame(time: f32, fps: f32) -> f32 {
    if !(fps.is_finite() && fps > 0.0) {
        return time;
    }
    ((f64::from(time) * f64::from(fps)).round() / f64::from(fps)) as f32
}

/// Checks `keys` against `sequence` and groups them by track. Codes: `seq_keys_empty`,
/// `seq_keys_bad_track`, `seq_keys_not_keyed`, `seq_keys_bad_key`.
fn group(sequence: &Sequence, keys: &[KeyRef]) -> Result<BTreeMap<usize, BTreeSet<usize>>, EditError> {
    if keys.is_empty() {
        return refuse("seq_keys_empty", "no keys given");
    }
    let mut out: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for key in keys {
        let Some(track) = sequence.tracks.get(key.track) else {
            return refuse(
                "seq_keys_bad_track",
                format!(
                    "no track {}: the sequence has {}",
                    key.track,
                    sequence.tracks.len()
                ),
            );
        };
        if !has_keys(&track.keys) {
            return refuse(
                "seq_keys_not_keyed",
                format!(
                    "track `{}` is a {} track: it has sections, not keys",
                    track.name,
                    track.keys.kind()
                ),
            );
        }
        if key.index >= track.keys.len() {
            return refuse(
                "seq_keys_bad_key",
                format!(
                    "no key {} on track `{}`: it has {}",
                    key.index,
                    track.name,
                    track.keys.len()
                ),
            );
        }
        out.entry(key.track).or_default().insert(key.index);
    }
    Ok(out)
}

/// Key access the batch edits share across the three keyed kinds.
trait TimedKey: Clone {
    fn time(&self) -> f32;
    fn set_time(&mut self, time: f32);
    /// Divides explicit tangent slopes by `factor` (a time scale).
    fn scale_slopes(&mut self, factor: f32);
}

impl TimedKey for ScalarKey {
    fn time(&self) -> f32 {
        self.time
    }
    fn set_time(&mut self, time: f32) {
        self.time = time;
    }
    fn scale_slopes(&mut self, factor: f32) {
        self.tangent = match self.tangent {
            Tangent::Aligned(s) => Tangent::Aligned(s / factor),
            Tangent::Broken(a, b) => Tangent::Broken(a / factor, b / factor),
            other => other,
        };
    }
}

impl TimedKey for Vec3Key {
    fn time(&self) -> f32 {
        self.time
    }
    fn set_time(&mut self, time: f32) {
        self.time = time;
    }
    fn scale_slopes(&mut self, factor: f32) {
        let div = |s: [f32; 3]| s.map(|v| v / factor);
        self.tangent = match self.tangent {
            Tangent::Aligned(s) => Tangent::Aligned(div(s)),
            Tangent::Broken(a, b) => Tangent::Broken(div(a), div(b)),
            other => other,
        };
    }
}

impl TimedKey for BoolKey {
    fn time(&self) -> f32 {
        self.time
    }
    fn set_time(&mut self, time: f32) {
        self.time = time;
    }
    fn scale_slopes(&mut self, _factor: f32) {}
}

impl TimedKey for EventKey {
    fn time(&self) -> f32 {
        self.time
    }
    fn set_time(&mut self, time: f32) {
        self.time = time;
    }
    fn scale_slopes(&mut self, _factor: f32) {}
}

/// Gives the keys in `selected` a new time `map(t)` (slopes scaled by `slope` when given), keeps
/// the list sorted and returns the new indices of the selected keys. Two keys at one time are
/// refused unless `ties` (event keys: keys at one time fire in file order, and the stable sort
/// keeps their order). Codes: `seq_keys_out_of_range`, `seq_keys_time_taken`.
fn retime<K: TimedKey>(
    keys: &mut Vec<K>,
    name: &str,
    selected: &BTreeSet<usize>,
    map: impl Fn(f32) -> f32,
    slope: Option<f32>,
    ties: bool,
) -> Result<Vec<usize>, EditError> {
    let mut next: Vec<(K, bool)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.clone(), selected.contains(&i)))
        .collect();
    for (key, chosen) in &mut next {
        if !*chosen {
            continue;
        }
        let time = map(key.time());
        if !(time.is_finite() && time >= 0.0) {
            return refuse(
                "seq_keys_out_of_range",
                format!("a key of `{name}` would move to {time} s: key times are zero or more"),
            );
        }
        key.set_time(time);
        if let Some(factor) = slope {
            key.scale_slopes(factor);
        }
    }
    // Stable: equal times keep their order, and then are refused below.
    next.sort_by(|a, b| a.0.time().total_cmp(&b.0.time()));
    if !ties && let Some(pair) = next.windows(2).find(|w| w[0].0.time() == w[1].0.time()) {
        return refuse(
            "seq_keys_time_taken",
            format!("two keys of `{name}` would sit at {} s", pair[0].0.time()),
        );
    }
    let indices = next
        .iter()
        .enumerate()
        .filter_map(|(i, (_, chosen))| chosen.then_some(i))
        .collect();
    *keys = next.into_iter().map(|(k, _)| k).collect();
    Ok(indices)
}

fn retime_channel(
    channel: &mut Channel,
    name: &str,
    selected: &BTreeSet<usize>,
    map: impl Fn(f32) -> f32,
    slope: Option<f32>,
) -> Result<Vec<usize>, EditError> {
    match channel {
        Channel::Float(k) => retime(k, name, selected, map, slope, false),
        Channel::Vec3(k) => retime(k, name, selected, map, slope, false),
        Channel::Bool(k) => retime(k, name, selected, map, slope, false),
        Channel::Event(k) => retime(k, name, selected, map, slope, true),
        other => refuse(
            "seq_keys_not_keyed",
            format!(
                "track `{name}` is a {} track: it has sections, not keys",
                other.kind()
            ),
        ),
    }
}

fn remove_keys(channel: &mut Channel, selected: &BTreeSet<usize>) {
    fn drop_from<K>(keys: &mut Vec<K>, selected: &BTreeSet<usize>) {
        let mut i = 0;
        keys.retain(|_| {
            let keep = !selected.contains(&i);
            i += 1;
            keep
        });
    }
    match channel {
        Channel::Float(k) => drop_from(k, selected),
        Channel::Vec3(k) => drop_from(k, selected),
        Channel::Bool(k) => drop_from(k, selected),
        Channel::Event(k) => drop_from(k, selected),
        _ => {}
    }
}

/// Copies `keys` into a [`KeyClip`] (times relative to the earliest). Codes: those of the key
/// references (`seq_keys_empty`, `seq_keys_bad_track`, `seq_keys_not_keyed`, `seq_keys_bad_key`).
pub fn copy_keys(sequence: &Sequence, keys: &[KeyRef]) -> Result<KeyClip, EditError> {
    let groups = group(sequence, keys)?;
    let start = keys
        .iter()
        .filter_map(|k| key_time(sequence, *k))
        .fold(f32::INFINITY, f32::min);
    let mut clip = KeyClip::default();
    for (track_index, indices) in groups {
        let track = &sequence.tracks[track_index];
        let keys = match &track.keys {
            Channel::Float(k) => Channel::Float(
                indices
                    .iter()
                    .map(|&i| ScalarKey {
                        time: k[i].time - start,
                        ..k[i].clone()
                    })
                    .collect(),
            ),
            Channel::Vec3(k) => Channel::Vec3(
                indices
                    .iter()
                    .map(|&i| Vec3Key {
                        time: k[i].time - start,
                        ..k[i].clone()
                    })
                    .collect(),
            ),
            Channel::Bool(k) => Channel::Bool(
                indices
                    .iter()
                    .map(|&i| BoolKey {
                        time: k[i].time - start,
                        value: k[i].value,
                    })
                    .collect(),
            ),
            Channel::Event(k) => Channel::Event(
                indices
                    .iter()
                    .map(|&i| EventKey {
                        time: k[i].time - start,
                        ..k[i].clone()
                    })
                    .collect(),
            ),
            _ => continue,
        };
        clip.tracks.push(ClipTrack {
            name: track.name.clone(),
            target: track.target.clone(),
            property: track.property.clone(),
            keys,
        });
    }
    Ok(clip)
}

/// Copies `marks` into a [`KeyClip`] (w11, after ADR-8640): the selected keys, and every section
/// with either of its marks selected, whole; times relative to the earliest copied key or section
/// start. Codes: `seq_keys_empty`, `seq_keys_bad_track`, `seq_keys_bad_key`,
/// `seq_section_bad_index`.
pub fn copy_marks(sequence: &Sequence, marks: &[MarkRef]) -> Result<KeyClip, EditError> {
    if marks.is_empty() {
        return refuse("seq_keys_empty", "no marks given");
    }
    let mut keys: Vec<KeyRef> = Vec::new();
    let mut sections: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for mark in marks {
        match *mark {
            MarkRef::Key(key) => keys.push(key),
            MarkRef::SectionEnd { track, index, .. } => {
                let Some(t) = sequence.tracks.get(track) else {
                    return refuse(
                        "seq_keys_bad_track",
                        format!("no track {track}: the sequence has {}", sequence.tracks.len()),
                    );
                };
                if has_keys(&t.keys) || index >= t.keys.len() {
                    return refuse(
                        "seq_section_bad_index",
                        format!("no section {index} on track `{}`", t.name),
                    );
                }
                sections.entry(track).or_default().insert(index);
            }
        }
    }
    let groups = if keys.is_empty() {
        BTreeMap::new()
    } else {
        group(sequence, &keys)?
    };
    let key_start = keys
        .iter()
        .filter_map(|k| key_time(sequence, *k))
        .fold(f32::INFINITY, f32::min);
    let section_start = sections
        .iter()
        .flat_map(|(&track, picked)| {
            crate::sequencer::sections::section_spans(&sequence.tracks[track].keys)
                .unwrap_or_default()
                .into_iter()
                .filter(|span| picked.contains(&span.index))
                .map(|span| span.start)
        })
        .fold(f32::INFINITY, f32::min);
    let start = key_start.min(section_start);
    let mut by_track: BTreeMap<usize, ClipTrack> = BTreeMap::new();
    if !groups.is_empty() {
        let copied = copy_keys(sequence, &keys)?;
        // `copy_keys` measures from the earliest key; re-base on the clip's own start.
        let shift = key_start - start;
        for (track_index, mut clip) in groups.keys().zip(copied.tracks) {
            shift_keys(&mut clip.keys, shift);
            by_track.insert(*track_index, clip);
        }
    }
    for (&track, picked) in &sections {
        let t = &sequence.tracks[track];
        let mut channel = section_subset(&t.keys, picked);
        shift_sections(&mut channel, -start)?;
        by_track.insert(
            track,
            ClipTrack {
                name: t.name.clone(),
                target: t.target.clone(),
                property: t.property.clone(),
                keys: channel,
            },
        );
    }
    Ok(KeyClip {
        tracks: by_track.into_values().collect(),
    })
}

/// Adds `shift` seconds to every key of a channel with keys.
fn shift_keys(channel: &mut Channel, shift: f32) {
    fn add<K: TimedKey>(keys: &mut [K], shift: f32) {
        for key in keys {
            key.set_time(key.time() + shift);
        }
    }
    match channel {
        Channel::Float(k) => add(k, shift),
        Channel::Vec3(k) => add(k, shift),
        Channel::Bool(k) => add(k, shift),
        Channel::Event(k) => add(k, shift),
        _ => {}
    }
}

/// The sections of a section channel whose indices are in `picked`, in index order, as a channel
/// of the same kind.
fn section_subset(channel: &Channel, picked: &BTreeSet<usize>) -> Channel {
    fn pick<T: Clone>(list: &[T], picked: &BTreeSet<usize>) -> Vec<T> {
        list.iter()
            .enumerate()
            .filter(|(i, _)| picked.contains(i))
            .map(|(_, s)| s.clone())
            .collect()
    }
    match channel {
        Channel::Clip(l) => Channel::Clip(pick(l, picked)),
        Channel::Audio(l) => Channel::Audio(pick(l, picked)),
        Channel::Sub(l) => Channel::Sub(pick(l, picked)),
        Channel::Video(l) => Channel::Video(pick(l, picked)),
        keyed => keyed.clone(),
    }
}

/// Moves every section of a section channel by `delta` seconds (whole, content kept). Codes:
/// those of `sections::apply_section_edit` (`seq_section_out_of_range` below zero).
fn shift_sections(channel: &mut Channel, delta: f32) -> Result<(), EditError> {
    for span in crate::sequencer::sections::section_spans(channel).unwrap_or_default() {
        crate::sequencer::sections::apply_section_edit(
            channel,
            crate::sequencer::sections::SectionEdit::Move {
                index: span.index,
                start: span.start + delta,
            },
        )?;
    }
    Ok(())
}

/// The sections of a section channel as [`crate::sequencer::sections::Section`] values.
fn sections_of(channel: &Channel) -> Vec<crate::sequencer::sections::Section> {
    use crate::sequencer::sections::Section;
    match channel {
        Channel::Clip(l) => l.iter().cloned().map(Section::Clip).collect(),
        Channel::Audio(l) => l.iter().cloned().map(Section::Audio).collect(),
        Channel::Sub(l) => l.iter().cloned().map(Section::Sub).collect(),
        Channel::Video(l) => l.iter().cloned().map(Section::Video).collect(),
        Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => Vec::new(),
    }
}

/// Pastes `clip` at `at` seconds into a validated copy of `sequence` (w11, after ADR-8640): keys
/// as [`KeyBatch::Paste`] does, and each copied section whole, its start at `at` plus its offset in
/// the clip, appended to the track of the same name and kind (else the same binding, property and
/// kind). A pasted section may not overlap a section the track already has (touching is fine).
/// Returns the pasted marks (keys at their indices, both ends of each pasted section). Codes:
/// `seq_keys_empty` (an empty clip), `seq_keys_out_of_range` (a non-finite time, a key below
/// zero), `seq_keys_no_track`, `seq_section_overlap`, `seq_section_out_of_range`,
/// `seq_section_limit`, `seq_keys_invalid`.
pub fn paste_marks(sequence: &mut Sequence, clip: &KeyClip, at: f32) -> Result<Vec<MarkRef>, EditError> {
    let mut next = sequence.clone();
    let marks = paste_into(&mut next, clip, at)?;
    if let Err(e) = next.validate() {
        return refuse("seq_keys_invalid", e.to_string());
    }
    *sequence = next;
    Ok(marks)
}

/// The body of [`paste_marks`] on a copy the caller validates.
fn paste_into(next: &mut Sequence, clip: &KeyClip, at: f32) -> Result<Vec<MarkRef>, EditError> {
    if clip.is_empty() {
        return refuse("seq_keys_empty", "the clipboard holds no keys");
    }
    if !at.is_finite() {
        return refuse("seq_keys_out_of_range", format!("paste time {at} must be finite"));
    }
    let mut marks = Vec::new();
    for source in &clip.tracks {
        let track = paste_target(next, source)?;
        if has_keys(&source.keys) {
            let pasted = paste_track(&mut next.tracks[track].keys, &source.keys, at)?;
            marks.extend(
                pasted
                    .into_iter()
                    .map(|index| MarkRef::Key(KeyRef { track, index })),
            );
            continue;
        }
        let mut shifted = source.keys.clone();
        shift_sections(&mut shifted, at)?;
        let existing =
            crate::sequencer::sections::section_spans(&next.tracks[track].keys).unwrap_or_default();
        for (section, span) in sections_of(&shifted)
            .into_iter()
            .zip(crate::sequencer::sections::section_spans(&shifted).unwrap_or_default())
        {
            if existing.iter().any(|e| span.start < e.end && e.start < span.end) {
                return refuse(
                    "seq_section_overlap",
                    format!(
                        "a pasted section at {} s would overlap a section of `{}`",
                        span.start, next.tracks[track].name
                    ),
                );
            }
            let added = crate::sequencer::sections::apply_section_edit(
                &mut next.tracks[track].keys,
                crate::sequencer::sections::SectionEdit::Add(section),
            )?;
            if let Some(index) = added {
                marks.push(MarkRef::SectionEnd {
                    track,
                    index,
                    end: false,
                });
                marks.push(MarkRef::SectionEnd {
                    track,
                    index,
                    end: true,
                });
            }
        }
    }
    marks.sort_unstable();
    marks.dedup();
    Ok(marks)
}

/// The track a clip track pastes into: the same name and kind, else the same binding, property and
/// kind. Code: `seq_keys_no_track`.
fn paste_target(sequence: &Sequence, clip: &ClipTrack) -> Result<usize, EditError> {
    let kind = clip.keys.kind();
    sequence
        .tracks
        .iter()
        .position(|t| t.name == clip.name && t.keys.kind() == kind)
        .or_else(|| {
            sequence
                .tracks
                .iter()
                .position(|t| t.target == clip.target && t.property == clip.property && t.keys.kind() == kind)
        })
        .map_or_else(
            || {
                refuse(
                    "seq_keys_no_track",
                    format!(
                        "no {kind} track named `{}` or on `{}`.`{}` to paste into",
                        clip.name, clip.target, clip.property
                    ),
                )
            },
            Ok,
        )
}

/// Inserts `new` keys (already at their final times) into `keys`, replacing a key at an equal
/// time. Returns the final indices of the inserted keys.
fn insert_keys<K: TimedKey>(keys: &mut Vec<K>, new: Vec<K>) -> Vec<usize> {
    let times: Vec<f32> = new.iter().map(TimedKey::time).collect();
    for key in new {
        let time = key.time();
        if let Some(existing) = keys.iter_mut().find(|k| k.time() == time) {
            *existing = key;
        } else {
            let at = keys.iter().take_while(|k| k.time() < time).count();
            keys.insert(at, key);
        }
    }
    times
        .iter()
        .filter_map(|t| keys.iter().position(|k| k.time() == *t))
        .collect()
}

/// Inserts `new` event keys (already at their final times, in clip order) into `keys`, each after
/// the keys already at its time: event keys may share a time and fire in file order, so a paste
/// adds events and never replaces one. Returns the final indices of the inserted keys, in order.
fn insert_events(keys: &mut Vec<EventKey>, new: Vec<EventKey>) -> Vec<usize> {
    let mut all: Vec<(EventKey, bool)> = keys.drain(..).map(|k| (k, false)).collect();
    for key in new {
        let at = all.iter().take_while(|(k, _)| k.time <= key.time).count();
        all.insert(at, (key, true));
    }
    let indices = all
        .iter()
        .enumerate()
        .filter_map(|(i, (_, new))| new.then_some(i))
        .collect();
    *keys = all.into_iter().map(|(k, _)| k).collect();
    indices
}

fn paste_track(channel: &mut Channel, clip: &Channel, at: f32) -> Result<Vec<usize>, EditError> {
    let shift = |t: f32| {
        let time = at + t;
        if time.is_finite() && time >= 0.0 {
            Ok(time)
        } else {
            refuse(
                "seq_keys_out_of_range",
                format!("a pasted key would sit at {time} s"),
            )
        }
    };
    match (channel, clip) {
        (Channel::Float(keys), Channel::Float(new)) => {
            let new = new
                .iter()
                .map(|k| shift(k.time).map(|time| ScalarKey { time, ..k.clone() }))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(insert_keys(keys, new))
        }
        (Channel::Vec3(keys), Channel::Vec3(new)) => {
            let new = new
                .iter()
                .map(|k| shift(k.time).map(|time| Vec3Key { time, ..k.clone() }))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(insert_keys(keys, new))
        }
        (Channel::Bool(keys), Channel::Bool(new)) => {
            let new = new
                .iter()
                .map(|k| shift(k.time).map(|time| BoolKey { time, value: k.value }))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(insert_keys(keys, new))
        }
        (Channel::Event(keys), Channel::Event(new)) => {
            let new = new
                .iter()
                .map(|k| shift(k.time).map(|time| EventKey { time, ..k.clone() }))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(insert_events(keys, new))
        }
        (channel, clip) => refuse(
            "seq_keys_no_track",
            format!(
                "a {} clip cannot paste into a {} track",
                clip.kind(),
                channel.kind()
            ),
        ),
    }
}

/// Applies `batch` to a copy of `sequence`, validates the copy and swaps it in. Returns the keys
/// the edit leaves selected (the moved, scaled or pasted keys at their new indices; none after a
/// delete). Errors (nothing changed): `seq_keys_empty`, `seq_keys_bad_track`, `seq_keys_not_keyed`,
/// `seq_keys_bad_key`, `seq_keys_out_of_range` (a time below zero or not finite, a non-finite
/// delta), `seq_keys_bad_scale` (a factor not greater than zero), `seq_keys_time_taken` (two keys
/// of a track at one time), `seq_keys_last_key` (a delete would empty a track),
/// `seq_keys_no_track` (a paste found no matching track), `seq_keys_invalid` (the sequence fails
/// validation).
pub fn apply_key_batch(sequence: &mut Sequence, batch: &KeyBatch) -> Result<Vec<KeyRef>, EditError> {
    let mut next = sequence.clone();
    let mut selected = Vec::new();
    match batch {
        KeyBatch::Move { keys, delta } => {
            if !delta.is_finite() {
                return refuse("seq_keys_out_of_range", format!("delta {delta} must be finite"));
            }
            let delta = *delta;
            for (track, indices) in group(&next, keys)? {
                let t = &mut next.tracks[track];
                let moved = retime_channel(&mut t.keys, &t.name, &indices, |time| time + delta, None)?;
                selected.extend(moved.into_iter().map(|index| KeyRef { track, index }));
            }
        }
        KeyBatch::Scale { keys, pivot, factor } => {
            if !(factor.is_finite() && *factor > 0.0) {
                return refuse(
                    "seq_keys_bad_scale",
                    format!("factor {factor} must be greater than zero"),
                );
            }
            if !pivot.is_finite() {
                return refuse("seq_keys_out_of_range", format!("pivot {pivot} must be finite"));
            }
            let (pivot, factor) = (*pivot, *factor);
            for (track, indices) in group(&next, keys)? {
                let t = &mut next.tracks[track];
                let map = |time: f32| pivot + (time - pivot) * factor;
                let moved = retime_channel(&mut t.keys, &t.name, &indices, map, Some(factor))?;
                selected.extend(moved.into_iter().map(|index| KeyRef { track, index }));
            }
        }
        KeyBatch::Delete { keys } => {
            for (track, indices) in group(&next, keys)? {
                let t = &mut next.tracks[track];
                if indices.len() >= t.keys.len() {
                    return refuse(
                        "seq_keys_last_key",
                        format!(
                            "track `{}` keeps at least one key; remove the track instead",
                            t.name
                        ),
                    );
                }
                remove_keys(&mut t.keys, &indices);
            }
        }
        KeyBatch::Paste { clip, at } => {
            let marks = paste_into(&mut next, clip, *at)?;
            selected.extend(marks.into_iter().filter_map(|m| match m {
                MarkRef::Key(k) => Some(k),
                MarkRef::SectionEnd { .. } => None,
            }));
        }
    }
    if let Err(e) = next.validate() {
        return refuse("seq_keys_invalid", e.to_string());
    }
    *sequence = next;
    Ok(selected)
}

/// Sets marker `index`'s time and/or payload on a copy, validated (the timeline's marker drag and
/// payload field). Returns the index. Errors (nothing changed): `seq_marker_bad_index`,
/// `seq_marker_invalid` (a negative or non-finite time).
pub fn set_marker(
    sequence: &mut Sequence,
    index: usize,
    time: Option<f32>,
    payload: Option<&str>,
) -> Result<usize, EditError> {
    let mut next = sequence.clone();
    let count = next.markers.len();
    let Some(marker) = next.markers.get_mut(index) else {
        return refuse(
            "seq_marker_bad_index",
            format!("no marker {index}: the sequence has {count}"),
        );
    };
    if let Some(time) = time {
        if !(time.is_finite() && time >= 0.0) {
            return refuse("seq_marker_invalid", format!("time {time} must be zero or more"));
        }
        marker.time = time;
    }
    if let Some(payload) = payload {
        marker.payload = payload.to_string();
    }
    if let Err(e) = next.validate() {
        return refuse("seq_marker_invalid", e.to_string());
    }
    *sequence = next;
    Ok(index)
}

/// Replaces the markers from a marker CSV (`marker_sheet`), validated. Code: `seq_marker_csv`
/// (the row and what is wrong), `seq_marker_invalid`.
pub fn import_marker_csv(sequence: &mut Sequence, csv: &str) -> Result<usize, EditError> {
    let mut next = sequence.clone();
    apply_marker_csv(&mut next, csv).map_err(|e| EditError {
        code: "seq_marker_csv",
        message: e.to_string(),
    })?;
    if let Err(e) = next.validate() {
        return refuse("seq_marker_invalid", e.to_string());
    }
    *sequence = next;
    Ok(sequence.markers.len())
}

/// The camera a new cut starts from: a still pose (the selected or active camera's).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutPose {
    /// World position, metres.
    pub position: [f32; 3],
    /// World orientation, a unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Vertical field of view, degrees (1 to 170).
    pub fov_degrees: f32,
}

/// One edit of a shot list's cuts and fades. Cut `i` is the boundary between shot `i` and shot
/// `i + 1`, at [`ShotSequence::shot_start`]`(i + 1)`.
///
/// `ripple` (off by default in every variant that has it) makes the edit move everything after it
/// in time as well (a ripple edit, as film editors call it): with it on, [`ripple_span`] says
/// where the shift starts and by how much, and the caller shifts the sequence's later keys and
/// markers with [`ripple_shift`]. On the shot list itself: a rippling move keeps the second shot's
/// duration (so every later cut moves by the same delta); a trim already moves the later shots
/// (their starts sum the durations), and a cut add splits a shot in place, so its ripple delta is
/// zero.
#[derive(Clone, Debug, PartialEq)]
pub enum CutEdit {
    /// Cut at `time` seconds: the shot playing there ends at `time` (its motion up to the cut
    /// kept) and a new still shot of `pose` plays from `time` to where that shot ended.
    Add {
        /// Seconds on the shot list's clock, strictly inside a shot.
        time: f32,
        /// The new shot's camera.
        pose: CutPose,
        /// Shift everything after the cut (the split keeps the list's length, so the shift is
        /// zero seconds; the field is here so every edit that can ripple takes it).
        ripple: bool,
    },
    /// Move cut `cut` to `time`: shot `cut` ends and shot `cut + 1` starts there (each keeps its
    /// own clock from its start; keys past a shortened shot's end are cut at its end). With
    /// `ripple`, shot `cut + 1` keeps its duration and every later cut moves with it.
    Move {
        /// Cut index (`0..shots - 1`).
        cut: usize,
        /// Seconds, strictly after the first shot's start; without `ripple`, also strictly
        /// before the second shot's end.
        time: f32,
        /// Shift every later cut, and (through [`ripple_shift`]) the sequence's later keys and
        /// markers, by the move's delta.
        ripple: bool,
    },
    /// Set shot `shot`'s duration (the later shots move with its end). With `ripple`, the
    /// sequence's keys and markers after the shot's new end move by the same delta
    /// ([`ripple_shift`]).
    Trim {
        /// Shot index.
        shot: usize,
        /// Seconds, greater than zero.
        duration: f32,
        /// Shift the sequence's later keys and markers by the trim's delta.
        ripple: bool,
    },
    /// Remove cut `cut`: shot `cut` plays on for shot `cut + 1`'s duration, which is removed.
    Delete {
        /// Cut index.
        cut: usize,
    },
    /// Set shot `shot`'s fades (`None` keeps one).
    Fade {
        /// Shot index.
        shot: usize,
        /// Seconds from the fade color to the picture at the shot's start.
        fade_in: Option<f32>,
        /// Seconds from the picture to the fade color at the shot's end.
        fade_out: Option<f32>,
    },
}

/// The cut times of a shot list: the start of every shot after the first, seconds.
#[must_use]
pub fn cut_times(shots: &ShotSequence) -> Vec<f32> {
    (1..shots.shots.len())
        .map(|i| shots.shot_start(i) as f32)
        .collect()
}

/// A ripple edit's span: the edge the edit moves, at its place before the edit (`point`), and
/// the shift everything after it moves by (`delta`), read from the shot list the edit is about to
/// change. `None` when the edit does not ripple (the `ripple` flag is off, the edit's index is
/// bad, or the shift would be zero seconds): a cut `add` splits a shot in place, so its delta is
/// always zero, and a `trim` or a `move` shifts by the duration or cut change.
///
/// - `Trim { shot, duration }`: point the shot's old end `shot_start(shot) +
///   shots[shot].duration`, delta `duration - shots[shot].duration`.
/// - `Move { cut, time }`: point the cut's old time `shot_start(cut + 1)`, delta
///   `time - shot_start(cut + 1)`.
///
/// The point is the old edge whichever way the edge moves (ADR-8640, w9 review finding 3): the
/// keys and markers after it are the ones that played after the edge, so they keep their order
/// and their place after it; [`ripple_shift`] refuses a shortening edit whose removed span
/// (`point + delta`, `point`] holds a key or marker.
#[must_use]
pub fn ripple_span(shots: &ShotSequence, edit: &CutEdit) -> Option<(f32, f32)> {
    match *edit {
        CutEdit::Move { cut, time, ripple } => {
            if !ripple || cut + 2 > shots.shots.len() {
                return None;
            }
            let old = shots.shot_start(cut + 1) as f32;
            (time - old != 0.0).then_some((old, time - old))
        }
        CutEdit::Trim {
            shot,
            duration,
            ripple,
        } => {
            if !ripple || shot >= shots.shots.len() {
                return None;
            }
            let old = shots.shots[shot].duration;
            let delta = duration - old;
            (delta != 0.0).then(|| (shots.shot_start(shot) as f32 + old, delta))
        }
        CutEdit::Add { .. } | CutEdit::Delete { .. } | CutEdit::Fade { .. } => None,
    }
}

/// Shifts every key and marker of `sequence` that sits strictly after `point` by `delta` seconds
/// (the sequence half of a ripple edit, ADR-8640; `point` is the edge's place before the edit,
/// see [`ripple_span`]), and every clip, audio, sub-sequence and video section that starts
/// strictly after `point` (whole: its length and content kept; w11). A section that starts at or
/// before `point` keeps its span, even one that runs past it. A negative `delta` removes the span
/// (`point + delta`, `point`]: a key, marker or section start there would have to move before ones
/// that stay, so the shift is refused instead (`seq_ripple_keys_in_span`; move or delete those
/// first, or edit without ripple). Applied to a copy and validated whole, so a refusal changes
/// nothing. Errors: `seq_keys_out_of_range` (a non-finite point or delta, a key or marker that
/// would move below zero), `seq_ripple_keys_in_span`, `seq_keys_time_taken` (a moved key onto one
/// that stays), `seq_section_overlap` (a moved section onto a section of its track that stays and
/// that it did not overlap before), `seq_section_out_of_range`, `seq_keys_invalid` (the sequence
/// fails validation).
pub fn ripple_shift(sequence: &mut Sequence, point: f32, delta: f32) -> Result<(), EditError> {
    if !(point.is_finite() && delta.is_finite()) {
        return refuse(
            "seq_keys_out_of_range",
            format!("a ripple needs a finite point and delta, not {point} + {delta}"),
        );
    }
    if delta < 0.0 {
        let removed = |t: f32| t > point + delta && t <= point;
        let keyed = sequence.tracks.iter().find_map(|track| {
            key_times_of(&track.keys)
                .into_iter()
                .find(|t| removed(*t))
                .map(|t| format!("a key of `{}` at {t} s", track.name))
        });
        let marked = || {
            sequence
                .markers
                .iter()
                .find(|m| removed(m.time))
                .map(|m| format!("marker `{}` at {} s", m.name, m.time))
        };
        let sectioned = || {
            sequence.tracks.iter().find_map(|track| {
                crate::sequencer::sections::section_spans(&track.keys)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|span| removed(span.start))
                    .map(|span| format!("a section of `{}` starting at {} s", track.name, span.start))
            })
        };
        if let Some(what) = keyed.or_else(marked).or_else(sectioned) {
            return refuse(
                "seq_ripple_keys_in_span",
                format!(
                    "{what} is in the {} s the ripple removes (after {} s up to {point} s): move or \
                     delete it first, or edit without ripple",
                    -delta,
                    point + delta
                ),
            );
        }
    }
    let mut next = sequence.clone();
    for track in &mut next.tracks {
        if !has_keys(&track.keys) {
            continue;
        }
        let selected: BTreeSet<usize> = key_times_of(&track.keys)
            .into_iter()
            .enumerate()
            .filter(|(_, t)| *t > point)
            .map(|(i, _)| i)
            .collect();
        if selected.is_empty() {
            continue;
        }
        retime_channel(&mut track.keys, &track.name, &selected, |time| time + delta, None)?;
    }
    for marker in &mut next.markers {
        if marker.time > point {
            marker.time += delta;
        }
    }
    for (track, old) in next.tracks.iter_mut().zip(&sequence.tracks) {
        let Some(spans) = crate::sequencer::sections::section_spans(&old.keys) else {
            continue;
        };
        let moving: BTreeSet<usize> = spans
            .iter()
            .filter(|span| span.start > point)
            .map(|span| span.index)
            .collect();
        for span in spans.iter().filter(|span| moving.contains(&span.index)) {
            crate::sequencer::sections::apply_section_edit(
                &mut track.keys,
                crate::sequencer::sections::SectionEdit::Move {
                    index: span.index,
                    start: span.start + delta,
                },
            )?;
        }
        // A moved section may not newly overlap one that stays (touching is fine).
        let now = crate::sequencer::sections::section_spans(&track.keys).unwrap_or_default();
        let overlaps = |a: (f32, f32), b: (f32, f32)| a.0 < b.1 && b.0 < a.1;
        let at = |list: &[crate::sequencer::sections::SectionSpan], i: usize| {
            list.iter().find(|s| s.index == i).map(|s| (s.start, s.end))
        };
        for &moved in &moving {
            for stay in spans.iter().filter(|s| !moving.contains(&s.index)) {
                let (Some(a_now), Some(b), Some(a_old)) =
                    (at(&now, moved), at(&now, stay.index), at(&spans, moved))
                else {
                    continue;
                };
                if overlaps(a_now, b) && !overlaps(a_old, b) {
                    return refuse(
                        "seq_section_overlap",
                        format!(
                            "the ripple would move a section of `{}` onto another one",
                            track.name
                        ),
                    );
                }
            }
        }
    }
    if let Err(e) = next.validate() {
        return refuse("seq_keys_invalid", e.to_string());
    }
    *sequence = next;
    Ok(())
}

/// The key times of a channel with keys (event keys included) in key order (empty for a section
/// channel).
fn key_times_of(channel: &Channel) -> Vec<f32> {
    match channel {
        Channel::Float(k) => k.iter().map(|k| k.time).collect(),
        Channel::Vec3(k) => k.iter().map(|k| k.time).collect(),
        Channel::Bool(k) => k.iter().map(|k| k.time).collect(),
        Channel::Event(k) => k.iter().map(|k| k.time).collect(),
        _ => Vec::new(),
    }
}

/// A still shot of `pose` lasting `duration`.
#[must_use]
pub fn still_shot(name: &str, pose: CutPose, duration: f32) -> Shot {
    Shot {
        name: name.to_string(),
        duration,
        camera: ShotCamera {
            path: CameraPath {
                interp: PathInterp::Linear,
                keys: vec![PathKey {
                    time: 0.0,
                    position: pose.position,
                }],
                look_at: LookAt::Heading,
            },
            fov: ShotTrack(vec![crate::shots::ScalarKey {
                time: 0.0,
                value: pose.fov_degrees,
                ease: crate::shots::Ease::Linear,
            }]),
            roll: ShotTrack::default(),
            dof: None,
            rotation: vec![RotationKey {
                time: 0.0,
                rotation: pose.rotation,
                ease: crate::shots::Ease::Linear,
            }],
        },
        fade_in: 0.0,
        fade_out: 0.0,
        fade_color: [0.0; 3],
    }
}

/// A new shot list of one still shot of `pose` lasting `duration` at `fps` (what the timeline's
/// **Add cut** makes when the player has no shot list yet). Code: `seq_cut_invalid`.
pub fn new_shot_list(pose: CutPose, duration: f32, fps: f32) -> Result<ShotSequence, EditError> {
    let list = ShotSequence {
        fps,
        shots: vec![still_shot("shot1", pose, duration)],
        ..ShotSequence::default()
    };
    if let Err(e) = list.validate() {
        return refuse("seq_cut_invalid", e.to_string());
    }
    Ok(list)
}

/// Ends a scalar track at `end`: keys after `end` are dropped, with a key at `end` holding the
/// value the track had there when one was dropped.
fn trim_track(track: &mut ShotTrack, end: f32) {
    if track.0.iter().any(|k| k.time > end) {
        let value = track.sample(end);
        track.0.retain(|k| k.time <= end);
        if let Some(value) = value
            && track.0.last().is_none_or(|k| k.time < end)
        {
            track.0.push(crate::shots::ScalarKey {
                time: end,
                value,
                ease: crate::shots::Ease::Linear,
            });
        }
    }
}

/// Ends a keyed path list at `end` seconds. A Catmull-Rom path keeps its keys past the end as
/// control keys (ADR-8640): every segment before the cut keeps the control points it was drawn
/// with, so the shortened path samples exactly the curve of the unshortened one at every time
/// before the cut, and a later move or trim that gives the time back restores the original keys.
/// The shot's clock never reaches them. Other interpolations drop the keys past `end` and end
/// with a key at `end` holding `at_end`.
fn trim_path(keys: &mut Vec<PathKey>, end: f32, at_end: [f32; 3], interp: PathInterp) {
    if keys.iter().any(|k| k.time > end) {
        if interp == PathInterp::CatmullRom {
            return;
        }
        keys.retain(|k| k.time <= end);
        if keys.last().is_none_or(|k| k.time < end) {
            keys.push(PathKey {
                time: end,
                position: at_end,
            });
        }
    }
}

/// Ends a shot at `end` seconds of its own clock: every keyed track keeps its value at `end`
/// (keys past it dropped, one key added at `end`), the fades are shortened to fit.
fn trim_shot(shot: &mut Shot, end: f32) {
    let camera = &mut shot.camera;
    let interp = camera.path.interp;
    let eye = camera.path.position(end);
    let target = camera.path.target(end, eye);
    trim_path(&mut camera.path.keys, end, eye.to_array(), interp);
    if let LookAt::Track(keys) = &mut camera.path.look_at {
        trim_path(keys, end, target.to_array(), interp);
    }
    if camera.rotation.iter().any(|k| k.time > end) {
        let rotation = sample_rotation(&camera.rotation, end);
        camera.rotation.retain(|k| k.time <= end);
        if let Some(rotation) = rotation
            && camera.rotation.last().is_none_or(|k| k.time < end)
        {
            camera.rotation.push(RotationKey {
                time: end,
                rotation: rotation.to_array(),
                ease: crate::shots::Ease::Linear,
            });
        }
    }
    trim_track(&mut camera.fov, end);
    trim_track(&mut camera.roll, end);
    if let Some(dof) = &mut camera.dof {
        trim_track(&mut dof.focus_distance, end);
        trim_track(&mut dof.aperture, end);
    }
    shot.duration = end;
    shot.fade_in = shot.fade_in.min(end);
    shot.fade_out = shot.fade_out.min(end - shot.fade_in);
}

/// A shot name not used in `shots`: `cut<n>`.
fn free_shot_name(shots: &ShotSequence) -> String {
    let mut n = shots.shots.len() + 1;
    loop {
        let name = format!("cut{n}");
        if shots.shots.iter().all(|s| s.name != name) {
            return name;
        }
        n += 1;
    }
}

/// Applies `edit` to a copy of `shots`, validates it and swaps it in. Returns the index of the
/// edited shot (the new shot of an add, the earlier shot of a move or delete). Errors (nothing
/// changed): `seq_cut_bad_index`, `seq_cut_out_of_range` (a time outside the shot list or not
/// strictly inside the shots a move touches, a duration not greater than zero, a fade below
/// zero), `seq_cut_time_taken` (a cut already at that time), `seq_cut_invalid` (the list fails
/// validation: a fade longer than its shot, a field of view outside 1 to 170 degrees, a zero
/// quaternion).
pub fn apply_cut_edit(shots: &mut ShotSequence, edit: &CutEdit) -> Result<usize, EditError> {
    let mut next = shots.clone();
    let count = next.shots.len();
    let check_cut = |cut: usize| {
        if cut + 1 < count {
            Ok(())
        } else {
            refuse(
                "seq_cut_bad_index",
                format!("no cut {cut}: the shot list has {} cuts", count.saturating_sub(1)),
            )
        }
    };
    let check_shot = |shot: usize| {
        if shot < count {
            Ok(())
        } else {
            refuse(
                "seq_cut_bad_index",
                format!("no shot {shot}: the shot list has {count}"),
            )
        }
    };
    let index = match *edit {
        CutEdit::Add { time, pose, .. } => {
            let total = next.duration() as f32;
            if !(time.is_finite() && time > 0.0 && time < total) {
                return refuse(
                    "seq_cut_out_of_range",
                    format!("a cut goes strictly inside the shot list (0 to {total} s), not at {time}"),
                );
            }
            if cut_times(&next).contains(&time) {
                return refuse("seq_cut_time_taken", format!("a cut already sits at {time} s"));
            }
            let Some((shot, local)) = next.locate(f64::from(time)) else {
                return refuse("seq_cut_bad_index", "the shot list has no shots");
            };
            let rest = next.shots[shot].duration - local;
            let name = free_shot_name(&next);
            trim_shot(&mut next.shots[shot], local);
            next.shots.insert(shot + 1, still_shot(&name, pose, rest));
            shot + 1
        }
        CutEdit::Move { cut, time, ripple } => {
            check_cut(cut)?;
            let start = next.shot_start(cut) as f32;
            // A rippling move keeps the second shot's duration, so only the first shot's start
            // bounds it (w9 review finding 7); a plain move stays inside the two shots.
            let end = next.shot_start(cut + 2) as f32;
            if !(time.is_finite() && time > start && (ripple || time < end)) {
                let span = if ripple {
                    format!("after {start} s")
                } else {
                    format!("strictly between {start} and {end} s")
                };
                return refuse(
                    "seq_cut_out_of_range",
                    format!("cut {cut} moves {span}, not to {time}"),
                );
            }
            let first = time - start;
            if first < next.shots[cut].duration {
                trim_shot(&mut next.shots[cut], first);
            } else {
                next.shots[cut].duration = first;
            }
            let second = end - time;
            if ripple {
                // The second shot keeps its clock, so every later cut moves by the same delta
                // (their starts sum the durations).
            } else if second < next.shots[cut + 1].duration {
                trim_shot(&mut next.shots[cut + 1], second);
            } else {
                next.shots[cut + 1].duration = second;
            }
            cut
        }
        CutEdit::Trim {
            shot,
            duration,
            ripple: _,
        } => {
            check_shot(shot)?;
            if !(duration.is_finite() && duration > 0.0) {
                return refuse(
                    "seq_cut_out_of_range",
                    format!("duration {duration} must be greater than zero"),
                );
            }
            if duration < next.shots[shot].duration {
                trim_shot(&mut next.shots[shot], duration);
            } else {
                next.shots[shot].duration = duration;
            }
            shot
        }
        CutEdit::Delete { cut } => {
            check_cut(cut)?;
            let removed = next.shots.remove(cut + 1);
            next.shots[cut].duration += removed.duration;
            cut
        }
        CutEdit::Fade {
            shot,
            fade_in,
            fade_out,
        } => {
            check_shot(shot)?;
            for fade in [fade_in, fade_out].into_iter().flatten() {
                if !(fade.is_finite() && fade >= 0.0) {
                    return refuse(
                        "seq_cut_out_of_range",
                        format!("fade {fade} must be zero or more"),
                    );
                }
            }
            let s = &mut next.shots[shot];
            if let Some(f) = fade_in {
                s.fade_in = f;
            }
            if let Some(f) = fade_out {
                s.fade_out = f;
            }
            shot
        }
    };
    if let Err(e) = next.validate() {
        return refuse("seq_cut_invalid", e.to_string());
    }
    *shots = next;
    Ok(index)
}

/// The fade overlay's opacity over a shot list, sampled `samples` times evenly from 0 to the
/// end (`(time, opacity)`, opacity 0 = picture only, 1 = the fade color): the timeline's fade
/// curve.
#[must_use]
pub fn fade_curve(shots: &ShotSequence, samples: usize) -> Vec<(f32, f32)> {
    let total = shots.duration();
    let n = samples.max(2);
    (0..n)
        .filter_map(|i| {
            let t = total * i as f64 / (n - 1) as f64;
            shots.sample(t).map(|s| (t as f32, s.fade))
        })
        .collect()
}

#[cfg(test)]
mod tests;
