// SPDX-License-Identifier: MPL-2.0
//! Montage editing: segment edits with linked notifies and sections, and the edit operations
//! the editor and the agent protocol apply to a `.montage.ron` document.
//!
//! Specification: `docs/specs/34-montage-extensions.md` M8 and M10, read through ADR-1870.
//! Segments sit back to back from montage time 0, so a segment owns no timeline position: an
//! edit changes its clip-local trim points (`start`, `end`), its place in the track order, or
//! which segments exist, and the timeline positions follow. Every edit runs on a copy:
//!
//! 1. the operation changes the copy and records where each old segment went;
//! 2. each section or notify with a `follow` link moves by its segment's change
//!    ([`FollowMode::KeepTime`] stays, [`FollowMode::ShiftWithStart`] moves with the segment's
//!    timeline start, [`FollowMode::ScaleWithLength`] keeps its fractional position);
//! 3. times clamp at 0, the first section goes back to 0, and [`Montage::validate`] runs.
//!
//! On any error the montage is left unchanged. The runtime ignores `follow` entirely.

use serde::{Deserialize, Serialize};

use crate::montage::{
    DEFAULT_SECTION, FollowMode, Montage, MontageBlend, MontageNotify, MontageSection, MontageSegment,
    MontageTrack, SectionNext, SegmentFollow, WarpWindow,
};
use crate::montage_variant::{MontageVariant, SegmentOverride};
use crate::warp::{RotationKind, WarpAnchor, WarpMode};

/// Why a montage edit was refused (nothing changed).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MontageEditError {
    /// The edit itself is wrong: an index out of range, a bad number, an unknown name, or a
    /// timeline length the edit needs but cannot know (`montage_bad_edit`).
    #[error("montage_bad_edit: {0}")]
    BadEdit(String),
    /// The edited montage would fail [`Montage::validate`] (`montage_invalid`).
    #[error("montage_invalid: {0}")]
    Invalid(String),
}

impl MontageEditError {
    /// The agent-protocol error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            MontageEditError::BadEdit(_) => "montage_bad_edit",
            MontageEditError::Invalid(_) => "montage_invalid",
        }
    }
}

fn bad<T>(message: impl Into<String>) -> Result<T, MontageEditError> {
    Err(MontageEditError::BadEdit(message.into()))
}

/// Which kind of timeline item an edit touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditedItem {
    /// A section marker.
    Section,
    /// A notify (instant, window or branching point).
    Notify,
}

/// A linked item an edit moved.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MovedItem {
    /// Section or notify.
    pub kind: EditedItem,
    /// Index in the montage's list after the edit.
    pub index: usize,
    /// Its name.
    pub name: String,
    /// Montage time before the edit.
    pub from: f32,
    /// Montage time after the edit.
    pub to: f32,
}

/// What an edit did to linked items.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EditReport {
    /// Sections and notifies whose time changed.
    pub moved: Vec<MovedItem>,
    /// Items whose linked segment was removed: their `follow` is cleared, their time kept.
    pub unlinked: Vec<(EditedItem, String)>,
}

/// A follow link as the edit surface writes it (snake-case mode names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowEdit {
    /// Track index.
    pub track: usize,
    /// Segment index on that track.
    pub segment: usize,
    /// `keep_time`, `shift_with_start` or `scale_with_length`.
    #[serde(default)]
    pub mode: FollowModeEdit,
}

/// [`FollowMode`] with snake-case names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowModeEdit {
    /// Stay at the same time.
    #[default]
    KeepTime,
    /// Move with the segment's timeline start.
    ShiftWithStart,
    /// Keep the fractional position within the segment.
    ScaleWithLength,
}

impl From<FollowEdit> for SegmentFollow {
    fn from(f: FollowEdit) -> Self {
        SegmentFollow {
            track: f.track,
            segment: f.segment,
            mode: match f.mode {
                FollowModeEdit::KeepTime => FollowMode::KeepTime,
                FollowModeEdit::ShiftWithStart => FollowMode::ShiftWithStart,
                FollowModeEdit::ScaleWithLength => FollowMode::ScaleWithLength,
            },
        }
    }
}

/// Where a section goes at its end, as the edit surface writes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextEdit {
    /// Play on into the next section.
    Continue,
    /// End the montage at this section's end.
    Stop,
    /// Jump to the named section.
    Jump(String),
}

impl From<NextEdit> for SectionNext {
    fn from(n: NextEdit) -> Self {
        match n {
            NextEdit::Continue => SectionNext::Continue,
            NextEdit::Stop => SectionNext::Stop,
            NextEdit::Jump(s) => SectionNext::Jump(s),
        }
    }
}

fn one() -> f32 {
    1.0
}

fn one_loop() -> u32 {
    1
}

fn yes() -> bool {
    true
}

/// One edit of a `.montage.ron` document (`anim.montage_edit`'s `edit` object, tagged by `op`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum MontageEdit {
    /// Insert a clip on the track that plays `slot` (a new track when no track plays it), at
    /// `index` (default: the end). Linked items of the segments after it move.
    AddSegment {
        /// Slot of the track.
        slot: String,
        /// Insert position on the track; `None` appends.
        #[serde(default)]
        index: Option<usize>,
        /// Clip name.
        clip: String,
        /// Clip-local start, seconds.
        #[serde(default)]
        start: f32,
        /// Clip-local end; `None` plays to the clip's end.
        #[serde(default)]
        end: Option<f32>,
        /// Segment rate.
        #[serde(default = "one")]
        rate: f32,
        /// Loop count.
        #[serde(default = "one_loop")]
        loops: u32,
    },
    /// Trim a segment (`start`, `end`, or `to_clip_end` to clear its end) and/or move it to
    /// position `to` on its track. Linked items move (M8).
    MoveSegment {
        /// Track index.
        track: usize,
        /// Segment index.
        segment: usize,
        /// New clip-local start.
        #[serde(default)]
        start: Option<f32>,
        /// New clip-local end.
        #[serde(default)]
        end: Option<f32>,
        /// Play to the clip's end (clears `end`).
        #[serde(default)]
        to_clip_end: bool,
        /// New position on the track.
        #[serde(default)]
        to: Option<usize>,
        /// New rate.
        #[serde(default)]
        rate: Option<f32>,
        /// New loop count.
        #[serde(default)]
        loops: Option<u32>,
    },
    /// Remove a segment (a track left empty is removed; the main track keeps at least one).
    RemoveSegment {
        /// Track index.
        track: usize,
        /// Segment index.
        segment: usize,
    },
    /// Add or change the section `name`.
    SetSection {
        /// Section name.
        name: String,
        /// Start, montage seconds (required for a new section).
        #[serde(default)]
        start: Option<f32>,
        /// Link taken at its end.
        #[serde(default)]
        next: Option<NextEdit>,
        /// Follow link to set.
        #[serde(default)]
        follow: Option<FollowEdit>,
    },
    /// Remove the section `name`.
    RemoveSection {
        /// Section name.
        name: String,
    },
    /// Set where section `from` goes: to section `to`, to the end (`stop`), or on (neither).
    LinkSections {
        /// Section name.
        from: String,
        /// Target section.
        #[serde(default)]
        to: Option<String>,
        /// End the montage at `from`'s end.
        #[serde(default)]
        stop: bool,
    },
    /// Add a notify.
    AddNotify {
        /// Notify name.
        name: String,
        /// Montage time.
        time: f32,
        /// `0` = instant; `> 0` = a window.
        #[serde(default)]
        duration: f32,
        /// Payload string.
        #[serde(default)]
        payload: Option<String>,
        /// A branching point.
        #[serde(default)]
        branch: bool,
        /// Follow link.
        #[serde(default)]
        follow: Option<FollowEdit>,
    },
    /// Remove every notify named `name`.
    RemoveNotify {
        /// Notify name.
        name: String,
    },
    /// Set or clear (`follow: null`) the follow link of a section or of every notify named `name`.
    SetFollow {
        /// `section` or `notify`.
        kind: EditedItem,
        /// Item name.
        name: String,
        /// The link; `None` clears it.
        #[serde(default)]
        follow: Option<FollowEdit>,
    },
    /// Change blend settings (absent fields keep their value).
    SetBlend {
        /// Blend-in seconds.
        #[serde(default)]
        blend_in: Option<f32>,
        /// Blend-out seconds.
        #[serde(default)]
        blend_out: Option<f32>,
        /// Seconds before the end the automatic blend-out starts.
        #[serde(default)]
        blend_out_lead: Option<f32>,
        /// Blend out automatically at the end.
        #[serde(default)]
        auto_blend_out: Option<bool>,
    },
    /// Add a motion-warping window.
    AddWarp {
        /// Warp target name.
        target: String,
        /// Window start.
        start: f32,
        /// Window end.
        end: f32,
        /// Warp horizontal translation.
        #[serde(default = "yes")]
        translation: bool,
        /// Warp height.
        #[serde(default)]
        vertical: bool,
        /// Turn toward the target.
        #[serde(default)]
        rotation: bool,
        /// `similarity` (default) or `path_scale`.
        #[serde(default)]
        mode: Option<String>,
        /// `auto` (default), `match_target` or `face_target`.
        #[serde(default)]
        rotation_kind: Option<String>,
        /// Fraction of the window the turn takes, `(0, 1]`.
        #[serde(default = "one")]
        rotation_fraction: f32,
        /// Anchor bone (the bone lands on the target).
        #[serde(default)]
        anchor_bone: Option<String>,
        /// Anchor offset from the origin, rig space.
        #[serde(default)]
        anchor_offset: Option<[f32; 3]>,
    },
    /// Remove warp window `index`.
    RemoveWarp {
        /// Window index.
        index: usize,
    },
}

impl MontageEdit {
    /// The `op` name.
    #[must_use]
    pub fn op(&self) -> &'static str {
        match self {
            MontageEdit::AddSegment { .. } => "add_segment",
            MontageEdit::MoveSegment { .. } => "move_segment",
            MontageEdit::RemoveSegment { .. } => "remove_segment",
            MontageEdit::SetSection { .. } => "set_section",
            MontageEdit::RemoveSection { .. } => "remove_section",
            MontageEdit::LinkSections { .. } => "link_sections",
            MontageEdit::AddNotify { .. } => "add_notify",
            MontageEdit::RemoveNotify { .. } => "remove_notify",
            MontageEdit::SetFollow { .. } => "set_follow",
            MontageEdit::SetBlend { .. } => "set_blend",
            MontageEdit::AddWarp { .. } => "add_warp",
            MontageEdit::RemoveWarp { .. } => "remove_warp",
        }
    }
}

/// One edit of a `.montagevar.ron` (child montage) document: a variant inherits everything but
/// its overridden clips, so only overrides can be edited (ADR-1870).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum VariantEdit {
    /// Replace (or add) the override of one parent segment.
    SetOverride {
        /// Parent track index.
        track: usize,
        /// Parent segment index.
        segment: usize,
        /// Replacement clip.
        clip: String,
        /// Rescale the replacement to the original segment's duration.
        #[serde(default)]
        rescale: bool,
    },
    /// Remove the override of one parent segment.
    RemoveOverride {
        /// Parent track index.
        track: usize,
        /// Parent segment index.
        segment: usize,
    },
}

impl VariantEdit {
    /// Apply to `variant` (checked against `parent` for the indices); nothing changes on error.
    ///
    /// # Errors
    ///
    /// [`MontageEditError::BadEdit`] for an index the parent lacks, an empty clip, or a missing
    /// override to remove.
    pub fn apply(&self, variant: &mut MontageVariant, parent: &Montage) -> Result<(), MontageEditError> {
        match self {
            VariantEdit::SetOverride {
                track,
                segment,
                clip,
                rescale,
            } => {
                if parent
                    .tracks
                    .get(*track)
                    .is_none_or(|t| *segment >= t.segments.len())
                {
                    return bad(format!(
                        "the parent {:?} has no track {track} segment {segment}",
                        parent.name
                    ));
                }
                if clip.is_empty() {
                    return bad("the override needs a clip");
                }
                let entry = SegmentOverride {
                    track: *track,
                    segment: *segment,
                    clip: clip.clone(),
                    rescale: *rescale,
                };
                match variant
                    .overrides
                    .iter_mut()
                    .find(|o| (o.track, o.segment) == (*track, *segment))
                {
                    Some(existing) => *existing = entry,
                    None => variant.overrides.push(entry),
                }
            }
            VariantEdit::RemoveOverride { track, segment } => {
                let before = variant.overrides.len();
                variant
                    .overrides
                    .retain(|o| (o.track, o.segment) != (*track, *segment));
                if before == variant.overrides.len() {
                    return bad(format!("no override replaces track {track} segment {segment}"));
                }
            }
        }
        Ok(())
    }
}

/// Montage seconds one segment covers (all loops), or `None` when it plays to the end of a
/// clip whose length `lengths` does not know. Clamped like [`Montage::layout`] when the clip
/// length is known.
fn segment_len(seg: &MontageSegment, lengths: &dyn Fn(&str) -> Option<f32>) -> Option<f32> {
    let clip = lengths(&seg.clip).filter(|d| d.is_finite() && *d >= 0.0);
    let (start, end) = match (clip, seg.end) {
        (Some(d), end) => {
            let start = seg.start.min(d).max(0.0);
            (start, end.unwrap_or(d).min(d).max(start))
        }
        (None, Some(end)) => (seg.start, end.max(seg.start)),
        (None, None) => return None,
    };
    Some((end - start) / seg.rate * seg.loops as f32)
}

/// Timeline start of segment `index` on `track` (the sum of the lengths before it).
fn segment_at(track: &MontageTrack, index: usize, lengths: &dyn Fn(&str) -> Option<f32>) -> Option<f32> {
    track.segments[..index]
        .iter()
        .map(|s| segment_len(s, lengths))
        .sum()
}

/// The change of a timeline start between an old and a new list of the segments before it:
/// equal segments cancel (in any order), the rest need their lengths.
fn prefix_delta(
    old: &[MontageSegment],
    new: &[MontageSegment],
    lengths: &dyn Fn(&str) -> Option<f32>,
) -> Result<f32, MontageEditError> {
    let mut rest: Vec<&MontageSegment> = new.iter().collect();
    let mut delta = 0.0;
    let need = |seg: &MontageSegment| {
        segment_len(seg, lengths).ok_or_else(|| {
            MontageEditError::BadEdit(format!(
                "moving a linked item needs the length of clip {:?}, which plays to its end: load the clip or give the segment an `end`",
                seg.clip
            ))
        })
    };
    for seg in old {
        match rest.iter().position(|n| *n == seg) {
            Some(i) => {
                rest.swap_remove(i);
            }
            None => delta -= need(seg)?,
        }
    }
    for seg in rest {
        delta += need(seg)?;
    }
    Ok(delta)
}

/// Where each old `(track, segment)` went: `None` = removed.
type SegmentMap<'a> = &'a dyn Fn(usize, usize) -> Option<(usize, usize)>;

fn unknown_length(track: &MontageTrack, upto: usize, lengths: &dyn Fn(&str) -> Option<f32>) -> String {
    let clip = track.segments[..=upto]
        .iter()
        .find(|s| segment_len(s, lengths).is_none())
        .map_or("?", |s| s.clip.as_str());
    format!(
        "moving a linked item needs the length of clip {clip:?}, which plays to its end: load the clip or give the segment an `end`"
    )
}

/// Move every linked item of `new` (whose links still hold `old` indices) by its segment's
/// change, remap the links through `map`, and record what moved.
fn retime(
    old: &Montage,
    new: &mut Montage,
    map: SegmentMap<'_>,
    lengths: &dyn Fn(&str) -> Option<f32>,
    report: &mut EditReport,
) -> Result<(), MontageEditError> {
    // The new time of an item at `t` linked by `f` (old indices), or `None` to unlink.
    let new_tracks = new.tracks.clone();
    let moved_time = |t: f32, f: &SegmentFollow| -> Result<Option<(f32, SegmentFollow)>, MontageEditError> {
        let Some((nt, ns)) = map(f.track, f.segment) else {
            return Ok(None);
        };
        let link = SegmentFollow {
            track: nt,
            segment: ns,
            mode: f.mode,
        };
        if f.mode == FollowMode::KeepTime {
            return Ok(Some((t, link)));
        }
        let (Some(old_track), Some(new_track)) = (old.tracks.get(f.track), new_tracks.get(nt)) else {
            return Ok(Some((t, link)));
        };
        let (Some(old_seg), Some(new_seg)) = (old_track.segments.get(f.segment), new_track.segments.get(ns))
        else {
            return Ok(Some((t, link)));
        };
        // The change of the segment's timeline start: segments in both prefixes cancel, so only
        // the lengths of segments that changed, appeared or went away are needed.
        let delta = prefix_delta(
            &old_track.segments[..f.segment],
            &new_track.segments[..ns],
            lengths,
        )?;
        let to = match f.mode {
            FollowMode::KeepTime => t,
            FollowMode::ShiftWithStart => t + delta,
            FollowMode::ScaleWithLength if old_seg == new_seg => t + delta,
            FollowMode::ScaleWithLength => {
                let old_at = segment_at(old_track, f.segment, lengths).ok_or_else(|| {
                    MontageEditError::BadEdit(unknown_length(old_track, f.segment, lengths))
                })?;
                let old_len = segment_len(old_seg, lengths).ok_or_else(|| {
                    MontageEditError::BadEdit(unknown_length(old_track, f.segment, lengths))
                })?;
                let new_len = segment_len(new_seg, lengths)
                    .ok_or_else(|| MontageEditError::BadEdit(unknown_length(new_track, ns, lengths)))?;
                // new_at + (t - old_at) * ratio, with new_at = old_at + delta.
                if old_len > 0.0 {
                    t + delta + (t - old_at) * (new_len / old_len - 1.0)
                } else {
                    t + delta
                }
            }
        };
        Ok(Some((to.max(0.0), link)))
    };
    for (index, section) in new.sections.iter_mut().enumerate() {
        let Some(f) = section.follow else { continue };
        match moved_time(section.start, &f)? {
            Some((to, link)) => {
                if to != section.start {
                    report.moved.push(MovedItem {
                        kind: EditedItem::Section,
                        index,
                        name: section.name.clone(),
                        from: section.start,
                        to,
                    });
                    section.start = to;
                }
                section.follow = Some(link);
            }
            None => {
                section.follow = None;
                report.unlinked.push((EditedItem::Section, section.name.clone()));
            }
        }
    }
    for (index, notify) in new.notifies.iter_mut().enumerate() {
        let Some(f) = notify.follow else { continue };
        match moved_time(notify.time, &f)? {
            Some((to, link)) => {
                if to != notify.time {
                    report.moved.push(MovedItem {
                        kind: EditedItem::Notify,
                        index,
                        name: notify.name.clone(),
                        from: notify.time,
                        to,
                    });
                    notify.time = to;
                }
                notify.follow = Some(link);
            }
            None => {
                notify.follow = None;
                report.unlinked.push((EditedItem::Notify, notify.name.clone()));
            }
        }
    }
    // The first section starts at 0 (a section kept at the timeline start stays there).
    if let Some(first) = new.sections.first_mut()
        && first.start != 0.0
    {
        report.moved.push(MovedItem {
            kind: EditedItem::Section,
            index: 0,
            name: first.name.clone(),
            from: first.start,
            to: 0.0,
        });
        first.start = 0.0;
    }
    Ok(())
}

fn check_time(value: f32, what: &str) -> Result<(), MontageEditError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        bad(format!("{what} must be finite and >= 0, got {value}"))
    }
}

fn parse_mode(mode: Option<&str>) -> Result<WarpMode, MontageEditError> {
    match mode {
        None | Some("similarity") => Ok(WarpMode::Similarity),
        Some("path_scale") => Ok(WarpMode::PathScale),
        Some(other) => bad(format!("warp mode {other:?} is not similarity or path_scale")),
    }
}

fn parse_rotation_kind(kind: Option<&str>) -> Result<RotationKind, MontageEditError> {
    match kind {
        None | Some("auto") => Ok(RotationKind::Auto),
        Some("match_target") => Ok(RotationKind::MatchTarget),
        Some("face_target") => Ok(RotationKind::FaceTarget),
        Some(other) => bad(format!(
            "rotation_kind {other:?} is not auto, match_target or face_target"
        )),
    }
}

impl Montage {
    /// Trim segment `segment` of track `track` to clip-local `new_start..new_end` (`None` = to
    /// the clip's end) and move the linked notifies and sections (spec 34 M8, ADR-1870). Uses
    /// no clip lengths: see [`Montage::edit_segment_with`].
    ///
    /// # Errors
    ///
    /// [`MontageEditError`]; the montage is unchanged on error.
    pub fn edit_segment(
        &mut self,
        track: usize,
        segment: usize,
        new_start: f32,
        new_end: Option<f32>,
    ) -> Result<EditReport, MontageEditError> {
        self.edit_segment_with(track, segment, new_start, new_end, &|_| None)
    }

    /// [`Montage::edit_segment`] with a clip-length lookup (seconds), needed when a linked item
    /// moves and a segment before it (or its own) plays to its clip's end.
    ///
    /// # Errors
    ///
    /// [`MontageEditError`]; the montage is unchanged on error.
    pub fn edit_segment_with(
        &mut self,
        track: usize,
        segment: usize,
        new_start: f32,
        new_end: Option<f32>,
        lengths: &dyn Fn(&str) -> Option<f32>,
    ) -> Result<EditReport, MontageEditError> {
        let edit = MontageEdit::MoveSegment {
            track,
            segment,
            start: Some(new_start),
            end: new_end,
            to_clip_end: new_end.is_none(),
            to: None,
            rate: None,
            loops: None,
        };
        self.apply_edit(&edit, lengths)
    }

    /// Apply one edit atomically: on a copy, moving linked items, then [`Montage::validate`].
    /// `lengths` gives clip lengths in seconds where known.
    ///
    /// # Errors
    ///
    /// [`MontageEditError`]; the montage is unchanged on error.
    #[tracing::instrument(skip_all, name = "montage_apply_edit")]
    pub fn apply_edit(
        &mut self,
        edit: &MontageEdit,
        lengths: &dyn Fn(&str) -> Option<f32>,
    ) -> Result<EditReport, MontageEditError> {
        let mut next = self.clone();
        let mut report = EditReport::default();
        apply(self, &mut next, edit, lengths, &mut report)?;
        next.validate()
            .map_err(|e| MontageEditError::Invalid(e.to_string()))?;
        *self = next;
        Ok(report)
    }
}

fn track_of(montage: &Montage, track: usize) -> Result<&MontageTrack, MontageEditError> {
    montage
        .tracks
        .get(track)
        .ok_or_else(|| MontageEditError::BadEdit(format!("the montage has no track {track}")))
}

fn segment_of(montage: &Montage, track: usize, segment: usize) -> Result<&MontageSegment, MontageEditError> {
    track_of(montage, track)?
        .segments
        .get(segment)
        .ok_or_else(|| MontageEditError::BadEdit(format!("track {track} has no segment {segment}")))
}

fn apply(
    old: &Montage,
    next: &mut Montage,
    edit: &MontageEdit,
    lengths: &dyn Fn(&str) -> Option<f32>,
    report: &mut EditReport,
) -> Result<(), MontageEditError> {
    match edit {
        MontageEdit::AddSegment {
            slot,
            index,
            clip,
            start,
            end,
            rate,
            loops,
        } => {
            if clip.is_empty() || slot.is_empty() {
                return bad("add_segment needs a slot and a clip");
            }
            check_time(*start, "start")?;
            let segment = MontageSegment {
                clip: clip.clone(),
                start: *start,
                end: *end,
                rate: *rate,
                loops: *loops,
            };
            let track = match next.tracks.iter().position(|t| t.slot == *slot) {
                Some(t) => t,
                None => {
                    next.tracks.push(MontageTrack {
                        slot: slot.clone(),
                        segments: Vec::new(),
                    });
                    next.tracks.len() - 1
                }
            };
            let count = next.tracks[track].segments.len();
            let at = index.unwrap_or(count);
            if at > count {
                return bad(format!(
                    "track {track} has {count} segments; index {at} is past the end"
                ));
            }
            next.tracks[track].segments.insert(at, segment);
            let map = |t: usize, s: usize| Some((t, if t == track && s >= at { s + 1 } else { s }));
            retime(old, next, &map, lengths, report)
        }
        MontageEdit::MoveSegment {
            track,
            segment,
            start,
            end,
            to_clip_end,
            to,
            rate,
            loops,
        } => {
            let (track, segment) = (*track, *segment);
            let mut seg = segment_of(old, track, segment)?.clone();
            if let Some(start) = start {
                check_time(*start, "start")?;
                seg.start = *start;
            }
            if *to_clip_end {
                seg.end = None;
            } else if let Some(end) = end {
                seg.end = Some(*end);
            }
            if let Some(rate) = rate {
                seg.rate = *rate;
            }
            if let Some(loops) = loops {
                seg.loops = *loops;
            }
            let count = old.tracks[track].segments.len();
            let dest = to.unwrap_or(segment);
            if dest >= count {
                return bad(format!(
                    "track {track} has {count} segments; position {dest} is past the end"
                ));
            }
            let segments = &mut next.tracks[track].segments;
            segments.remove(segment);
            segments.insert(dest, seg);
            let map = |t: usize, s: usize| {
                if t != track {
                    return Some((t, s));
                }
                // Removing at `segment` then inserting at `dest`.
                let s2 = if s == segment {
                    dest
                } else {
                    let after_remove = if s > segment { s - 1 } else { s };
                    if after_remove >= dest {
                        after_remove + 1
                    } else {
                        after_remove
                    }
                };
                Some((t, s2))
            };
            retime(old, next, &map, lengths, report)
        }
        MontageEdit::RemoveSegment { track, segment } => {
            let (track, segment) = (*track, *segment);
            segment_of(old, track, segment)?;
            let only = old.tracks[track].segments.len() == 1;
            if only && track == 0 {
                return bad("the main track (track 0) needs at least one segment");
            }
            if only {
                next.tracks.remove(track);
            } else {
                next.tracks[track].segments.remove(segment);
            }
            let map = |t: usize, s: usize| {
                if t == track && s == segment {
                    None
                } else if t == track {
                    Some((t, if s > segment { s - 1 } else { s }))
                } else if only && t > track {
                    Some((t - 1, s))
                } else {
                    Some((t, s))
                }
            };
            retime(old, next, &map, lengths, report)
        }
        MontageEdit::SetSection {
            name,
            start,
            next: link,
            follow,
        } => {
            if name.is_empty() {
                return bad("a section needs a name");
            }
            if let Some(start) = start {
                check_time(*start, "section start")?;
            }
            match next.sections.iter().position(|s| s.name == *name) {
                Some(i) => {
                    let section = &mut next.sections[i];
                    if let Some(start) = start {
                        if i == 0 && *start != 0.0 {
                            return bad(format!("the first section {name:?} starts at 0"));
                        }
                        section.start = *start;
                    }
                    if let Some(link) = link {
                        section.next = link.clone().into();
                    }
                    if let Some(follow) = follow {
                        section.follow = Some((*follow).into());
                    }
                }
                None => {
                    let Some(start) = start else {
                        return bad(format!("new section {name:?} needs a start"));
                    };
                    if next.sections.is_empty() && *start > 0.0 && name != DEFAULT_SECTION {
                        // The implicit whole-montage section becomes explicit so the first
                        // section still starts at 0.
                        next.sections.push(MontageSection {
                            name: DEFAULT_SECTION.into(),
                            start: 0.0,
                            next: SectionNext::Continue,
                            follow: None,
                        });
                    }
                    next.sections.push(MontageSection {
                        name: name.clone(),
                        start: *start,
                        next: link.clone().map_or(SectionNext::Continue, Into::into),
                        follow: follow.map(Into::into),
                    });
                }
            }
            next.sections.sort_by(|a, b| a.start.total_cmp(&b.start));
            Ok(())
        }
        MontageEdit::RemoveSection { name } => {
            let Some(i) = next.sections.iter().position(|s| s.name == *name) else {
                return bad(format!("no section named {name:?}"));
            };
            next.sections.remove(i);
            if let Some(first) = next.sections.first_mut()
                && first.start != 0.0
            {
                report.moved.push(MovedItem {
                    kind: EditedItem::Section,
                    index: 0,
                    name: first.name.clone(),
                    from: first.start,
                    to: 0.0,
                });
                first.start = 0.0;
            }
            Ok(())
        }
        MontageEdit::LinkSections { from, to, stop } => {
            let Some(i) = next.sections.iter().position(|s| s.name == *from) else {
                return bad(format!("no section named {from:?}"));
            };
            next.sections[i].next = match (to, stop) {
                (Some(_), true) => return bad("give `to` or `stop`, not both"),
                (Some(to), false) => {
                    if !next.sections.iter().any(|s| s.name == *to) {
                        return bad(format!("no section named {to:?}"));
                    }
                    SectionNext::Jump(to.clone())
                }
                (None, true) => SectionNext::Stop,
                (None, false) => SectionNext::Continue,
            };
            Ok(())
        }
        MontageEdit::AddNotify {
            name,
            time,
            duration,
            payload,
            branch,
            follow,
        } => {
            check_time(*time, "notify time")?;
            check_time(*duration, "notify duration")?;
            next.notifies.push(MontageNotify {
                name: name.clone(),
                time: *time,
                duration: *duration,
                payload: payload.clone(),
                branch: *branch,
                follow: follow.map(Into::into),
            });
            Ok(())
        }
        MontageEdit::RemoveNotify { name } => {
            let before = next.notifies.len();
            next.notifies.retain(|n| n.name != *name);
            if before == next.notifies.len() {
                return bad(format!("no notify named {name:?}"));
            }
            Ok(())
        }
        MontageEdit::SetFollow { kind, name, follow } => {
            let link: Option<SegmentFollow> = follow.map(Into::into);
            let mut found = false;
            match kind {
                EditedItem::Section => {
                    for s in next.sections.iter_mut().filter(|s| s.name == *name) {
                        s.follow = link;
                        found = true;
                    }
                }
                EditedItem::Notify => {
                    for n in next.notifies.iter_mut().filter(|n| n.name == *name) {
                        n.follow = link;
                        found = true;
                    }
                }
            }
            if found {
                Ok(())
            } else {
                bad(format!("no {kind:?} named {name:?}").to_lowercase())
            }
        }
        MontageEdit::SetBlend {
            blend_in,
            blend_out,
            blend_out_lead,
            auto_blend_out,
        } => {
            for (value, what) in [
                (blend_in, "blend_in"),
                (blend_out, "blend_out"),
                (blend_out_lead, "blend_out_lead"),
            ] {
                if let Some(v) = value {
                    check_time(*v, what)?;
                }
            }
            if let Some(t) = blend_in {
                next.blend_in = MontageBlend {
                    time: *t,
                    ..next.blend_in
                };
            }
            if let Some(t) = blend_out {
                next.blend_out = MontageBlend {
                    time: *t,
                    ..next.blend_out
                };
            }
            if blend_out_lead.is_some() {
                next.blend_out_lead = *blend_out_lead;
            }
            if let Some(auto) = auto_blend_out {
                next.auto_blend_out = *auto;
            }
            Ok(())
        }
        MontageEdit::AddWarp {
            target,
            start,
            end,
            translation,
            vertical,
            rotation,
            mode,
            rotation_kind,
            rotation_fraction,
            anchor_bone,
            anchor_offset,
        } => {
            let anchor = match (anchor_bone, anchor_offset) {
                (Some(_), Some(_)) => return bad("give anchor_bone or anchor_offset, not both"),
                (Some(bone), None) => WarpAnchor::Bone(bone.clone()),
                (None, Some(v)) => WarpAnchor::Offset(*v),
                (None, None) => WarpAnchor::Root,
            };
            next.warps.push(WarpWindow {
                translation: *translation,
                vertical: *vertical,
                rotation: *rotation,
                mode: parse_mode(mode.as_deref())?,
                rotation_kind: parse_rotation_kind(rotation_kind.as_deref())?,
                rotation_fraction: *rotation_fraction,
                anchor,
                ..WarpWindow::new(target.clone(), *start, *end)
            });
            Ok(())
        }
        MontageEdit::RemoveWarp { index } => {
            if *index >= next.warps.len() {
                return bad(format!(
                    "the montage has {} warp windows; no index {index}",
                    next.warps.len()
                ));
            }
            next.warps.remove(*index);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(clip: &str, start: f32, end: f32) -> MontageSegment {
        MontageSegment {
            clip: clip.into(),
            start,
            end: Some(end),
            rate: 1.0,
            loops: 1,
        }
    }

    fn notify(name: &str, time: f32, follow: Option<(usize, usize, FollowMode)>) -> MontageNotify {
        MontageNotify {
            name: name.into(),
            time,
            duration: 0.0,
            payload: None,
            branch: false,
            follow: follow.map(|(track, segment, mode)| SegmentFollow { track, segment, mode }),
        }
    }

    /// Three back-to-back 1 s segments on the main track: [0,1) [1,2) [2,3).
    fn three() -> Montage {
        let mut m = Montage::single("combo", "full", "a");
        m.tracks[0].segments = vec![seg("a", 0.0, 1.0), seg("b", 0.0, 1.0), seg("c", 0.0, 1.0)];
        m
    }

    #[test]
    fn trim_moves_followers_by_mode() {
        let mut m = three();
        m.notifies = vec![
            notify("keep", 1.5, Some((0, 1, FollowMode::KeepTime))),
            notify("shift_same", 1.5, Some((0, 1, FollowMode::ShiftWithStart))),
            notify("scale_same", 1.5, Some((0, 1, FollowMode::ScaleWithLength))),
            notify("shift_after", 2.5, Some((0, 2, FollowMode::ShiftWithStart))),
            notify("scale_after", 2.5, Some((0, 2, FollowMode::ScaleWithLength))),
            notify("before", 0.5, Some((0, 0, FollowMode::ShiftWithStart))),
        ];
        // Segment 1 grows from 1 s to 2 s.
        let report = m.edit_segment(0, 1, 0.0, Some(2.0)).expect("edit");
        let time = |n: &str| m.notifies.iter().find(|x| x.name == n).map(|x| x.time);
        assert_eq!(time("keep"), Some(1.5));
        // The edited segment's timeline start did not change.
        assert_eq!(time("shift_same"), Some(1.5));
        // Halfway through [1,2) → halfway through [1,3).
        assert_eq!(time("scale_same"), Some(2.0));
        assert_eq!(time("shift_after"), Some(3.5));
        assert_eq!(time("scale_after"), Some(3.5));
        assert_eq!(time("before"), Some(0.5));
        assert_eq!(report.moved.len(), 3, "{report:?}");
    }

    #[test]
    fn edit_is_atomic_when_sections_would_cross() {
        let mut m = three();
        m.sections = vec![
            MontageSection {
                name: "a".into(),
                start: 0.0,
                next: SectionNext::Continue,
                follow: None,
            },
            MontageSection {
                name: "b".into(),
                start: 1.5,
                next: SectionNext::Continue,
                follow: Some(SegmentFollow {
                    track: 0,
                    segment: 2,
                    mode: FollowMode::ShiftWithStart,
                }),
            },
            MontageSection {
                name: "c".into(),
                start: 2.0,
                next: SectionNext::Continue,
                follow: None,
            },
        ];
        let before = m.clone();
        // Segment 1 grows by 1 s: section b would move to 2.5, past c at 2.0.
        let err = m.edit_segment(0, 1, 0.0, Some(2.0)).expect_err("crossing");
        assert_eq!(err.code(), "montage_invalid");
        assert_eq!(m, before);
    }

    #[test]
    fn unknown_length_is_needed_only_where_items_move() {
        let mut m = three();
        m.tracks[0].segments[0].end = None; // plays to the end of an unknown clip
        m.notifies = vec![notify("late", 2.5, Some((0, 2, FollowMode::ShiftWithStart)))];
        // Trimming segment 1 shifts segment 2 by the change of segment 1 only: segment 0's
        // length is never needed, so this works without lengths...
        m.edit_segment(0, 1, 0.0, Some(0.5)).expect("prefix unchanged");
        assert_eq!(m.notifies[0].time, 2.0);
        // ...but scaling an item inside segment 1 needs segment 1's absolute start.
        m.notifies = vec![notify("in", 1.2, Some((0, 1, FollowMode::ScaleWithLength)))];
        let err = m.edit_segment(0, 1, 0.0, Some(1.0)).expect_err("needs length");
        assert_eq!(err.code(), "montage_bad_edit");
        assert!(err.to_string().contains("\"a\""), "{err}");
        let report = m
            .edit_segment_with(0, 1, 0.0, Some(1.0), &|c| (c == "a").then_some(1.0))
            .expect("with lengths");
        assert_eq!(report.moved.len(), 1);
    }

    #[test]
    fn reorder_and_remove_remap_links() {
        let mut m = three();
        m.notifies = vec![
            notify("on_c", 2.25, Some((0, 2, FollowMode::ShiftWithStart))),
            notify("on_a", 0.25, Some((0, 0, FollowMode::ShiftWithStart))),
        ];
        // Move c to the front: c now covers [0,1), a [1,2).
        m.apply_edit(
            &MontageEdit::MoveSegment {
                track: 0,
                segment: 2,
                start: None,
                end: None,
                to_clip_end: false,
                to: Some(0),
                rate: None,
                loops: None,
            },
            &|_| None,
        )
        .expect("reorder");
        assert_eq!(m.notifies[0].time, 0.25);
        assert_eq!(m.notifies[0].follow.map(|f| f.segment), Some(0));
        assert_eq!(m.notifies[1].time, 1.25);
        assert_eq!(m.notifies[1].follow.map(|f| f.segment), Some(1));
        // Remove c (now segment 0): its follower is unlinked and stays; a's moves back.
        let report = m
            .apply_edit(&MontageEdit::RemoveSegment { track: 0, segment: 0 }, &|_| None)
            .expect("remove");
        assert_eq!(report.unlinked, vec![(EditedItem::Notify, "on_c".to_string())]);
        assert_eq!(m.notifies[0].follow, None);
        assert_eq!(m.notifies[1].time, 0.25);
    }

    #[test]
    fn first_section_returns_to_zero() {
        let mut m = three();
        m.sections = vec![MontageSection {
            name: "start".into(),
            start: 0.0,
            next: SectionNext::Continue,
            follow: Some(SegmentFollow {
                track: 0,
                segment: 1,
                mode: FollowMode::ShiftWithStart,
            }),
        }];
        m.apply_edit(
            &MontageEdit::AddSegment {
                slot: "full".into(),
                index: Some(0),
                clip: "x".into(),
                start: 0.0,
                end: Some(0.5),
                rate: 1.0,
                loops: 1,
            },
            &|_| None,
        )
        .expect("insert");
        assert_eq!(m.sections[0].start, 0.0);
        assert_eq!(m.sections[0].follow.map(|f| f.segment), Some(2));
    }

    #[test]
    fn variant_edits_only_touch_overrides() {
        let parent = three();
        let mut variant = MontageVariant {
            format_version: 1,
            name: "combo_axe".into(),
            parent: "combo".into(),
            overrides: Vec::new(),
        };
        let set = VariantEdit::SetOverride {
            track: 0,
            segment: 1,
            clip: "axe".into(),
            rescale: true,
        };
        set.apply(&mut variant, &parent).expect("set");
        set.apply(&mut variant, &parent).expect("replace");
        assert_eq!(variant.overrides.len(), 1);
        let out_of_range = VariantEdit::SetOverride {
            track: 0,
            segment: 9,
            clip: "axe".into(),
            rescale: false,
        };
        assert_eq!(
            out_of_range.apply(&mut variant, &parent).map_err(|e| e.code()),
            Err("montage_bad_edit")
        );
        VariantEdit::RemoveOverride { track: 0, segment: 1 }
            .apply(&mut variant, &parent)
            .expect("remove");
        assert!(variant.overrides.is_empty());
    }
}
