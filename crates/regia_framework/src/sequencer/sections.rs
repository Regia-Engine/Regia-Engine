// SPDX-License-Identifier: MPL-2.0
//! Editing the sections of clip, audio, sub-sequence and video tracks: the edits of the editor's section
//! strip (the dope sheet of the Sequence Curves panel, `regia_editor::seq_sections`, ADR-2135), as
//! plain data so the panel, its agent methods and tests run the same code.
//!
//! A section track holds sections that may overlap ([`Channel::Clip`], [`Channel::Audio`],
//! [`Channel::Sub`], [`Channel::Video`]). An edit changes one section in place and never reorders the others, so an
//! index stays valid across edits (adds append). A refused edit changes nothing and says why with
//! a lowercase agent error code ([`EditError::code`], `seq_section_*`). Every edit is checked with
//! the format's own validation ([`Track::validate`]) before it lands; the caller validates the
//! whole sequence afterwards ([`super::Sequence::validate`]).
//!
//! Trims keep the content where it was on the clock: trimming the start of a clip later by one
//! second at speed 2 moves its offset two clip seconds on, so the frames that were showing still
//! show at the same instants. A section made shorter than its ramps (blend or fade in plus out)
//! has both ramps scaled down by the same factor so they fit.
//!
//! [`section_spans`] lays the sections out in rows for drawing: each section takes the lowest row
//! free at its start, visiting sections by start time then index, so the layout is a pure
//! function of the sections.
//!
//! ```
//! use regia_framework::sequencer::sections::{SectionEdit, apply_section_edit, section_spans};
//! use regia_framework::sequencer::{AudioSection, Channel};
//!
//! let sound = |start: f32, end: f32| AudioSection::new("music", start, end);
//! let mut channel = Channel::Audio(vec![sound(0.0, 4.0)]);
//! apply_section_edit(&mut channel, SectionEdit::TrimStart { index: 0, start: 1.0 }).expect("trim");
//! let Channel::Audio(sections) = &channel else { unreachable!() };
//! assert_eq!((sections[0].start, sections[0].offset), (1.0, 1.0));
//! assert_eq!(section_spans(&channel).expect("sections")[0].row, 0);
//! ```

use super::asset::{
    AudioSection, AudioSpatial, Channel, ClipSection, MAX_SECTIONS, SubRepeat, SubSection, TimeScaleMode,
    Track, VideoSection, VideoTarget,
};
use super::edit::EditError;

/// One section of any section track kind (what [`SectionEdit::Add`] inserts).
#[derive(Clone, Debug, PartialEq)]
pub enum Section {
    /// An animation clip section.
    Clip(ClipSection),
    /// A sound section.
    Audio(AudioSection),
    /// A sub-sequence section.
    Sub(SubSection),
    /// A video section (v4).
    Video(VideoSection),
}

impl Section {
    /// The channel kind this section belongs to (`Clip`, `Audio`, `Sub` or `Video`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Clip(_) => "Clip",
            Self::Audio(_) => "Audio",
            Self::Sub(_) => "Sub",
            Self::Video(_) => "Video",
        }
    }
}

/// One section laid out for drawing (see [`section_spans`]).
#[derive(Clone, Debug, PartialEq)]
pub struct SectionSpan {
    /// Index of the section in its track.
    pub index: usize,
    /// Sequence time the section starts, seconds.
    pub start: f32,
    /// Sequence time the section ends, seconds.
    pub end: f32,
    /// Row of the strip (0 at the top); overlapping sections take different rows.
    pub row: usize,
    /// What the section plays: the clip, sound, sub-sequence or video name.
    pub label: String,
    /// The blend or fade in, seconds (zero for a sub-sequence).
    pub rise: f32,
    /// The blend or fade out, seconds (zero for a sub-sequence).
    pub fall: f32,
}

/// One edit of a section track.
#[derive(Clone, Debug, PartialEq)]
pub enum SectionEdit {
    /// Append a section (of the track's own kind).
    Add(Section),
    /// Append a copy of section `index` that starts at `start` (same length).
    Duplicate {
        /// Section index.
        index: usize,
        /// Sequence time the copy starts, seconds.
        start: f32,
    },
    /// Remove section `index` (a section track keeps at least one section).
    Remove {
        /// Section index.
        index: usize,
    },
    /// Move section `index` to start at `start`, keeping its length and what it plays.
    Move {
        /// Section index.
        index: usize,
        /// Sequence time the section starts, seconds.
        start: f32,
    },
    /// Move the start of section `index` to `start`, keeping its end and the content on the
    /// clock (the offset follows by the section's speed).
    TrimStart {
        /// Section index.
        index: usize,
        /// The new start, seconds (before the end).
        start: f32,
    },
    /// Move the end of section `index` to `end`, keeping its start.
    TrimEnd {
        /// Section index.
        index: usize,
        /// The new end, seconds (after the start).
        end: f32,
    },
    /// Set the speed of a clip or sub-sequence section (start and end stay).
    Speed {
        /// Section index.
        index: usize,
        /// Content seconds per sequence second, in `(0, 1000]`.
        speed: f32,
    },
    /// Set the content time at the section's start (clip or sub-sequence seconds; seconds into
    /// the sound, zero or more, for an audio section).
    Offset {
        /// Section index.
        index: usize,
        /// The offset, seconds.
        offset: f32,
    },
    /// Set the blend (clip) or fade (audio) in and out, seconds. Their sum must fit the section.
    /// Sub-sequence and video sections have neither.
    Ramps {
        /// Section index.
        index: usize,
        /// Seconds after the start.
        rise: f32,
        /// Seconds before the end.
        fall: f32,
    },
    /// Set a clip section's weight (in `(0, 1]`), or the volume of an audio section or of a video
    /// section's own sound (dB, at most 24).
    Gain {
        /// Section index.
        index: usize,
        /// The weight or the volume in dB.
        value: f32,
    },
    /// Set what section `index` plays: a clip section's clip, an audio section's sound, a
    /// sub-sequence section's sequence name or a video section's source path (not empty).
    Rename {
        /// Section index.
        index: usize,
        /// The clip, sound or sequence name.
        name: String,
    },
    /// Set an audio section's time scale (v3, in `(0, 16]`; varispeed: pitch follows, ADR-2405).
    /// The start and end stay; the offset stays (it is sound seconds at the start).
    TimeScale {
        /// Section index.
        index: usize,
        /// Sound seconds per sequence second.
        scale: f32,
    },
    /// Set how an audio section follows its time scale (v5, ADR-3197): varispeed, or keeping its
    /// pitch (a pitch-preserving time scale must be in 0.5..=2).
    TimeScaleMode {
        /// Section index.
        index: usize,
        /// Varispeed or pitch-preserving.
        mode: TimeScaleMode,
    },
    /// Place an audio section in 3D, or make it flat again with `None` (v3).
    Spatial {
        /// Section index.
        index: usize,
        /// Where it emits from and how it attenuates.
        spatial: Option<AudioSpatial>,
    },
    /// Set how many passes a sub-sequence section plays and the length of one pass (v3).
    Repeat {
        /// Section index.
        index: usize,
        /// Once, a count or until the section ends.
        repeat: SubRepeat,
        /// Sub-sequence seconds per pass (`None`: the sub-sequence's length after the offset).
        loop_length: Option<f32>,
    },
    /// Set (`Some`) or remove (`None`) one target rename of a sub-sequence section's `bindings`
    /// map (`from` is the sub-sequence's target name, `to` the name it binds as here; the map's
    /// key is `from`). Only the one section changes: the sequence-wide rename is
    /// [`super::rename::rename_binding`].
    SubBinding {
        /// Section index.
        index: usize,
        /// The sub-sequence target name (the map's key).
        from: String,
        /// The local binding name, or `None` to remove the entry.
        to: Option<String>,
    },
    /// Set where a video section's picture goes (v4).
    VideoTarget {
        /// Section index.
        index: usize,
        /// The target.
        target: VideoTarget,
    },
    /// Set whether a video section wraps over its source's length (v4).
    VideoLoop {
        /// Section index.
        index: usize,
        /// Wrap instead of holding the last frame.
        looping: bool,
    },
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// The timing fields every section kind shares, borrowed mutably.
struct Timing<'a> {
    start: &'a mut f32,
    end: &'a mut f32,
    offset: &'a mut f32,
    /// `None` for audio (its rate is the time scale, set by [`SectionEdit::TimeScale`]) and video
    /// (its rate is one).
    speed: Option<&'a mut f32>,
    /// Content seconds per sequence second when `speed` is `None` (an audio section's time scale,
    /// one for video).
    scale: f32,
    /// `None` for sub-sequences and video (no ramps).
    ramps: Option<(&'a mut f32, &'a mut f32)>,
}

fn timing_of(channel: &mut Channel, index: usize) -> Option<Timing<'_>> {
    match channel {
        Channel::Clip(s) => s.get_mut(index).map(|s| Timing {
            start: &mut s.start,
            end: &mut s.end,
            offset: &mut s.offset,
            speed: Some(&mut s.speed),
            scale: 1.0,
            ramps: Some((&mut s.blend_in, &mut s.blend_out)),
        }),
        Channel::Audio(s) => s.get_mut(index).map(|s| Timing {
            start: &mut s.start,
            end: &mut s.end,
            offset: &mut s.offset,
            speed: None,
            scale: s.time_scale,
            ramps: Some((&mut s.fade_in, &mut s.fade_out)),
        }),
        Channel::Sub(s) => s.get_mut(index).map(|s| Timing {
            start: &mut s.start,
            end: &mut s.end,
            offset: &mut s.offset,
            speed: Some(&mut s.speed),
            scale: 1.0,
            ramps: None,
        }),
        Channel::Video(s) => s.get_mut(index).map(|s| Timing {
            start: &mut s.start,
            end: &mut s.end,
            offset: &mut s.offset,
            speed: None,
            scale: 1.0,
            ramps: None,
        }),
        Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => None,
    }
}

/// Scales `rise` and `fall` down by one factor so their sum fits `length` (no change when it
/// already fits).
fn fit_ramps(rise: &mut f32, fall: &mut f32, length: f32) {
    let sum = *rise + *fall;
    if sum > length && sum > 0.0 {
        let k = length / sum;
        *rise *= k;
        *fall *= k;
    }
}

/// Applies `edit` to a clip, audio, sub-sequence or video channel. Returns the index of the section the
/// edit leaves selected (`None` after a removal). Errors (nothing changed):
/// `seq_section_not_sections` (a float, vec3 or bool channel), `seq_section_bad_index`,
/// `seq_section_wrong_kind` (an added section of another kind, a speed on an audio or video
/// section, a gain on a sub-sequence, ramps on a sub-sequence or video section), `seq_section_out_of_range` (a negative,
/// reversed or non-finite time, a speed, weight, volume or ramp outside its range, an empty name,
/// or anything else the format's validation refuses), `seq_section_last`, `seq_section_limit` (more than
/// [`MAX_SECTIONS`]), and for [`SectionEdit::SubBinding`]: `seq_binding_empty`, `seq_binding_same`
/// and `seq_binding_unknown` (removing a target the section does not bind).
pub fn apply_section_edit(channel: &mut Channel, edit: SectionEdit) -> Result<Option<usize>, EditError> {
    if channel.is_keyed() {
        return refuse(
            "seq_section_not_sections",
            format!(
                "a {} track has keys, not sections; only Clip, Audio, Sub and Video tracks do",
                channel.kind()
            ),
        );
    }
    let mut edited = channel.clone();
    let selected = edit_sections(&mut edited, edit)?;
    // The format's own rules, on a stand-in track (the target only has to be non-empty).
    let check = Track {
        name: "section".into(),
        target: "target".into(),
        property: String::new(),
        keys: edited,
        enabled: true,
        locked: false,
    };
    if let Err(e) = check.validate() {
        return refuse("seq_section_out_of_range", e.to_string());
    }
    *channel = check.keys;
    Ok(selected)
}

fn edit_sections(channel: &mut Channel, edit: SectionEdit) -> Result<Option<usize>, EditError> {
    let len = channel.len();
    let check_index = |index: usize| {
        if index < len {
            Ok(())
        } else {
            refuse(
                "seq_section_bad_index",
                format!("no section {index}: the track has {len}"),
            )
        }
    };
    let check_time = |what: &str, t: f32| {
        if t.is_finite() && t >= 0.0 {
            Ok(())
        } else {
            refuse(
                "seq_section_out_of_range",
                format!("{what} {t} must be zero or more"),
            )
        }
    };
    let room = || {
        if len >= MAX_SECTIONS {
            refuse(
                "seq_section_limit",
                format!("the track already has {len} sections; the limit is {MAX_SECTIONS}"),
            )
        } else {
            Ok(())
        }
    };
    match edit {
        SectionEdit::Add(section) => {
            room()?;
            let kind = section.kind();
            match (channel, section) {
                (Channel::Clip(list), Section::Clip(s)) => list.push(s),
                (Channel::Audio(list), Section::Audio(s)) => list.push(s),
                (Channel::Sub(list), Section::Sub(s)) => list.push(s),
                (Channel::Video(list), Section::Video(s)) => list.push(s),
                (other, _) => {
                    return refuse(
                        "seq_section_wrong_kind",
                        format!("a {kind} section does not go on a {} track", other.kind()),
                    );
                }
            }
            Ok(Some(len))
        }
        SectionEdit::Duplicate { index, start } => {
            check_index(index)?;
            check_time("start", start)?;
            room()?;
            match channel {
                Channel::Clip(list) => {
                    let mut s = list[index].clone();
                    (s.end, s.start) = (start + (s.end - s.start), start);
                    list.push(s);
                }
                Channel::Audio(list) => {
                    let mut s = list[index].clone();
                    (s.end, s.start) = (start + (s.end - s.start), start);
                    list.push(s);
                }
                Channel::Sub(list) => {
                    let mut s = list[index].clone();
                    (s.end, s.start) = (start + (s.end - s.start), start);
                    list.push(s);
                }
                Channel::Video(list) => {
                    let mut s = list[index].clone();
                    (s.end, s.start) = (start + (s.end - s.start), start);
                    list.push(s);
                }
                Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => {}
            }
            Ok(Some(len))
        }
        SectionEdit::Remove { index } => {
            check_index(index)?;
            if len == 1 {
                return refuse(
                    "seq_section_last",
                    "a section track keeps at least one section; disable the track instead",
                );
            }
            match channel {
                Channel::Clip(list) => {
                    list.remove(index);
                }
                Channel::Audio(list) => {
                    list.remove(index);
                }
                Channel::Sub(list) => {
                    list.remove(index);
                }
                Channel::Video(list) => {
                    list.remove(index);
                }
                Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => {}
            }
            Ok(None)
        }
        SectionEdit::Gain { index, value } => {
            check_index(index)?;
            if !value.is_finite() {
                return refuse("seq_section_out_of_range", format!("gain {value} must be finite"));
            }
            match channel {
                Channel::Clip(list) => list[index].weight = value,
                Channel::Audio(list) => list[index].volume_db = value,
                Channel::Video(list) => list[index].volume_db = value,
                _ => {
                    return refuse(
                        "seq_section_wrong_kind",
                        "a sub-sequence section has no gain (its own tracks carry theirs)",
                    );
                }
            }
            Ok(Some(index))
        }
        SectionEdit::Rename { index, name } => {
            check_index(index)?;
            match channel {
                Channel::Clip(list) => list[index].clip = name,
                Channel::Audio(list) => list[index].sound = name,
                Channel::Sub(list) => list[index].sequence = name,
                Channel::Video(list) => list[index].video = name,
                Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => {}
            }
            Ok(Some(index))
        }
        SectionEdit::TimeScale { index, scale } => {
            check_index(index)?;
            let Channel::Audio(list) = channel else {
                return refuse(
                    "seq_section_wrong_kind",
                    "only an audio section has a time scale (clips and sub-sequences have a speed)",
                );
            };
            list[index].time_scale = scale;
            Ok(Some(index))
        }
        SectionEdit::TimeScaleMode { index, mode } => {
            check_index(index)?;
            let Channel::Audio(list) = channel else {
                return refuse(
                    "seq_section_wrong_kind",
                    "only an audio section has a time scale mode",
                );
            };
            list[index].time_scale_mode = mode;
            Ok(Some(index))
        }
        SectionEdit::Spatial { index, spatial } => {
            check_index(index)?;
            let Channel::Audio(list) = channel else {
                return refuse("seq_section_wrong_kind", "only an audio section plays in 3D");
            };
            list[index].spatial = spatial;
            Ok(Some(index))
        }
        SectionEdit::Repeat {
            index,
            repeat,
            loop_length,
        } => {
            check_index(index)?;
            let Channel::Sub(list) = channel else {
                return refuse("seq_section_wrong_kind", "only a sub-sequence section repeats");
            };
            list[index].repeat = repeat;
            list[index].loop_length = loop_length;
            Ok(Some(index))
        }
        SectionEdit::VideoTarget { index, target } => {
            check_index(index)?;
            let Channel::Video(list) = channel else {
                return refuse(
                    "seq_section_wrong_kind",
                    "only a video section has a video target",
                );
            };
            list[index].target = target;
            Ok(Some(index))
        }
        SectionEdit::VideoLoop { index, looping } => {
            check_index(index)?;
            let Channel::Video(list) = channel else {
                return refuse(
                    "seq_section_wrong_kind",
                    "only a video section has this loop switch (a clip's is part of the clip section)",
                );
            };
            list[index].looping = looping;
            Ok(Some(index))
        }
        SectionEdit::SubBinding { index, from, to } => {
            check_index(index)?;
            let Channel::Sub(list) = channel else {
                return refuse(
                    "seq_section_wrong_kind",
                    "only a sub-sequence section has target renames (its `bindings` map)",
                );
            };
            let section = &mut list[index];
            match to {
                Some(to) => {
                    if from.is_empty() || to.is_empty() {
                        return refuse("seq_binding_empty", "binding names must not be empty");
                    }
                    if from == to {
                        return refuse("seq_binding_same", format!("`{from}` is already called `{to}`"));
                    }
                    section.bindings.insert(from, to);
                }
                None => {
                    if section.bindings.remove(&from).is_none() {
                        return refuse(
                            "seq_binding_unknown",
                            format!("the section binds no sub-sequence target `{from}`"),
                        );
                    }
                }
            }
            Ok(Some(index))
        }
        SectionEdit::Move { index, .. }
        | SectionEdit::TrimStart { index, .. }
        | SectionEdit::TrimEnd { index, .. }
        | SectionEdit::Speed { index, .. }
        | SectionEdit::Offset { index, .. }
        | SectionEdit::Ramps { index, .. } => {
            check_index(index)?;
            let kind = channel.kind();
            let Some(t) = timing_of(channel, index) else {
                return refuse("seq_section_bad_index", format!("no section {index}"));
            };
            edit_timing(t, kind, &edit, check_time)?;
            Ok(Some(index))
        }
    }
}

fn edit_timing(
    t: Timing<'_>,
    kind: &str,
    edit: &SectionEdit,
    check_time: impl Fn(&str, f32) -> Result<(), EditError>,
) -> Result<(), EditError> {
    let Timing {
        start,
        end,
        offset,
        speed,
        scale,
        ramps,
    } = t;
    let rate = speed.as_deref().copied().unwrap_or(scale);
    match *edit {
        SectionEdit::Move { start: to, .. } => {
            check_time("start", to)?;
            let length = *end - *start;
            *start = to;
            *end = to + length;
        }
        SectionEdit::TrimStart { start: to, .. } => {
            check_time("start", to)?;
            if to >= *end {
                return refuse(
                    "seq_section_out_of_range",
                    format!("start {to} must come before the end {}", *end),
                );
            }
            *offset += (to - *start) * rate;
            *start = to;
        }
        SectionEdit::TrimEnd { end: to, .. } => {
            check_time("end", to)?;
            if to <= *start {
                return refuse(
                    "seq_section_out_of_range",
                    format!("end {to} must come after the start {}", *start),
                );
            }
            *end = to;
        }
        SectionEdit::Speed { speed: to, .. } => {
            let Some(speed) = speed else {
                return refuse(
                    "seq_section_wrong_kind",
                    format!("a {kind} section has no speed (sounds and videos play at their own rate)"),
                );
            };
            *speed = to;
        }
        SectionEdit::Offset { offset: to, .. } => {
            if !to.is_finite() {
                return refuse("seq_section_out_of_range", format!("offset {to} must be finite"));
            }
            *offset = to;
        }
        SectionEdit::Ramps { rise, fall, .. } => {
            let Some((r, f)) = ramps else {
                return refuse(
                    "seq_section_wrong_kind",
                    format!("a {kind} section has no blend or fade"),
                );
            };
            (*r, *f) = (rise, fall);
            // Too long is refused (by the format check), not fitted: the caller asked for these.
            return Ok(());
        }
        SectionEdit::Add(_)
        | SectionEdit::Duplicate { .. }
        | SectionEdit::Remove { .. }
        | SectionEdit::Gain { .. }
        | SectionEdit::Rename { .. }
        | SectionEdit::TimeScale { .. }
        | SectionEdit::TimeScaleMode { .. }
        | SectionEdit::Spatial { .. }
        | SectionEdit::Repeat { .. }
        | SectionEdit::VideoTarget { .. }
        | SectionEdit::VideoLoop { .. }
        | SectionEdit::SubBinding { .. } => {}
    }
    if let Some((r, f)) = ramps {
        fit_ramps(r, f, *end - *start);
    }
    Ok(())
}

/// The sections of a clip, audio, sub-sequence or video channel laid out in rows (`None` for a keyed
/// channel), in index order.
#[must_use]
pub fn section_spans(channel: &Channel) -> Option<Vec<SectionSpan>> {
    let mut spans: Vec<SectionSpan> = match channel {
        Channel::Clip(list) => list
            .iter()
            .enumerate()
            .map(|(index, s)| span(index, s.start, s.end, &s.clip, s.blend_in, s.blend_out))
            .collect(),
        Channel::Audio(list) => list
            .iter()
            .enumerate()
            .map(|(index, s)| span(index, s.start, s.end, &s.sound, s.fade_in, s.fade_out))
            .collect(),
        Channel::Sub(list) => list
            .iter()
            .enumerate()
            .map(|(index, s)| span(index, s.start, s.end, &s.sequence, 0.0, 0.0))
            .collect(),
        Channel::Video(list) => list
            .iter()
            .enumerate()
            .map(|(index, s)| span(index, s.start, s.end, &s.video, 0.0, 0.0))
            .collect(),
        Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => return None,
    };
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&a, &b| spans[a].start.total_cmp(&spans[b].start).then(a.cmp(&b)));
    // The end time of the last section on each row.
    let mut rows: Vec<f32> = Vec::new();
    for i in order {
        let (start, end) = (spans[i].start, spans[i].end);
        let row = match rows.iter().position(|&last| last <= start) {
            Some(row) => row,
            None => {
                rows.push(f32::NEG_INFINITY);
                rows.len() - 1
            }
        };
        rows[row] = end;
        spans[i].row = row;
    }
    Some(spans)
}

fn span(index: usize, start: f32, end: f32, label: &str, rise: f32, fall: f32) -> SectionSpan {
    SectionSpan {
        index,
        start,
        end,
        row: 0,
        label: label.to_string(),
        rise,
        fall,
    }
}

/// `time` rounded to the nearest frame at `fps` (unchanged for a non-positive or non-finite
/// `fps`). The section strip snaps drags with it.
#[must_use]
pub fn snap_to_frame(time: f32, fps: f32) -> f32 {
    if !(fps.is_finite() && fps > 0.0) {
        return time;
    }
    (time * fps).round() / fps
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn clip(start: f32, end: f32) -> ClipSection {
        ClipSection {
            clip: "walk".into(),
            start,
            end,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        }
    }

    fn sound(start: f32, end: f32) -> AudioSection {
        AudioSection::new("music", start, end)
    }

    fn sub(start: f32, end: f32) -> SubSection {
        SubSection {
            bindings: BTreeMap::new(),
            ..SubSection::new("fx.seq.ron", start, end)
        }
    }

    fn code(r: Result<Option<usize>, EditError>) -> &'static str {
        r.expect_err("refused").code
    }

    fn video(start: f32, end: f32) -> VideoSection {
        VideoSection::new("movies/a.webm", start, end)
    }

    /// A trim keeps the frames on the clock, and the video-only edits land or refuse like the
    /// others.
    #[test]
    fn video_sections_edit_like_sound_sections_and_refuse_what_they_do_not_have() {
        let source_time_at = |c: &Channel, t: f64| {
            let Channel::Video(list) = c else { unreachable!() };
            list[0].source_time(t)
        };
        let mut c = Channel::Video(vec![VideoSection {
            offset: 0.5,
            ..video(1.0, 4.0)
        }]);
        let before = source_time_at(&c, 3.0);
        apply_section_edit(&mut c, SectionEdit::TrimStart { index: 0, start: 2.0 }).expect("trim");
        assert_eq!(
            source_time_at(&c, 3.0),
            before,
            "the frame at 3 s still shows at 3 s"
        );
        apply_section_edit(&mut c, SectionEdit::TrimEnd { index: 0, end: 3.5 }).expect("trim end");
        apply_section_edit(&mut c, SectionEdit::Move { index: 0, start: 5.0 }).expect("move");
        apply_section_edit(
            &mut c,
            SectionEdit::Offset {
                index: 0,
                offset: 2.0,
            },
        )
        .expect("offset");
        apply_section_edit(
            &mut c,
            SectionEdit::Gain {
                index: 0,
                value: -12.0,
            },
        )
        .expect("gain");
        apply_section_edit(
            &mut c,
            SectionEdit::Rename {
                index: 0,
                name: "movies/b.webm".into(),
            },
        )
        .expect("rename");
        apply_section_edit(
            &mut c,
            SectionEdit::VideoTarget {
                index: 0,
                target: VideoTarget::UiImage("slate".into()),
            },
        )
        .expect("target");
        apply_section_edit(
            &mut c,
            SectionEdit::VideoLoop {
                index: 0,
                looping: true,
            },
        )
        .expect("loop");
        assert_eq!(
            c,
            Channel::Video(vec![VideoSection {
                video: "movies/b.webm".into(),
                start: 5.0,
                end: 6.5,
                offset: 2.0,
                target: VideoTarget::UiImage("slate".into()),
                volume_db: -12.0,
                looping: true,
            }])
        );
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::Duplicate { index: 0, start: 7.0 }).expect("copy"),
            Some(1)
        );
        assert_eq!(section_spans(&c).expect("spans")[1].label, "movies/b.webm");
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::Add(Section::Video(video(0.0, 1.0)))).expect("add"),
            Some(2)
        );
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::Remove { index: 2 }).expect("remove"),
            None
        );

        let same = c.clone();
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::Speed { index: 0, speed: 2.0 }
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::Ramps {
                    index: 0,
                    rise: 0.1,
                    fall: 0.1
                }
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::Gain {
                    index: 0,
                    value: 30.0
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::VideoTarget {
                    index: 0,
                    target: VideoTarget::Player(String::new()),
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::Add(Section::Audio(sound(0.0, 1.0)))
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(c, same, "a refused edit changes nothing");

        let mut a = Channel::Audio(vec![sound(0.0, 1.0)]);
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::VideoLoop {
                    index: 0,
                    looping: true
                }
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::VideoTarget {
                    index: 0,
                    target: VideoTarget::default(),
                }
            )),
            "seq_section_wrong_kind"
        );
    }

    /// A sub-sequence section's target renames (`bindings`) are edited per section: a set writes
    /// the pair into that section's map alone, a remove takes one out, and the other section
    /// kinds and the refusal codes behave (w8-editor-smalls-z).
    #[test]
    fn sub_section_bindings_set_and_remove_per_section() {
        let mut c = Channel::Sub(vec![sub(0.0, 2.0), sub(2.0, 4.0)]);
        let set = |from: &str, to: &str| SectionEdit::SubBinding {
            index: 0,
            from: from.into(),
            to: Some(to.into()),
        };
        assert_eq!(apply_section_edit(&mut c, set("lamp", "hero")), Ok(Some(0)));
        let Channel::Sub(list) = &c else { unreachable!() };
        assert_eq!(
            list[0].bindings.get("lamp").map(String::as_str),
            Some("hero"),
            "the section's map holds the pair"
        );
        assert!(list[1].bindings.is_empty(), "other sections are untouched");
        // Re-setting the same key retargets it; a second pair lands beside the first.
        apply_section_edit(&mut c, set("lamp", "guard")).expect("retarget");
        apply_section_edit(&mut c, set("bell", "hero")).expect("second pair");
        let Channel::Sub(list) = &c else { unreachable!() };
        assert_eq!(
            (
                list[0].bindings.get("lamp").map(String::as_str),
                list[0].bindings.get("bell").map(String::as_str)
            ),
            (Some("guard"), Some("hero"))
        );
        // Remove: the key goes, the other pair stays; a missing key is refused.
        assert_eq!(
            apply_section_edit(
                &mut c,
                SectionEdit::SubBinding {
                    index: 0,
                    from: "bell".into(),
                    to: None,
                }
            ),
            Ok(Some(0))
        );
        let Channel::Sub(list) = &c else { unreachable!() };
        assert!(!list[0].bindings.contains_key("bell"), "removed");
        assert_eq!(list[0].bindings.get("lamp").map(String::as_str), Some("guard"));

        let same = c.clone();
        assert_eq!(
            code(apply_section_edit(&mut c, set("", "hero"))),
            "seq_binding_empty"
        );
        assert_eq!(
            code(apply_section_edit(&mut c, set("lamp", ""))),
            "seq_binding_empty"
        );
        assert_eq!(
            code(apply_section_edit(&mut c, set("lamp", "lamp"))),
            "seq_binding_same"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::SubBinding {
                    index: 0,
                    from: "nobody".into(),
                    to: None,
                }
            )),
            "seq_binding_unknown"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::SubBinding {
                    index: 2,
                    from: "lamp".into(),
                    to: Some("hero".into()),
                }
            )),
            "seq_section_bad_index"
        );
        assert_eq!(c, same, "a refused edit changes nothing");

        // Only a sub-sequence section has a bindings table.
        let mut a = Channel::Audio(vec![sound(0.0, 1.0)]);
        assert_eq!(
            code(apply_section_edit(&mut a, set("lamp", "hero"))),
            "seq_section_wrong_kind"
        );
    }

    #[test]
    fn moves_and_trims_keep_the_content_on_the_clock() {
        let mut c = Channel::Clip(vec![ClipSection {
            speed: 2.0,
            offset: 0.5,
            ..clip(1.0, 4.0)
        }]);
        let clip_time_at = |c: &Channel, t: f32| {
            let Channel::Clip(s) = c else { unreachable!() };
            s[0].offset + (t - s[0].start) * s[0].speed
        };
        let before = clip_time_at(&c, 3.0);
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::TrimStart { index: 0, start: 2.0 }),
            Ok(Some(0))
        );
        assert_eq!(clip_time_at(&c, 3.0), before, "the frame at 3 s did not change");
        let Channel::Clip(s) = &c else { unreachable!() };
        assert_eq!((s[0].start, s[0].end, s[0].offset), (2.0, 4.0, 2.5));
        apply_section_edit(&mut c, SectionEdit::TrimEnd { index: 0, end: 5.0 }).expect("trim end");
        apply_section_edit(&mut c, SectionEdit::Move { index: 0, start: 0.0 }).expect("move");
        let Channel::Clip(s) = &c else { unreachable!() };
        assert_eq!(
            (s[0].start, s[0].end, s[0].offset),
            (0.0, 3.0, 2.5),
            "a move keeps the offset"
        );
        // An audio trim moves the offset by sound seconds (no speed).
        let mut a = Channel::Audio(vec![sound(1.0, 3.0)]);
        apply_section_edit(&mut a, SectionEdit::TrimStart { index: 0, start: 1.5 }).expect("trim");
        let Channel::Audio(s) = &a else { unreachable!() };
        assert_eq!(s[0].offset, 0.5);
        // Trimming an audio start earlier than its sound starts is refused.
        let mut early = Channel::Audio(vec![sound(1.0, 3.0)]);
        assert_eq!(
            code(apply_section_edit(
                &mut early,
                SectionEdit::TrimStart { index: 0, start: 0.5 }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(early, Channel::Audio(vec![sound(1.0, 3.0)]), "nothing changed");
    }

    #[test]
    fn a_shorter_section_fits_its_ramps_and_too_long_ramps_are_refused() {
        let mut c = Channel::Clip(vec![ClipSection {
            blend_in: 1.0,
            blend_out: 3.0,
            ..clip(0.0, 4.0)
        }]);
        apply_section_edit(&mut c, SectionEdit::TrimEnd { index: 0, end: 2.0 }).expect("trim");
        let Channel::Clip(s) = &c else { unreachable!() };
        assert_eq!(
            (s[0].blend_in, s[0].blend_out),
            (0.5, 1.5),
            "scaled by one factor"
        );
        let keep = c.clone();
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::Ramps {
                    index: 0,
                    rise: 1.5,
                    fall: 1.0
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(c, keep);
        apply_section_edit(
            &mut c,
            SectionEdit::Ramps {
                index: 0,
                rise: 0.25,
                fall: 0.25,
            },
        )
        .expect("ramps that fit");
        let mut s = Channel::Sub(vec![sub(0.0, 1.0)]);
        assert_eq!(
            code(apply_section_edit(
                &mut s,
                SectionEdit::Ramps {
                    index: 0,
                    rise: 0.1,
                    fall: 0.1
                }
            )),
            "seq_section_wrong_kind"
        );
    }

    #[test]
    fn adds_duplicates_and_removes_keep_indices_stable() {
        let mut c = Channel::Sub(vec![sub(0.0, 2.0), sub(1.0, 3.0)]);
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::Duplicate { index: 0, start: 5.0 }),
            Ok(Some(2))
        );
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::Add(Section::Sub(sub(8.0, 9.0)))),
            Ok(Some(3))
        );
        assert_eq!(
            apply_section_edit(&mut c, SectionEdit::Remove { index: 1 }),
            Ok(None)
        );
        let Channel::Sub(s) = &c else { unreachable!() };
        let spans: Vec<(f32, f32)> = s.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, [(0.0, 2.0), (5.0, 7.0), (8.0, 9.0)]);
        assert_eq!(
            code(apply_section_edit(
                &mut c,
                SectionEdit::Add(Section::Clip(clip(0.0, 1.0)))
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(apply_section_edit(&mut c, SectionEdit::Remove { index: 7 })),
            "seq_section_bad_index"
        );
        let mut one = Channel::Audio(vec![sound(0.0, 1.0)]);
        assert_eq!(
            code(apply_section_edit(&mut one, SectionEdit::Remove { index: 0 })),
            "seq_section_last"
        );
        let mut full = Channel::Audio(vec![sound(0.0, 1.0); MAX_SECTIONS]);
        assert_eq!(
            code(apply_section_edit(
                &mut full,
                SectionEdit::Duplicate { index: 0, start: 2.0 }
            )),
            "seq_section_limit"
        );
        let mut keyed = Channel::Bool(vec![]);
        assert_eq!(
            code(apply_section_edit(&mut keyed, SectionEdit::Remove { index: 0 })),
            "seq_section_not_sections"
        );
    }

    #[test]
    fn speed_offset_and_gain_check_their_ranges_and_kinds() {
        let mut c = Channel::Clip(vec![clip(0.0, 2.0)]);
        apply_section_edit(&mut c, SectionEdit::Speed { index: 0, speed: 0.5 }).expect("speed");
        apply_section_edit(
            &mut c,
            SectionEdit::Offset {
                index: 0,
                offset: -1.0,
            },
        )
        .expect("a clip offset may be negative");
        apply_section_edit(
            &mut c,
            SectionEdit::Gain {
                index: 0,
                value: 0.25,
            },
        )
        .expect("weight");
        let Channel::Clip(s) = &c else { unreachable!() };
        assert_eq!((s[0].speed, s[0].offset, s[0].weight), (0.5, -1.0, 0.25));
        let keep = c.clone();
        for edit in [
            SectionEdit::Speed { index: 0, speed: 0.0 },
            SectionEdit::Speed {
                index: 0,
                speed: f32::NAN,
            },
            SectionEdit::Gain { index: 0, value: 1.5 },
            SectionEdit::Offset {
                index: 0,
                offset: f32::INFINITY,
            },
            SectionEdit::Move {
                index: 0,
                start: -1.0,
            },
            SectionEdit::TrimEnd { index: 0, end: 0.0 },
        ] {
            assert_eq!(
                code(apply_section_edit(&mut c, edit.clone())),
                "seq_section_out_of_range",
                "{edit:?}"
            );
        }
        assert_eq!(c, keep, "refused edits change nothing");
        let mut a = Channel::Audio(vec![sound(0.0, 2.0)]);
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::Speed { index: 0, speed: 2.0 }
            )),
            "seq_section_wrong_kind"
        );
        apply_section_edit(
            &mut a,
            SectionEdit::Gain {
                index: 0,
                value: -12.0,
            },
        )
        .expect("volume");
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::Gain {
                    index: 0,
                    value: 30.0
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::Offset {
                    index: 0,
                    offset: -0.5
                }
            )),
            "seq_section_out_of_range"
        );
        let mut s = Channel::Sub(vec![sub(0.0, 2.0)]);
        assert_eq!(
            code(apply_section_edit(
                &mut s,
                SectionEdit::Gain { index: 0, value: 0.5 }
            )),
            "seq_section_wrong_kind"
        );
    }

    #[test]
    fn a_rename_changes_what_a_section_plays_and_an_empty_name_is_refused() {
        let mut c = Channel::Clip(vec![clip(0.0, 1.0), clip(1.0, 2.0)]);
        let rename = |index: usize, name: &str| SectionEdit::Rename {
            index,
            name: name.into(),
        };
        assert_eq!(apply_section_edit(&mut c, rename(1, "wave")), Ok(Some(1)));
        let Channel::Clip(s) = &c else { unreachable!() };
        assert_eq!((s[0].clip.as_str(), s[1].clip.as_str()), ("walk", "wave"));
        let keep = c.clone();
        assert_eq!(
            code(apply_section_edit(&mut c, rename(0, ""))),
            "seq_section_out_of_range"
        );
        assert_eq!(
            code(apply_section_edit(&mut c, rename(5, "idle"))),
            "seq_section_bad_index"
        );
        assert_eq!(c, keep);
        let mut a = Channel::Audio(vec![sound(0.0, 1.0)]);
        apply_section_edit(&mut a, rename(0, "chime")).expect("sound");
        let mut b = Channel::Sub(vec![sub(0.0, 1.0)]);
        apply_section_edit(&mut b, rename(0, "fx/other.seq.ron")).expect("sequence");
        let (Channel::Audio(a), Channel::Sub(b)) = (&a, &b) else {
            unreachable!()
        };
        assert_eq!(
            (a[0].sound.as_str(), b[0].sequence.as_str()),
            ("chime", "fx/other.seq.ron")
        );
    }

    #[test]
    fn spans_take_the_lowest_free_row_in_start_order() {
        let c = Channel::Clip(vec![
            clip(2.0, 5.0),
            clip(0.0, 3.0),
            clip(3.0, 4.0),
            clip(5.0, 6.0),
            clip(1.0, 2.0),
        ]);
        let spans = section_spans(&c).expect("sections");
        let rows: Vec<usize> = spans.iter().map(|s| s.row).collect();
        // By start: [1] 0..3 row 0, [4] 1..2 row 1, [0] 2..5 row 1, [2] 3..4 row 0, [3] 5..6 row 0.
        assert_eq!(rows, [1, 0, 0, 0, 1]);
        assert_eq!(spans.iter().map(|s| s.index).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        assert_eq!(section_spans(&c), Some(spans), "a pure function");
        assert!(section_spans(&Channel::Float(vec![])).is_none());
        let a = Channel::Audio(vec![AudioSection {
            fade_in: 0.25,
            fade_out: 0.5,
            ..sound(0.0, 1.0)
        }]);
        let span = &section_spans(&a).expect("sections")[0];
        assert_eq!((span.label.as_str(), span.rise, span.fall), ("music", 0.25, 0.5));
    }

    #[test]
    fn snapping_rounds_to_the_nearest_frame() {
        assert_eq!(snap_to_frame(1.01, 30.0), 1.0);
        assert_eq!(snap_to_frame(1.02, 30.0), 31.0 / 30.0);
        assert_eq!(snap_to_frame(0.3, 0.0), 0.3);
        assert_eq!(snap_to_frame(0.3, f32::NAN), 0.3);
    }

    /// The v5 time scale mode (ADR-3197): set on an audio section, refused elsewhere and for a
    /// pitch-preserving time scale outside 0.5..=2; Doppler and occlusion travel in `Spatial`.
    #[test]
    fn v5_time_scale_mode_edits_audio_sections_only() {
        use super::super::asset::{SoundSource, TimeScaleMode};
        let mut a = Channel::Audio(vec![sound(1.0, 3.0)]);
        let keep = |index| SectionEdit::TimeScaleMode {
            index,
            mode: TimeScaleMode::PreservePitch,
        };
        apply_section_edit(&mut a, keep(0)).expect("mode");
        let mut spatial = AudioSpatial::new(SoundSource::Position([0.0, 0.0, -3.0]));
        spatial.doppler = 0.5;
        spatial.occlusion = true;
        apply_section_edit(
            &mut a,
            SectionEdit::Spatial {
                index: 0,
                spatial: Some(spatial.clone()),
            },
        )
        .expect("spatial");
        let Channel::Audio(list) = &a else { unreachable!() };
        assert_eq!(list[0].time_scale_mode, TimeScaleMode::PreservePitch);
        assert_eq!(list[0].spatial.as_ref(), Some(&spatial));
        let before = a.clone();
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::TimeScale { index: 0, scale: 3.0 }
            )),
            "seq_section_out_of_range",
            "a pitch-preserving section stretches 0.5..=2"
        );
        let mut loud = spatial.clone();
        loud.doppler = 5.0;
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::Spatial {
                    index: 0,
                    spatial: Some(loud)
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(a, before, "refusals change nothing");
        let mut subs = Channel::Sub(vec![sub(0.0, 1.0)]);
        assert_eq!(
            code(apply_section_edit(&mut subs, keep(0))),
            "seq_section_wrong_kind"
        );
        assert_eq!(code(apply_section_edit(&mut a, keep(3))), "seq_section_bad_index");
    }

    #[test]
    fn v3_edits_set_time_scale_spatial_and_repeat_on_their_kinds_only() {
        use super::super::asset::{SoundSource, SubRepeat};
        let mut a = Channel::Audio(vec![sound(1.0, 3.0)]);
        apply_section_edit(&mut a, SectionEdit::TimeScale { index: 0, scale: 2.0 }).expect("scale");
        // A trim at time scale 2 keeps the sound on the clock: one second later is two sound seconds.
        apply_section_edit(&mut a, SectionEdit::TrimStart { index: 0, start: 2.0 }).expect("trim");
        let spatial = AudioSpatial::new(SoundSource::Binding("hero".into()));
        apply_section_edit(
            &mut a,
            SectionEdit::Spatial {
                index: 0,
                spatial: Some(spatial.clone()),
            },
        )
        .expect("spatial");
        let Channel::Audio(list) = &a else { unreachable!() };
        assert_eq!((list[0].time_scale, list[0].offset), (2.0, 2.0));
        assert_eq!(list[0].spatial.as_ref(), Some(&spatial));
        // Refusals change nothing.
        let before = a.clone();
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::TimeScale { index: 0, scale: 0.0 }
            )),
            "seq_section_out_of_range"
        );
        let mut bad = spatial.clone();
        bad.max_distance = 0.5;
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::Spatial {
                    index: 0,
                    spatial: Some(bad)
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut a,
                SectionEdit::Repeat {
                    index: 0,
                    repeat: SubRepeat::UntilStop,
                    loop_length: None
                }
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(a, before);

        let mut s = Channel::Sub(vec![sub(0.0, 4.0)]);
        apply_section_edit(
            &mut s,
            SectionEdit::Repeat {
                index: 0,
                repeat: SubRepeat::Count(3),
                loop_length: Some(0.5),
            },
        )
        .expect("repeat");
        let Channel::Sub(list) = &s else { unreachable!() };
        assert_eq!(
            (list[0].repeat, list[0].loop_length),
            (SubRepeat::Count(3), Some(0.5))
        );
        let before = s.clone();
        assert_eq!(
            code(apply_section_edit(
                &mut s,
                SectionEdit::Repeat {
                    index: 0,
                    repeat: SubRepeat::Count(0),
                    loop_length: None
                }
            )),
            "seq_section_out_of_range"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut s,
                SectionEdit::TimeScale { index: 0, scale: 2.0 }
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut s,
                SectionEdit::Spatial {
                    index: 0,
                    spatial: None
                }
            )),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(apply_section_edit(
                &mut s,
                SectionEdit::TimeScale { index: 5, scale: 2.0 }
            )),
            "seq_section_bad_index"
        );
        assert_eq!(s, before);
    }
}
