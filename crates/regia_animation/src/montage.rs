// SPDX-License-Identifier: MPL-2.0
//! Montage assets (`.montage.ron`, frozen v2): one-shot action timelines built from clips.
//!
//! Specification: `docs/specs/17-animation.md` § 4b (ADR-1465). A [`Montage`] lays clips out
//! back to back on one track per slot, splits the timeline into named [`MontageSection`]s that
//! link to each other (continue, stop, jump or loop), and carries timeline notifies (instant,
//! windows and branching points), blend settings, root-motion switches and motion-warping
//! windows. [`MontageLayout`] is the montage resolved against loaded clips: segment positions
//! on the timeline, the length, and the queries the player needs (sampling a track, the root
//! motion between two times, the clip spans a step crosses).
//!
//! The runtime lives in [`crate::montage_player`]; the warp math in [`crate::warp`].

use std::sync::Arc;

use regia_core::math::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::clip::{AnimationClip, ClipStep, RootMotionMode, filter_root_motion};
use crate::pose::Pose;
use crate::state_machine::TransitionCurve;
use crate::warp::{RotationKind, WarpAnchor, WarpMode};

/// The `.montage.ron` format version (v1 is the first version).
pub const MONTAGE_FORMAT_VERSION: u32 = 2;

/// Name of the implicit section of a montage that declares none.
pub const DEFAULT_SECTION: &str = "main";

fn one() -> f32 {
    1.0
}

fn one_loop() -> u32 {
    1
}

fn yes() -> bool {
    true
}

/// One clip placed on a montage track.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MontageSegment {
    /// Clip name (resolved against the player's library, then the animator's).
    pub clip: String,
    /// Clip-local time the segment starts at, seconds.
    #[serde(default)]
    pub start: f32,
    /// Clip-local time the segment ends at; `None` plays to the clip's end.
    #[serde(default)]
    pub end: Option<f32>,
    /// Playback rate of this segment (scales its length on the timeline).
    #[serde(default = "one")]
    pub rate: f32,
    /// How many times the `start..end` range plays back to back.
    #[serde(default = "one_loop")]
    pub loops: u32,
}

/// One track: the clips one slot plays, back to back from montage time 0.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MontageTrack {
    /// Slot name (a `SlotDef` on the player; an undefined slot is full body).
    pub slot: String,
    /// Clips in timeline order.
    pub segments: Vec<MontageSegment>,
}

/// Link used when a section index has no authored section.
const CONTINUE_LINK: SectionNext = SectionNext::Continue;

/// Where playback goes when it reaches the end of a section.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum SectionNext {
    /// Play on into the section that follows on the timeline (the montage ends after the last).
    #[default]
    Continue,
    /// The montage ends at this section's end.
    Stop,
    /// Jump to the start of the named section (a jump to itself loops the section).
    Jump(String),
}

/// A named range of the montage timeline, from `start` to the next section's start.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MontageSection {
    /// Section name (unique in the montage).
    pub name: String,
    /// Montage time the section starts at, seconds (the first section starts at 0).
    pub start: f32,
    /// Link taken at the section's end.
    #[serde(default)]
    pub next: SectionNext,
    /// Editing link to a segment (v2; the runtime ignores it).
    #[serde(default)]
    pub follow: Option<SegmentFollow>,
}

/// A notify on the montage timeline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MontageNotify {
    /// Notify name.
    pub name: String,
    /// Montage time, seconds.
    pub time: f32,
    /// `0` = an instant notify; `> 0` = a window with a begin and an end event.
    #[serde(default)]
    pub duration: f32,
    /// Optional RON payload string handed to listeners.
    #[serde(default)]
    pub payload: Option<String>,
    /// A branching point: playback stops exactly here inside the frame so an action armed with
    /// `MontagePlayer::on_branch` applies at this time (instant notifies only).
    #[serde(default)]
    pub branch: bool,
    /// Editing link to a segment (v2; the runtime ignores it).
    #[serde(default)]
    pub follow: Option<SegmentFollow>,
}

/// Blend settings for entering or leaving a montage.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct MontageBlend {
    /// Seconds (0 = immediate).
    pub time: f32,
    /// Easing of the weight over the blend.
    #[serde(default)]
    pub curve: TransitionCurve,
}

impl Default for MontageBlend {
    fn default() -> Self {
        MontageBlend {
            time: 0.2,
            curve: TransitionCurve::Smooth,
        }
    }
}

impl MontageBlend {
    /// The blend weight `0..=1` after `elapsed` seconds of a blend toward full weight.
    pub fn progress(&self, elapsed: f32) -> f32 {
        if self.time.is_nan() || self.time <= 0.0 {
            return 1.0;
        }
        self.curve.alpha(elapsed / self.time)
    }
}

/// A montage time range `[start, end]`, seconds.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct TimeWindow {
    /// Start, seconds.
    pub start: f32,
    /// End, seconds.
    pub end: f32,
}

/// A motion-warping window: while montage time is inside it and the named target is set, root
/// motion is corrected so the rig lands on the target at `end` (`docs/specs/17-animation.md`
/// § 4b, extended by `docs/specs/34-montage-extensions.md` M2).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WarpWindow {
    /// Warp target name (`MontagePlayer::set_warp_target`).
    pub target: String,
    /// Window start, montage seconds.
    pub start: f32,
    /// Window end, montage seconds: the rig reaches the target here.
    pub end: f32,
    /// Warp the horizontal translation.
    #[serde(default = "yes")]
    pub translation: bool,
    /// Also warp the height (spread linearly over the window).
    #[serde(default)]
    pub vertical: bool,
    /// Turn the rig to the target's facing (or toward the target position) over the window.
    #[serde(default)]
    pub rotation: bool,
    /// How the path is mapped onto the offset (v2).
    #[serde(default)]
    pub mode: WarpMode,
    /// Which facing the rotation turns to (v2).
    #[serde(default)]
    pub rotation_kind: RotationKind,
    /// The turn completes when this fraction `(0, 1]` of the window has elapsed (v2).
    #[serde(default = "one")]
    pub rotation_fraction: f32,
    /// What lands on the target at `end` (v2).
    #[serde(default)]
    pub anchor: WarpAnchor,
    /// Editor timeline color; ignored at runtime (v2).
    #[serde(default)]
    pub tint: Option<[u8; 3]>,
}

impl WarpWindow {
    /// A translation warp toward `target` over `start..end` with every v2 option at its default.
    pub fn new(target: impl Into<String>, start: f32, end: f32) -> Self {
        WarpWindow {
            target: target.into(),
            start,
            end,
            translation: true,
            vertical: false,
            rotation: false,
            mode: WarpMode::default(),
            rotation_kind: RotationKind::default(),
            rotation_fraction: 1.0,
            anchor: WarpAnchor::default(),
            tint: None,
        }
    }
}

/// How a rate curve interpolates between keys.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RateInterpolation {
    /// Straight lines between keys.
    #[default]
    Linear,
    /// Each key holds until the next.
    Step,
}

/// A playback-rate curve over montage time (`docs/specs/34-montage-extensions.md` M7).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RateCurve {
    /// `(montage time, rate)` keys, strictly increasing in time, rates positive.
    pub keys: Vec<(f32, f32)>,
    /// Interpolation between keys.
    #[serde(default)]
    pub interpolation: RateInterpolation,
}

/// The lowest and highest rate a curve value is clamped to.
pub const RATE_CURVE_LIMITS: (f32, f32) = (0.05, 20.0);

/// The longest wall-clock substep when integrating a linear rate curve, seconds.
const RATE_SUBSTEP: f32 = 1.0 / 240.0;
/// Most midpoint substeps one [`RateCurve::advance`] takes (17 s at [`RATE_SUBSTEP`]); a longer
/// step (a hitch, a debugger pause, a huge host delta) uses longer substeps instead of looping
/// `dt / RATE_SUBSTEP` times (`1e30` s never returned; audit-gameplay Low).
const MAX_RATE_SUBSTEPS: f32 = 4096.0;

impl RateCurve {
    /// The curve's rate at montage time `t`, clamped to [`RATE_CURVE_LIMITS`]. Before the first
    /// key the first rate holds, after the last key the last rate holds.
    #[must_use]
    pub fn rate_at(&self, t: f32) -> f32 {
        let (lo, hi) = RATE_CURVE_LIMITS;
        let raw = match self.keys.as_slice() {
            [] => 1.0,
            [only] => only.1,
            keys => match keys.iter().rposition(|&(time, _)| time <= t) {
                None => keys[0].1,
                Some(i) if i + 1 == keys.len() => keys[i].1,
                Some(i) => match self.interpolation {
                    RateInterpolation::Step => keys[i].1,
                    RateInterpolation::Linear => {
                        let ((t0, r0), (t1, r1)) = (keys[i], keys[i + 1]);
                        r0 + (r1 - r0) * ((t - t0) / (t1 - t0)).clamp(0.0, 1.0)
                    }
                },
            },
        };
        if raw.is_finite() { raw.clamp(lo, hi) } else { 1.0 }
    }

    /// Montage seconds covered by `dt` wall seconds starting at montage time `t`, with `base`
    /// (the montage rate times the play-call rate) multiplying the curve.
    ///
    /// Montage time advances by the integral of the rate: exactly for [`RateInterpolation::Step`]
    /// (the walk stops at each key), and for `Linear` by midpoint substeps of 1/240 s (at most 4096 per call; a longer step uses longer substeps), so
    /// the frame rate does not change where a montage ends up (within 1e-3 s).
    #[must_use]
    pub fn advance(&self, t: f32, dt: f32, base: f32) -> f32 {
        #[cfg(test)]
        perf_work::note_advance();
        if !(dt.is_finite() && dt > 0.0 && base.is_finite() && base > 0.0) {
            return 0.0;
        }
        let mut now = t;
        match self.interpolation {
            RateInterpolation::Step => {
                let mut wall = dt;
                // At most one stop per key plus the final stretch.
                for _ in 0..=self.keys.len() {
                    let speed = self.rate_at(now) * base;
                    let next = self.keys.iter().map(|k| k.0).find(|&k| k > now);
                    match next {
                        Some(key) if (key - now) / speed <= wall => {
                            wall -= (key - now) / speed;
                            now = key;
                        }
                        _ => {
                            now += speed * wall;
                            break;
                        }
                    }
                }
            }
            RateInterpolation::Linear => {
                // Capped: one long hitch (a 60 s pause is 14,400 substeps) must not cost orders of
                // magnitude more than a normal frame; past the cap the substeps just get longer
                // (muse L, ADR-1809).
                let steps = (dt / RATE_SUBSTEP).ceil().clamp(1.0, MAX_RATE_SUBSTEPS);
                let h = dt / steps;
                for _ in 0..steps as usize {
                    let mid = now + 0.5 * h * base * self.rate_at(now);
                    now += h * base * self.rate_at(mid);
                }
            }
        }
        now - t
    }
}

/// How an edited segment moves a following notify or section (M8).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum FollowMode {
    /// Stay at the same time.
    #[default]
    KeepTime,
    /// Shift by the change of the segment's start.
    ShiftWithStart,
    /// Keep the fractional position within the segment.
    ScaleWithLength,
}

/// A link from a notify or section to a segment (editing only).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentFollow {
    /// Track index.
    pub track: usize,
    /// Segment index on that track.
    pub segment: usize,
    /// How an edit moves the linked item.
    #[serde(default)]
    pub mode: FollowMode,
}

/// A montage asset (the `.montage.ron` document).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Montage {
    /// Format version of this file.
    #[serde(default)]
    pub format_version: u32,
    /// Montage name (the player's library key).
    pub name: String,
    /// Base playback rate (multiplied by the play call's rate).
    #[serde(default = "one")]
    pub rate: f32,
    /// One track per slot; track 0 is the main track (root motion, slot group).
    pub tracks: Vec<MontageTrack>,
    /// Named sections; empty = one implicit section [`DEFAULT_SECTION`] over the whole montage.
    #[serde(default)]
    pub sections: Vec<MontageSection>,
    /// Timeline notifies, windows and branching points.
    #[serde(default)]
    pub notifies: Vec<MontageNotify>,
    /// Blend into the montage.
    #[serde(default)]
    pub blend_in: MontageBlend,
    /// Blend out of the montage.
    #[serde(default)]
    pub blend_out: MontageBlend,
    /// Seconds before the end at which the automatic blend-out starts; `None` = the blend-out
    /// time (the blend finishes as the montage ends).
    #[serde(default)]
    pub blend_out_lead: Option<f32>,
    /// Blend out automatically at the end; `false` holds the last pose until stopped.
    #[serde(default = "yes")]
    pub auto_blend_out: bool,
    /// The main track's root motion drives the rig (each clip's import mode applies).
    #[serde(default = "yes")]
    pub root_motion: bool,
    /// Windows that drop root motion (the rig is steered by gameplay there).
    #[serde(default)]
    pub root_motion_off: Vec<TimeWindow>,
    /// Motion-warping windows.
    #[serde(default)]
    pub warps: Vec<WarpWindow>,
    /// While the montage blends out, its main track leads this sync group (`.sync.ron`), so the
    /// graph's clips in the group pick up in step with it.
    #[serde(default)]
    pub sync_group: Option<String>,
    /// Playback-rate curve over montage time (v2).
    #[serde(default)]
    pub rate_curve: Option<RateCurve>,
}

/// An authored item whose time lies past the end of its montage's timeline (spec 34 M4: a
/// `rescale: false` variant can shorten the timeline under its inherited sections, notifies and
/// windows).
#[derive(Clone, Debug, PartialEq)]
pub struct PastEndItem {
    /// Stable lowercase code: `montage_past_end`.
    pub code: &'static str,
    /// What the item is: `section`, `notify`, `warp` or `root_motion_off`.
    pub kind: &'static str,
    /// Its index in that list.
    pub index: usize,
    /// Its name (the warp target for a warp window, empty for a root-motion window).
    pub name: String,
    /// The latest time the item names (a window's end, a notify's end), seconds.
    pub time: f32,
    /// The timeline's end, seconds.
    pub end: f32,
}

/// Slack below which an item at the timeline's end is not past it, seconds (the rescale
/// tolerance of spec 34 M4).
const PAST_END_SLACK: f32 = 1e-5;

/// Errors loading, validating or resolving a montage.
#[derive(Debug, thiserror::Error)]
pub enum MontageError {
    /// RON syntax or shape error.
    #[error("montage RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("montage RON write: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file is newer than this engine.
    #[error("montage format version {0} is unknown (current is {MONTAGE_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed.
    #[error("invalid montage: {0}")]
    Invalid(String),
    /// A segment's clip is not loaded.
    #[error("montage {montage:?}: clip {clip:?} is not loaded")]
    MissingClip {
        /// The montage.
        montage: String,
        /// The clip it references.
        clip: String,
    },
}

/// `from` versions of the registered migrations: v1 → v2 only adds fields with defaults, so
/// the loader reads v1 text as is and the version moves on re-save (ADR-1645).
#[must_use]
pub fn montage_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The serde field-list fingerprint of the format (for `cargo xtask check-formats`).
#[must_use]
pub fn montage_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<Montage>(),
        serde_field_names::<MontageTrack>(),
        serde_field_names::<MontageSegment>(),
        serde_field_names::<MontageSection>(),
        serde_field_names::<MontageNotify>(),
        serde_field_names::<MontageBlend>(),
        serde_field_names::<TimeWindow>(),
        serde_field_names::<WarpWindow>(),
        serde_field_names::<RateCurve>(),
        serde_field_names::<SegmentFollow>(),
        serde_variant_names::<SectionNext>(),
        serde_variant_names::<WarpMode>(),
        serde_variant_names::<RotationKind>(),
        serde_variant_names::<WarpAnchor>(),
        serde_variant_names::<RateInterpolation>(),
        serde_variant_names::<FollowMode>(),
        serde_variant_names::<TransitionCurve>(),
    ])
}

fn finite_non_negative(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

impl Montage {
    /// A montage playing `clip` once, full length, on `slot`.
    pub fn single(name: impl Into<String>, slot: impl Into<String>, clip: impl Into<String>) -> Self {
        Montage {
            format_version: MONTAGE_FORMAT_VERSION,
            name: name.into(),
            rate: 1.0,
            tracks: vec![MontageTrack {
                slot: slot.into(),
                segments: vec![MontageSegment {
                    clip: clip.into(),
                    start: 0.0,
                    end: None,
                    rate: 1.0,
                    loops: 1,
                }],
            }],
            sections: Vec::new(),
            notifies: Vec::new(),
            blend_in: MontageBlend::default(),
            blend_out: MontageBlend::default(),
            blend_out_lead: None,
            auto_blend_out: true,
            root_motion: true,
            root_motion_off: Vec::new(),
            warps: Vec::new(),
            sync_group: None,
            rate_curve: None,
        }
    }

    /// Parses `.montage.ron` text: peeks the version (newer is rejected), validates.
    ///
    /// # Errors
    ///
    /// Syntax errors, unknown versions and failed [`Montage::validate`].
    pub fn from_ron_str(text: &str) -> Result<Self, MontageError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| MontageError::Invalid(format!("cannot read format_version: {e}")))?;
        // The format starts at 1: an explicit 0 is damage, not a current file
        // (audit-v035-formats 11).
        if version == 0 || version > MONTAGE_FORMAT_VERSION {
            return Err(MontageError::UnknownVersion(version));
        }
        let mut montage: Montage = ron::from_str(text)?;
        montage.format_version = MONTAGE_FORMAT_VERSION;
        montage.validate()?;
        Ok(montage)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, MontageError> {
        let mut montage = self.clone();
        montage.format_version = MONTAGE_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&montage, pretty)?)
    }

    /// Checks everything that does not need the clips.
    ///
    /// # Errors
    ///
    /// The first problem: an empty name, no tracks or an empty track, two tracks on one slot,
    /// a non-finite or non-positive rate, a segment with `loops: 0` or an `end` before its
    /// `start`, sections that are not strictly increasing from 0 or share a name, a jump to an
    /// unknown section, a negative or non-finite time, a branching point with a duration, or a
    /// window that ends before it starts.
    pub fn validate(&self) -> Result<(), MontageError> {
        let bad = |m: String| Err(MontageError::Invalid(m));
        if self.name.is_empty() {
            return bad("the montage has no name".into());
        }
        if !(self.rate.is_finite() && self.rate > 0.0) {
            return bad(format!("rate must be finite and positive, got {}", self.rate));
        }
        if self.tracks.is_empty() {
            return bad("the montage has no tracks".into());
        }
        for (i, track) in self.tracks.iter().enumerate() {
            if self.tracks[..i].iter().any(|t| t.slot == track.slot) {
                return bad(format!("two tracks play slot {:?}", track.slot));
            }
            if track.segments.is_empty() {
                return bad(format!("the track on slot {:?} has no segments", track.slot));
            }
            for seg in &track.segments {
                if !(seg.rate.is_finite() && seg.rate > 0.0) {
                    return bad(format!("segment {:?} rate must be finite and positive", seg.clip));
                }
                if seg.loops == 0 {
                    return bad(format!("segment {:?} has loops: 0", seg.clip));
                }
                if !finite_non_negative(seg.start) {
                    return bad(format!("segment {:?} start must be finite and >= 0", seg.clip));
                }
                if let Some(end) = seg.end
                    && !(end.is_finite() && end > seg.start)
                {
                    return bad(format!(
                        "segment {:?} end must be finite and after its start",
                        seg.clip
                    ));
                }
            }
        }
        for (i, section) in self.sections.iter().enumerate() {
            if section.name.is_empty() {
                return bad(format!("section {i} has no name"));
            }
            if self.sections[..i].iter().any(|s| s.name == section.name) {
                return bad(format!("two sections are named {:?}", section.name));
            }
            if !finite_non_negative(section.start) {
                return bad(format!(
                    "section {:?} start must be finite and >= 0",
                    section.name
                ));
            }
            if i == 0 && section.start != 0.0 {
                return bad(format!("the first section {:?} must start at 0", section.name));
            }
            if i > 0 && section.start <= self.sections[i - 1].start {
                return bad(format!(
                    "section {:?} must start after section {:?}",
                    section.name,
                    self.sections[i - 1].name
                ));
            }
        }
        for section in &self.sections {
            if let SectionNext::Jump(target) = &section.next
                && !self.sections.iter().any(|s| &s.name == target)
            {
                return bad(format!(
                    "section {:?} jumps to unknown section {target:?}",
                    section.name
                ));
            }
        }
        for notify in &self.notifies {
            if notify.name.is_empty() {
                return bad("a notify has no name".into());
            }
            if !finite_non_negative(notify.time) || !finite_non_negative(notify.duration) {
                return bad(format!(
                    "notify {:?} time and duration must be finite and >= 0",
                    notify.name
                ));
            }
            if notify.branch && notify.duration > 0.0 {
                return bad(format!(
                    "branching point {:?} cannot have a duration",
                    notify.name
                ));
            }
        }
        for blend in [self.blend_in, self.blend_out] {
            if !finite_non_negative(blend.time) {
                return bad("blend times must be finite and >= 0".into());
            }
        }
        if let Some(lead) = self.blend_out_lead
            && !finite_non_negative(lead)
        {
            return bad("blend_out_lead must be finite and >= 0".into());
        }
        for window in &self.root_motion_off {
            if !finite_non_negative(window.start) || !(window.end.is_finite() && window.end >= window.start) {
                return bad("root_motion_off windows need finite 0 <= start <= end".into());
            }
        }
        for warp in &self.warps {
            if warp.target.is_empty() {
                return bad("a warp window has no target name".into());
            }
            if !finite_non_negative(warp.start) || !(warp.end.is_finite() && warp.end > warp.start) {
                return bad(format!(
                    "warp window {:?} needs finite 0 <= start < end",
                    warp.target
                ));
            }
            if !(warp.rotation_fraction.is_finite()
                && warp.rotation_fraction > 0.0
                && warp.rotation_fraction <= 1.0)
            {
                return bad(format!(
                    "warp window {:?} rotation_fraction must be in (0, 1]",
                    warp.target
                ));
            }
            if let WarpAnchor::Offset(v) = &warp.anchor
                && !v.iter().all(|c| c.is_finite())
            {
                return bad(format!(
                    "warp window {:?} anchor offset must be finite",
                    warp.target
                ));
            }
            if let WarpAnchor::Bone(name) = &warp.anchor
                && name.is_empty()
            {
                return bad(format!("warp window {:?} anchor bone has no name", warp.target));
            }
        }
        if let Some(curve) = &self.rate_curve {
            let sorted = curve.keys.windows(2).all(|w| w[1].0 > w[0].0);
            let valid = !curve.keys.is_empty()
                && sorted
                && curve
                    .keys
                    .iter()
                    .all(|&(t, r)| finite_non_negative(t) && r.is_finite() && r > 0.0);
            if !valid {
                return bad(
                    "montage_bad_rate_curve: keys need strictly increasing times and positive rates".into(),
                );
            }
        }
        for follow in self
            .sections
            .iter()
            .filter_map(|s| s.follow.as_ref())
            .chain(self.notifies.iter().filter_map(|n| n.follow.as_ref()))
        {
            let known = self
                .tracks
                .get(follow.track)
                .is_some_and(|t| follow.segment < t.segments.len());
            if !known {
                return bad(format!(
                    "follow names track {} segment {}, which does not exist",
                    follow.track, follow.segment
                ));
            }
        }
        for warp in &self.warps {
            if !(warp.rotation_fraction.is_finite()
                && warp.rotation_fraction > 0.0
                && warp.rotation_fraction <= 1.0)
            {
                return bad(format!(
                    "warp window {:?} rotation_fraction must be in (0, 1]",
                    warp.target
                ));
            }
            match &warp.anchor {
                WarpAnchor::Bone(name) if name.is_empty() => {
                    return bad(format!(
                        "warp window {:?} anchors to an empty bone name",
                        warp.target
                    ));
                }
                WarpAnchor::Offset(v) if !v.iter().all(|c| c.is_finite()) => {
                    return bad(format!(
                        "warp window {:?} anchor offset is not finite",
                        warp.target
                    ));
                }
                _ => {}
            }
        }
        if let Some(curve) = &self.rate_curve {
            let sorted = curve.keys.windows(2).all(|w| w[1].0 > w[0].0);
            let sane = curve
                .keys
                .iter()
                .all(|&(t, r)| finite_non_negative(t) && r.is_finite() && r > 0.0);
            if curve.keys.is_empty() || !sorted || !sane {
                return bad("montage_bad_rate_curve: keys must be non-empty, strictly increasing in time, with positive rates".into());
            }
        }
        let segment_exists = |f: &SegmentFollow| {
            self.tracks
                .get(f.track)
                .is_some_and(|t| f.segment < t.segments.len())
        };
        for section in &self.sections {
            if let Some(f) = &section.follow
                && !segment_exists(f)
            {
                return bad(format!(
                    "section {:?} follows a segment that does not exist",
                    section.name
                ));
            }
        }
        for notify in &self.notifies {
            if let Some(f) = &notify.follow
                && !segment_exists(f)
            {
                return bad(format!(
                    "notify {:?} follows a segment that does not exist",
                    notify.name
                ));
            }
        }
        Ok(())
    }

    /// The timeline's length from clip durations alone (no clip data needed): the longest
    /// track, each segment `(end − start) / rate × loops` with `end` clamped to the clip's
    /// duration and `None` meaning the clip's end. `None` when a segment needs a duration
    /// `clip_span` does not know.
    #[must_use]
    pub fn timeline_length(&self, clip_span: &dyn Fn(&str) -> Option<f32>) -> Option<f32> {
        let mut length = 0.0_f32;
        for track in &self.tracks {
            let mut at = 0.0_f32;
            for seg in &track.segments {
                let clip = clip_span(&seg.clip);
                let end = match (seg.end, clip) {
                    (Some(end), Some(clip)) => end.min(clip),
                    (Some(end), None) => end,
                    (None, Some(clip)) => clip,
                    (None, None) => return None,
                };
                let rate = if seg.rate > 0.0 { seg.rate } else { 1.0 };
                at += (end - seg.start).max(0.0) / rate * seg.loops as f32;
            }
            length = length.max(at);
        }
        Some(length)
    }

    /// The sections, notifies, warp windows and root-motion-off windows that lie past `end`
    /// (spec 34 M4), in that order, each list in authored order. A section starting exactly at
    /// `end` is empty, not past it.
    #[must_use]
    pub fn items_past_end(&self, end: f32) -> Vec<PastEndItem> {
        let limit = end + PAST_END_SLACK;
        let item = |kind: &'static str, index: usize, name: &str, time: f32| PastEndItem {
            code: "montage_past_end",
            kind,
            index,
            name: name.to_owned(),
            time,
            end,
        };
        let mut out = Vec::new();
        for (i, s) in self.sections.iter().enumerate() {
            if s.start > limit {
                out.push(item("section", i, &s.name, s.start));
            }
        }
        for (i, n) in self.notifies.iter().enumerate() {
            let last = n.time + n.duration.max(0.0);
            if last > limit {
                out.push(item("notify", i, &n.name, last));
            }
        }
        for (i, w) in self.warps.iter().enumerate() {
            let last = w.start.max(w.end);
            if last > limit {
                out.push(item("warp", i, &w.target, last));
            }
        }
        for (i, w) in self.root_motion_off.iter().enumerate() {
            let last = w.start.max(w.end);
            if last > limit {
                out.push(item("root_motion_off", i, "", last));
            }
        }
        out
    }

    /// Number of sections (an implicit section counts as one).
    pub fn section_count(&self) -> usize {
        self.sections.len().max(1)
    }

    /// Index of the named section (the implicit section is [`DEFAULT_SECTION`]).
    pub fn section_index(&self, name: &str) -> Option<usize> {
        if self.sections.is_empty() {
            return (name == DEFAULT_SECTION).then_some(0);
        }
        self.sections.iter().position(|s| s.name == name)
    }

    /// Name of section `index`.
    pub fn section_name(&self, index: usize) -> &str {
        self.sections
            .get(index)
            .map_or(DEFAULT_SECTION, |s| s.name.as_str())
    }

    /// Start time of section `index`.
    pub fn section_start(&self, index: usize) -> f32 {
        self.sections.get(index).map_or(0.0, |s| s.start)
    }

    /// The authored link of section `index`.
    pub fn section_next(&self, index: usize) -> SectionNext {
        self.sections
            .get(index)
            .map_or(SectionNext::Continue, |s| s.next.clone())
    }

    /// The authored link of section `index`, borrowed. A missing section continues.
    ///
    /// The hot playback walk uses this so a `Jump` does not clone its section name.
    pub(crate) fn section_next_ref(&self, index: usize) -> &SectionNext {
        self.sections.get(index).map_or(&CONTINUE_LINK, |s| &s.next)
    }

    /// The section containing montage time `time` (the last section whose start is not after it).
    pub fn section_at(&self, time: f32) -> usize {
        self.sections.iter().rposition(|s| s.start <= time).unwrap_or(0)
    }

    /// Resolve the montage against loaded clips.
    ///
    /// # Errors
    ///
    /// [`MontageError::MissingClip`] for a clip `lookup` does not know.
    pub fn layout(
        &self,
        mut lookup: impl FnMut(&str) -> Option<Arc<AnimationClip>>,
    ) -> Result<MontageLayout, MontageError> {
        let mut tracks = Vec::with_capacity(self.tracks.len());
        let mut length: f32 = 0.0;
        for track in &self.tracks {
            let mut at = 0.0;
            let mut segments = Vec::with_capacity(track.segments.len());
            for seg in &track.segments {
                let clip = lookup(&seg.clip).ok_or_else(|| MontageError::MissingClip {
                    montage: self.name.clone(),
                    clip: seg.clip.clone(),
                })?;
                let start = seg.start.min(clip.duration).max(0.0);
                let end = seg.end.unwrap_or(clip.duration).min(clip.duration).max(start);
                let lap = (end - start) / seg.rate;
                let len = lap * seg.loops as f32;
                segments.push(PlacedSegment {
                    clip,
                    at,
                    len,
                    lap,
                    start,
                    end,
                    rate: seg.rate,
                    loops: seg.loops,
                });
                at += len;
            }
            length = length.max(at);
            tracks.push(PlacedTrack {
                slot: track.slot.clone(),
                segments,
            });
        }
        let section_ends = (0..self.section_count())
            .map(|i| {
                self.sections
                    .get(i + 1)
                    .map_or(length, |next| next.start.min(length))
            })
            .collect();
        Ok(MontageLayout {
            tracks,
            length,
            section_ends,
        })
    }
}

/// One segment placed on the timeline.
#[derive(Clone, Debug)]
pub struct PlacedSegment {
    /// The clip.
    pub clip: Arc<AnimationClip>,
    /// Montage time the segment starts at.
    pub at: f32,
    /// Montage seconds the segment covers (all loops).
    pub len: f32,
    /// Montage seconds one loop covers.
    pub lap: f32,
    /// Clip-local start.
    pub start: f32,
    /// Clip-local end.
    pub end: f32,
    /// Rate.
    pub rate: f32,
    /// Loop count.
    pub loops: u32,
}

impl PlacedSegment {
    /// Clip-local time at a montage-time offset `local` into the segment (`0..=len`).
    fn clip_time(&self, local: f32) -> f32 {
        if self.lap <= 0.0 {
            return self.start;
        }
        let local = local.clamp(0.0, self.len);
        let lap = ((local / self.lap).floor() as u32).min(self.loops.saturating_sub(1));
        let into = local - lap as f32 * self.lap;
        (self.start + into * self.rate).min(self.end)
    }
}

/// One track placed on the timeline.
#[derive(Clone, Debug)]
pub struct PlacedTrack {
    /// Slot name.
    pub slot: String,
    /// Segments in timeline order.
    pub segments: Vec<PlacedSegment>,
}

/// A piece of one clip a playback step crossed, forward and inside one loop of one segment.
#[derive(Clone, Debug)]
pub struct ClipSpan {
    /// The clip.
    pub clip: Arc<AnimationClip>,
    /// Clip-local time the span starts at.
    pub from: f32,
    /// Clip-local time the span ends at.
    pub to: f32,
    /// The span begins at the start of a loop of its segment that the step entered (so a
    /// notify exactly at that clip time fires).
    pub entered: bool,
}

/// A montage resolved against its clips.
#[derive(Clone, Debug)]
pub struct MontageLayout {
    /// Tracks, parallel to the montage's.
    pub tracks: Vec<PlacedTrack>,
    /// Montage length, seconds.
    pub length: f32,
    /// End time of each section.
    pub section_ends: Vec<f32>,
}

impl MontageLayout {
    /// Sample the named float curve at `montage_time`.
    ///
    /// A segment that covers that time contributes its clip curve. Overlapping segments: the
    /// later track wins. `None` when no covering segment has a curve of that name.
    #[must_use]
    pub fn sample_curve(&self, name: &str, montage_time: f32) -> Option<f32> {
        let mut found = None;
        for track in &self.tracks {
            for seg in &track.segments {
                if montage_time < seg.at || montage_time > seg.at + seg.len {
                    continue;
                }
                let local = (montage_time - seg.at).clamp(0.0, seg.len);
                let time = seg.clip_time(local);
                for curve in &seg.clip.curves {
                    if curve.name == name {
                        found = Some(curve.curve.sample(time));
                    }
                }
            }
        }
        found
    }
}

/// Composes root-motion deltas the way `RootMotionOutput::accumulate` does: each new
/// translation is expressed in the frame the rotation so far reached.
fn compose(acc: (Vec3, Quat), next: (Vec3, Quat)) -> (Vec3, Quat) {
    (acc.0 + acc.1 * next.0, (acc.1 * next.1).normalize())
}

impl MontageLayout {
    /// End time of section `index`.
    pub fn section_end(&self, index: usize) -> f32 {
        self.section_ends.get(index).copied().unwrap_or(self.length)
    }

    /// The clip spans track `track` crosses playing forward from montage time `a` to `b`.
    pub fn spans(&self, track: usize, a: f32, b: f32, out: &mut Vec<ClipSpan>) {
        let Some(track) = self.tracks.get(track) else {
            return;
        };
        if b.is_nan() || a.is_nan() || b <= a {
            return;
        }
        for seg in &track.segments {
            let seg_end = seg.at + seg.len;
            if seg_end <= a || seg.at >= b || seg.lap <= 0.0 {
                continue;
            }
            let lo = a.max(seg.at) - seg.at;
            let hi = b.min(seg_end) - seg.at;
            let first_lap = ((lo / seg.lap).floor() as u32).min(seg.loops.saturating_sub(1));
            for lap in first_lap..seg.loops {
                let lap_start = lap as f32 * seg.lap;
                if lap_start >= hi {
                    break;
                }
                let from_local = lo.max(lap_start);
                let to_local = hi.min(lap_start + seg.lap);
                if to_local > from_local {
                    out.push(ClipSpan {
                        clip: seg.clip.clone(),
                        from: seg.start + (from_local - lap_start) * seg.rate,
                        to: (seg.start + (to_local - lap_start) * seg.rate).min(seg.end),
                        // The step entered this loop (it did not stand at its start already).
                        entered: from_local == lap_start && lap_start + seg.at > a,
                    });
                }
            }
        }
    }

    /// The clip track `track` plays at montage time `time`: the clip, its clip-local time and
    /// the segment's rate (past the end, the last segment at its end).
    pub fn clip_at(&self, track: usize, time: f32) -> Option<(&Arc<AnimationClip>, f32, f32)> {
        let track = self.tracks.get(track)?;
        let seg = track
            .segments
            .iter()
            .find(|s| time < s.at + s.len)
            .or(track.segments.last())?;
        Some((&seg.clip, seg.clip_time(time - seg.at), seg.rate))
    }

    /// Sample track `track` at montage time `time` into `pose` (on top of `bind`). The root is
    /// pinned in place where the clip extracts root motion and `pin_root` is set. Returns
    /// `false` when the track has nothing at that time (the pose is left as the bind pose).
    pub fn sample(&self, track: usize, time: f32, bind: &Pose, pin_root: bool, pose: &mut Pose) -> bool {
        pose.copy_from(bind);
        let Some(track) = self.tracks.get(track) else {
            return false;
        };
        // Past the track's end the last segment holds its final pose.
        let seg = track
            .segments
            .iter()
            .find(|s| time < s.at + s.len)
            .or(track.segments.last());
        let Some(seg) = seg else {
            return false;
        };
        let clip_time = seg.clip_time(time - seg.at);
        seg.clip.sample_pose(clip_time, pose);
        if pin_root && let Some(rm) = &seg.clip.root_motion {
            seg.clip.pin_root_in_place(pose, rm.mode);
        }
        true
    }

    /// Root motion of the main track (track 0) from montage time `a` to `b` (forward), each
    /// clip filtered by its import mode, skipping `off` windows. Rig object space.
    pub fn root_motion_between(&self, a: f32, b: f32, off: &[TimeWindow]) -> (Vec3, Quat) {
        #[cfg(test)]
        perf_work::note_root_vecs();
        let mut cuts = Vec::new();
        let mut spans = Vec::new();
        self.root_motion_between_reuse(a, b, off, &mut cuts, &mut spans)
    }

    /// [`root_motion_between`](Self::root_motion_between) into caller-owned scratch. `cuts` and
    /// `spans` are cleared. A playback reuses them across steps so a tick does not allocate.
    pub(crate) fn root_motion_between_reuse(
        &self,
        a: f32,
        b: f32,
        off: &[TimeWindow],
        cuts: &mut Vec<f32>,
        spans: &mut Vec<ClipSpan>,
    ) -> (Vec3, Quat) {
        let mut acc = (Vec3::ZERO, Quat::IDENTITY);
        if b.is_nan() || a.is_nan() || b <= a {
            return acc;
        }
        // Split [a, b] into the pieces outside every off window.
        cuts.clear();
        cuts.push(a);
        cuts.push(b);
        for w in off {
            for t in [w.start, w.end] {
                if t > a && t < b {
                    cuts.push(t);
                }
            }
        }
        cuts.sort_by(f32::total_cmp);
        for pair in cuts.windows(2) {
            let (lo, hi) = (pair[0], pair[1]);
            let mid = 0.5 * (lo + hi);
            if off.iter().any(|w| mid >= w.start && mid <= w.end) {
                continue;
            }
            spans.clear();
            self.spans(0, lo, hi, spans);
            for span in spans.iter() {
                let Some(rm) = &span.clip.root_motion else {
                    continue;
                };
                if rm.mode == RootMotionMode::None {
                    continue;
                }
                let step = ClipStep {
                    prev: span.from,
                    time: span.to,
                    loops: 0,
                };
                if let Some((t, r)) = span.clip.root_motion_delta_step(&step) {
                    acc = compose(acc, filter_root_motion(rm.mode, t, r));
                }
            }
        }
        acc
    }
}

/// Counts the work a sequential montage query repeats (audit perf G, finding 1). Test-only:
/// production builds do not keep these counters.
#[cfg(test)]
pub(crate) mod perf_work {
    use std::cell::Cell;

    thread_local! {
        static ADVANCES: Cell<u64> = const { Cell::new(0) };
        static JUMPS: Cell<u64> = const { Cell::new(0) };
        static ROOT_VECS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        ADVANCES.with(|c| c.set(0));
        JUMPS.with(|c| c.set(0));
        ROOT_VECS.with(|c| c.set(0));
    }

    pub(crate) fn note_advance() {
        ADVANCES.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(crate) fn note_jump() {
        JUMPS.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(crate) fn note_root_vecs() {
        ROOT_VECS.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(crate) fn advances() -> u64 {
        ADVANCES.with(Cell::get)
    }

    pub(crate) fn jumps() -> u64 {
        JUMPS.with(Cell::get)
    }

    pub(crate) fn root_vecs() -> u64 {
        ROOT_VECS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(name: &str, duration: f32) -> Arc<AnimationClip> {
        Arc::new(AnimationClip::empty(name, duration))
    }

    fn lookup(name: &str) -> Option<Arc<AnimationClip>> {
        match name {
            "a" => Some(clip("a", 1.0)),
            "b" => Some(clip("b", 0.5)),
            _ => None,
        }
    }

    fn two_segment() -> Montage {
        let mut m = Montage::single("combo", "full_body", "a");
        m.tracks[0].segments.push(MontageSegment {
            clip: "b".into(),
            start: 0.0,
            end: None,
            rate: 0.5,
            loops: 2,
        });
        m.sections = vec![
            MontageSection {
                follow: None,
                name: "one".into(),
                start: 0.0,
                next: SectionNext::Continue,
            },
            MontageSection {
                follow: None,
                name: "two".into(),
                start: 1.0,
                next: SectionNext::Jump("two".into()),
            },
        ];
        m
    }

    #[test]
    fn follow_mode_ron_uses_the_enum_names() {
        assert!(ron::from_str::<FollowMode>("Absolute").is_err());
        assert!(ron::from_str::<FollowMode>("Relative").is_err());
        assert!(ron::from_str::<FollowMode>("Proportional").is_err());
        assert_eq!(
            ron::from_str::<FollowMode>("KeepTime").expect("KeepTime"),
            FollowMode::KeepTime
        );
        assert_eq!(
            ron::from_str::<FollowMode>("ShiftWithStart").expect("shift"),
            FollowMode::ShiftWithStart
        );
        let scaled: FollowMode = ron::from_str("ScaleWithLength").expect("scale");
        assert_eq!(scaled, FollowMode::ScaleWithLength);
        let back = ron::to_string(&scaled).expect("ser");
        assert!(back.contains("ScaleWithLength"), "{back}");
    }

    #[test]
    fn layout_places_segments_back_to_back() {
        let layout = two_segment().layout(lookup).expect("layout");
        let segs = &layout.tracks[0].segments;
        assert_eq!(segs[0].at, 0.0);
        assert_eq!(segs[1].at, 1.0);
        // 0.5 s clip at half rate is 1 s per loop, two loops.
        assert!((segs[1].len - 2.0).abs() < 1e-6);
        assert!((layout.length - 3.0).abs() < 1e-6);
        assert_eq!(layout.section_ends, vec![1.0, 3.0]);
    }

    #[test]
    fn spans_split_at_segment_and_loop_boundaries() {
        let layout = two_segment().layout(lookup).expect("layout");
        let mut spans = Vec::new();
        layout.spans(0, 0.5, 2.5, &mut spans);
        let got: Vec<(String, f32, f32, bool)> = spans
            .iter()
            .map(|s| (s.clip.name.clone(), s.from, s.to, s.entered))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".into(), 0.5, 1.0, false),
                ("b".into(), 0.0, 0.5, true),
                ("b".into(), 0.0, 0.25, true),
            ]
        );
    }

    #[test]
    fn missing_clip_is_reported() {
        let m = Montage::single("x", "full_body", "nope");
        assert!(matches!(m.layout(lookup), Err(MontageError::MissingClip { .. })));
    }

    #[test]
    fn ron_round_trips_and_validation_rejects_bad_sections() {
        let m = two_segment();
        let text = m.to_ron_string().expect("write");
        let back = Montage::from_ron_str(&text).expect("read");
        assert_eq!(back, m);
        let mut bad = m.clone();
        bad.sections[1].next = SectionNext::Jump("nowhere".into());
        assert!(bad.validate().is_err());
        let mut late = m.clone();
        late.sections[0].start = 0.1;
        assert!(late.validate().is_err(), "the first section starts at 0");
        let mut unordered = m;
        unordered.sections[1].start = 0.0;
        assert!(unordered.validate().is_err());
        assert!(Montage::from_ron_str("(format_version: 2, name: \"x\", tracks: [])").is_err());
    }

    /// audit-v035-formats 11: an explicit `format_version: 0` loaded (and was stamped current).
    #[test]
    fn version_zero_is_refused() {
        let text = r#"(format_version: 0, name: "hit", tracks: [(slot: "upper", segments: [(clip: "a")])])"#;
        assert!(
            Montage::from_ron_str(text).is_err(),
            "a zero version word is damage"
        );
        assert!(Montage::from_ron_str(&text.replace("format_version: 0", "format_version: 1")).is_ok());
    }

    #[test]
    fn minimal_ron_uses_defaults() {
        let text = r#"(format_version: 1, name: "hit", tracks: [(slot: "upper", segments: [(clip: "a")])])"#;
        let m = Montage::from_ron_str(text).expect("parse");
        assert_eq!(m.rate, 1.0);
        assert!(m.auto_blend_out && m.root_motion);
        assert_eq!(m.tracks[0].segments[0].loops, 1);
        assert_eq!(m.section_name(0), DEFAULT_SECTION);
        assert_eq!(m.section_index(DEFAULT_SECTION), Some(0));
    }

    #[test]
    fn fingerprint_lists_every_struct() {
        let fp = montage_schema_fingerprint();
        for field in [
            "blend_out_lead",
            "root_motion_off",
            "branch",
            "loops",
            "vertical",
            "Jump",
        ] {
            assert!(fp.contains(field), "{field} missing from {fp}");
        }
    }

    #[test]
    fn a_step_rate_curve_is_integrated_exactly() {
        // Rate 2 until montage time 1, then rate 0.5.
        let curve = RateCurve {
            keys: vec![(0.0, 2.0), (1.0, 0.5)],
            interpolation: RateInterpolation::Step,
        };
        // 0.25 s of wall time covers 0.5 montage seconds.
        assert!((curve.advance(0.0, 0.25, 1.0) - 0.5).abs() < 1e-6);
        // From 0.75: 0.125 s of wall time reaches montage time 1, the remaining 0.075 s run at
        // rate 0.5 for 0.0375 more.
        assert!((curve.advance(0.75, 0.2, 1.0) - 0.2875).abs() < 1e-6);
        assert_eq!(curve.advance(0.0, 0.0, 1.0), 0.0);
        assert_eq!(curve.advance(0.0, 0.1, f32::NAN), 0.0);
    }

    #[test]
    fn a_linear_rate_curve_ends_in_the_same_place_at_any_frame_rate() {
        let curve = RateCurve {
            keys: vec![(0.0, 0.5), (1.0, 3.0), (2.0, 1.0)],
            interpolation: RateInterpolation::Linear,
        };
        let end = |steps: usize| {
            let dt = 1.5 / steps as f32;
            let mut t = 0.0;
            for _ in 0..steps {
                t += curve.advance(t, dt, 1.0);
            }
            t
        };
        let (a, b, c) = (end(30), end(60), end(144));
        assert!((a - b).abs() < 1e-3 && (b - c).abs() < 1e-3, "{a} {b} {c}");
    }

    #[test]
    fn a_huge_step_is_integrated_in_bounded_substeps() {
        let constant = RateCurve {
            keys: vec![(0.0, 2.0)],
            interpolation: RateInterpolation::Linear,
        };
        // Constant rate: any substep count is exact, so the bounded count changes nothing.
        assert_eq!(constant.advance(0.0, 60.0, 1.0), 120.0);
        let moved = constant.advance(0.0, 1.0e30, 1.0);
        assert!(moved.is_finite() && moved > 1.0e30, "{moved}");
        // Below the bound the result is what it always was.
        let curve = RateCurve {
            keys: vec![(0.0, 0.5), (1.0, 3.0), (2.0, 1.0)],
            interpolation: RateInterpolation::Linear,
        };
        let short = curve.advance(0.0, 1.5, 1.0);
        assert!(short > 0.0 && short.is_finite());
    }

    #[test]
    fn a_huge_frame_time_is_integrated_with_bounded_work() {
        // Muse L (ADR-1809): a 1e30 s (or 60 s) hitch must not spin for 1e32 substeps. Past the
        // last key the rate is constant, so the coarse answer is still exact there.
        let curve = RateCurve {
            keys: vec![(0.0, 1.0), (1.0, 2.0)],
            interpolation: RateInterpolation::Linear,
        };
        let moved = curve.advance(10.0, 60.0, 1.0);
        assert!((moved - 120.0).abs() < 1e-2, "{moved}");
        assert!(curve.advance(10.0, 1.0e30, 1.0).is_finite());
    }

    #[test]
    fn curve_values_are_clamped_and_edges_hold() {
        let curve = RateCurve {
            keys: vec![(1.0, 0.001), (2.0, 100.0)],
            interpolation: RateInterpolation::Step,
        };
        assert_eq!(curve.rate_at(0.0), RATE_CURVE_LIMITS.0, "before the first key");
        assert_eq!(curve.rate_at(1.5), RATE_CURVE_LIMITS.0);
        assert_eq!(curve.rate_at(9.0), RATE_CURVE_LIMITS.1, "after the last key");
        assert_eq!(RateCurve::default().rate_at(0.3), 1.0);
    }
}
