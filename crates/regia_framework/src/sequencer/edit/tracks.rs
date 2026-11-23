// SPDX-License-Identifier: MPL-2.0
//! Editing the structure of a sequence: adding, removing, renaming and reordering tracks of every
//! kind `.seq.ron` v6 holds, the marker list, keys written from a value at a time, and adding and
//! removing bindings (ADR-9710); duplicating, muting and locking a track and editing event keys
//! (`.seq.ron` v6, ADR-8105). The editor's sequence authoring rows (`regia_editor::seq_tracks`)
//! and their agent methods run the same code.
//!
//! Every edit works on a copy of the sequence and validates the whole copy
//! ([`Sequence::validate`]) before it replaces the original, so a refused edit changes nothing
//! and says why with a lowercase agent error code ([`EditError::code`]). An edit followed by its
//! inverse (add then remove, rename there and back, move there and back) gives back the same
//! sequence, so the saved file is byte-identical.
//!
//! A channel needs at least one key or section, so a new track starts with one
//! ([`starter_track`]). Fades and camera cuts are not `.seq.ron` channels: they live in the
//! `.shots.ron` shot list (ADR-268), so [`TrackKind::Fade`] is refused with
//! `seq_track_fade_in_shots`. A sequence has no explicit length: [`new_sequence`] marks the length
//! with an `end` marker, which sets [`Sequence::duration`] without a format change.
//!
//! A locked track ([`Track::locked`], v6) keeps its keys, sections, name, binding and position:
//! [`apply_track_edit`] refuses to remove, rename or move it and [`apply_event_key_edit`] to change
//! its keys, with `seq_track_locked`; every other editor edit path checks its result with
//! [`check_locked`]. Locking, unlocking, muting and duplicating a locked track are allowed.
//!
//! ```
//! use regia_framework::sequencer::Sequence;
//! use regia_framework::sequencer::edit::tracks::{TrackEdit, TrackKind, TrackSpec, apply_track_edit, starter_track};
//!
//! let mut sequence = Sequence::default();
//! let before = sequence.to_ron().expect("ron");
//! let track = starter_track(&TrackSpec::new(TrackKind::Transform, "cube.position", "cube")).expect("track");
//! let at = apply_track_edit(&mut sequence, TrackEdit::Add { at: None, track }).expect("added");
//! apply_track_edit(&mut sequence, TrackEdit::Remove { index: at }).expect("removed");
//! assert_eq!(sequence.to_ron().expect("ron"), before);
//! ```

use super::EditError;
use crate::sequencer::asset::{
    AudioSection, AudioSpatial, BoolKey, Channel, ClipSection, Ease, EventKey, Marker, ScalarKey, Sequence,
    SoundSource, Spawnable, SubSection, Track, Vec3Key, VideoSection, VideoTarget,
};
use crate::sequencer::sample::ChannelValue;

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// The kinds of track an author can add. Each maps to one [`Channel`] of `.seq.ron` v6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    /// A Vec3 track on `transform.position` (or `transform.rotation` / `transform.scale` when the
    /// spec names that property).
    Transform,
    /// A Float track on a property path.
    Float,
    /// A Vec3 track on a property path.
    Vec3,
    /// A Bool track on a property path.
    Bool,
    /// Animation clip sections on the bound rig.
    Clip,
    /// Flat (2D) sound sections.
    Audio,
    /// Spatial sound sections emitting from the bound entity.
    SpatialAudio,
    /// Sub-sequence sections.
    Sub,
    /// Video sections shown on the bound entity's video player (fullscreen with no binding).
    Video,
    /// A screen fade: not a `.seq.ron` channel (fades are `.shots.ron` data, ADR-268); refused.
    Fade,
    /// Event keys that fire a named event with a payload (`.seq.ron` v6, ADR-8105). The binding
    /// is optional (empty: a sequence-wide event).
    Event,
}

impl TrackKind {
    /// Every kind, in menu order.
    pub const ALL: [TrackKind; 11] = [
        Self::Transform,
        Self::Float,
        Self::Vec3,
        Self::Bool,
        Self::Clip,
        Self::Audio,
        Self::SpatialAudio,
        Self::Sub,
        Self::Video,
        Self::Fade,
        Self::Event,
    ];

    /// The protocol name (`transform`, `float`, `vec3`, `bool`, `clip`, `audio`, `spatial_audio`,
    /// `sub`, `video`, `fade`, `event`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Transform => "transform",
            Self::Float => "float",
            Self::Vec3 => "vec3",
            Self::Bool => "bool",
            Self::Clip => "clip",
            Self::Audio => "audio",
            Self::SpatialAudio => "spatial_audio",
            Self::Sub => "sub",
            Self::Video => "video",
            Self::Fade => "fade",
            Self::Event => "event",
        }
    }

    /// The kind with that protocol name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// `true` when the kind's track names a binding (`target`, a spatial emitter or a video
    /// player) the spec must give.
    #[must_use]
    pub fn needs_binding(self) -> bool {
        matches!(
            self,
            Self::Transform | Self::Float | Self::Vec3 | Self::Bool | Self::Clip | Self::SpatialAudio
        )
    }

    /// `true` for the kinds whose sections name an asset (clip, sound, sub-sequence, video).
    #[must_use]
    pub fn needs_asset(self) -> bool {
        matches!(
            self,
            Self::Clip | Self::Audio | Self::SpatialAudio | Self::Sub | Self::Video
        )
    }
}

/// What a new track starts with: its kind, name and binding, and for keyed kinds the property and
/// first key, for section kinds the asset and the first section's span.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackSpec {
    /// The kind.
    pub kind: TrackKind,
    /// The unique track name.
    pub name: String,
    /// The binding: the `target` of keyed and clip tracks, the emitter of spatial sound, the video
    /// player of a video track (empty: fullscreen). Unused by flat audio and sub-sequence tracks.
    pub binding: String,
    /// Keyed kinds: the property path (default for [`TrackKind::Transform`]:
    /// `transform.position`). Unused by section kinds.
    pub property: String,
    /// Section kinds: the clip name, sound, sub-sequence path or video path. Event tracks: the
    /// first key's event name (empty: the track's name).
    pub asset: String,
    /// Keyed kinds: the first key's time, seconds. Section kinds: the first section's start.
    pub time: f32,
    /// Section kinds: the first section's end, seconds (after `time`).
    pub end: f32,
    /// Keyed kinds: the first key's value (a Float uses `[0]`, a Bool reads non-zero `[0]` as
    /// `true`).
    pub value: [f32; 3],
}

impl TrackSpec {
    /// A spec of `kind` named `name` on `binding`, with defaults: the first key at 0 s with value
    /// zero (one for a `transform.scale` track), the first section from 0 to 1 s.
    #[must_use]
    pub fn new(kind: TrackKind, name: impl Into<String>, binding: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            binding: binding.into(),
            property: String::new(),
            asset: String::new(),
            time: 0.0,
            end: 1.0,
            value: [0.0; 3],
        }
    }
}

/// Builds the track a [`TrackSpec`] describes, with its first key or section. Errors (nothing
/// built): `seq_track_fade_in_shots` (fades are `.shots.ron` data), `seq_track_name_empty`,
/// `seq_track_needs_binding`, `seq_track_needs_property`, `seq_track_needs_asset`,
/// `seq_track_invalid` (the track fails validation: a negative time, a non-finite value, an end
/// before the start).
pub fn starter_track(spec: &TrackSpec) -> Result<Track, EditError> {
    if spec.kind == TrackKind::Fade {
        return refuse(
            "seq_track_fade_in_shots",
            "fades are not a .seq.ron track: they live in the .shots.ron shot list (ADR-268)",
        );
    }
    if spec.name.trim().is_empty() {
        return refuse("seq_track_name_empty", "a track needs a name");
    }
    if spec.kind.needs_binding() && spec.binding.is_empty() {
        return refuse(
            "seq_track_needs_binding",
            format!("a {} track needs a binding", spec.kind.name()),
        );
    }
    if spec.kind.needs_asset() && spec.asset.is_empty() {
        return refuse(
            "seq_track_needs_asset",
            format!("a {} track needs an asset (`asset`)", spec.kind.name()),
        );
    }
    let property = match spec.kind {
        TrackKind::Transform if spec.property.is_empty() => "transform.position".to_string(),
        TrackKind::Transform
            if !matches!(
                spec.property.as_str(),
                "transform.position" | "transform.rotation" | "transform.scale"
            ) =>
        {
            return refuse(
                "seq_track_needs_property",
                format!(
                    "a transform track is transform.position, transform.rotation or transform.scale, not `{}`",
                    spec.property
                ),
            );
        }
        TrackKind::Float | TrackKind::Vec3 | TrackKind::Bool if spec.property.is_empty() => {
            return refuse(
                "seq_track_needs_property",
                format!("a {} track needs a property path", spec.kind.name()),
            );
        }
        _ => spec.property.clone(),
    };
    let (t, end) = (spec.time, spec.end);
    let keys = match spec.kind {
        TrackKind::Transform | TrackKind::Vec3 => {
            Channel::Vec3(vec![Vec3Key::new(t, spec.value, Ease::Bezier)])
        }
        TrackKind::Float => Channel::Float(vec![ScalarKey::new(t, spec.value[0], Ease::Bezier)]),
        TrackKind::Bool => Channel::Bool(vec![BoolKey {
            time: t,
            value: spec.value[0] != 0.0,
        }]),
        TrackKind::Clip => Channel::Clip(vec![ClipSection {
            clip: spec.asset.clone(),
            start: t,
            end,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        }]),
        TrackKind::Audio => Channel::Audio(vec![AudioSection::new(spec.asset.clone(), t, end)]),
        TrackKind::SpatialAudio => {
            let mut section = AudioSection::new(spec.asset.clone(), t, end);
            section.spatial = Some(AudioSpatial::new(SoundSource::Binding(spec.binding.clone())));
            Channel::Audio(vec![section])
        }
        TrackKind::Sub => Channel::Sub(vec![SubSection::new(spec.asset.clone(), t, end)]),
        TrackKind::Video => {
            let mut section = VideoSection::new(spec.asset.clone(), t, end);
            if !spec.binding.is_empty() {
                section.target = VideoTarget::Player(spec.binding.clone());
            }
            Channel::Video(vec![section])
        }
        TrackKind::Fade => unreachable_fade()?,
        TrackKind::Event => {
            let event = if spec.asset.trim().is_empty() {
                spec.name.trim()
            } else {
                spec.asset.trim()
            };
            Channel::Event(vec![EventKey::new(t, event)])
        }
    };
    // Only keyed, clip and event tracks carry `target`; the others name their bindings per section.
    let target = if matches!(
        keys,
        Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Clip(_) | Channel::Event(_)
    ) {
        spec.binding.clone()
    } else {
        String::new()
    };
    let property = if keys.is_keyed() { property } else { String::new() };
    let track = Track {
        name: spec.name.trim().to_string(),
        target,
        property,
        keys,
        enabled: true,
        locked: false,
    };
    if let Err(e) = track.validate() {
        return refuse("seq_track_invalid", e.to_string());
    }
    Ok(track)
}

/// Fades were refused at the top of [`starter_track`]; this keeps the match total without a panic.
fn unreachable_fade() -> Result<Channel, EditError> {
    refuse(
        "seq_track_fade_in_shots",
        "fades are not a .seq.ron track: they live in the .shots.ron shot list (ADR-268)",
    )
}

/// One structural edit of a sequence's track list.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackEdit {
    /// Insert `track` at index `at` (`None`: at the end).
    Add {
        /// Where (`0..=len`); `None` appends.
        at: Option<usize>,
        /// The track ([`starter_track`] builds one).
        track: Track,
    },
    /// Remove the track at `index`.
    Remove {
        /// Track index.
        index: usize,
    },
    /// Rename the track at `index`.
    Rename {
        /// Track index.
        index: usize,
        /// The new, unique, non-empty name.
        name: String,
    },
    /// Move the track at `index` to index `to` (the others keep their order). Order matters: of
    /// two tracks writing the same property, the later one wins.
    Move {
        /// Track index.
        index: usize,
        /// The index it ends at (`0..len`).
        to: usize,
    },
    /// Insert a copy of the track at `index` right after it, under a free name (`name.2`, ...),
    /// unlocked (v6).
    Duplicate {
        /// Track index.
        index: usize,
    },
    /// Lock or unlock the track at `index` ([`Track::locked`], v6).
    Lock {
        /// Track index.
        index: usize,
        /// `true` locks.
        locked: bool,
    },
    /// Mute or unmute the track at `index` (`muted` is `!`[`Track::enabled`]).
    Mute {
        /// Track index.
        index: usize,
        /// `true` mutes.
        muted: bool,
    },
}

/// The refusal of an edit that would change a locked track.
fn locked_refusal<T>(name: &str) -> Result<T, EditError> {
    refuse(
        "seq_track_locked",
        format!(
            "track `{name}` is locked: unlock it to change its keys, sections, name, binding or position"
        ),
    )
}

/// Refuses (`seq_track_locked`) an edit that took `before` to `after` when it changed a track that
/// was locked in `before`: removed or renamed it, or changed its binding, property or keys (or
/// sections). Locking, unlocking and muting are not changes it refuses. Every editor edit path that
/// does not go through [`apply_track_edit`] or [`apply_event_key_edit`] checks its result here.
pub fn check_locked(before: &Sequence, after: &Sequence) -> Result<(), EditError> {
    for track in before.tracks.iter().filter(|t| t.locked) {
        let same = after
            .tracks
            .iter()
            .find(|t| t.name == track.name)
            .is_some_and(|t| {
                t.target == track.target && t.property == track.property && t.keys == track.keys
            });
        if !same {
            return locked_refusal(&track.name);
        }
    }
    Ok(())
}

/// Applies `edit` to `sequence` on a copy, validates it and swaps it in. Returns the index of the
/// edited track (the removed index for a removal, the copy's index for a duplicate). Errors
/// (nothing changed): `seq_track_bad_index`, `seq_track_name_empty`, `seq_track_name_taken`,
/// `seq_track_locked` (removing, renaming or moving a locked track), `seq_track_invalid`.
pub fn apply_track_edit(sequence: &mut Sequence, edit: TrackEdit) -> Result<usize, EditError> {
    let mut next = sequence.clone();
    let len = next.tracks.len();
    let check = |index: usize, len: usize| {
        if index < len {
            Ok(())
        } else {
            refuse(
                "seq_track_bad_index",
                format!("no track {index}: the sequence has {len}"),
            )
        }
    };
    let name_free = |next: &Sequence, name: &str, skip: Option<usize>| {
        if name.trim().is_empty() {
            return refuse("seq_track_name_empty", "a track needs a name");
        }
        if next
            .tracks
            .iter()
            .enumerate()
            .any(|(i, t)| Some(i) != skip && t.name == name)
        {
            return refuse(
                "seq_track_name_taken",
                format!("a track is already called `{name}`"),
            );
        }
        Ok(())
    };
    let at = match edit {
        TrackEdit::Add { at, track } => {
            name_free(&next, &track.name, None)?;
            let at = at.unwrap_or(len);
            if at > len {
                return refuse(
                    "seq_track_bad_index",
                    format!("cannot insert at {at}: the sequence has {len} tracks"),
                );
            }
            next.tracks.insert(at, track);
            at
        }
        TrackEdit::Remove { index } => {
            check(index, len)?;
            if next.tracks[index].locked {
                return locked_refusal(&next.tracks[index].name);
            }
            next.tracks.remove(index);
            index
        }
        TrackEdit::Rename { index, name } => {
            check(index, len)?;
            if next.tracks[index].locked {
                return locked_refusal(&next.tracks[index].name);
            }
            let name = name.trim().to_string();
            name_free(&next, &name, Some(index))?;
            next.tracks[index].name = name;
            index
        }
        TrackEdit::Move { index, to } => {
            check(index, len)?;
            check(to, len)?;
            if next.tracks[index].locked && index != to {
                return locked_refusal(&next.tracks[index].name);
            }
            let track = next.tracks.remove(index);
            next.tracks.insert(to, track);
            to
        }
        TrackEdit::Duplicate { index } => {
            check(index, len)?;
            let mut copy = next.tracks[index].clone();
            copy.name = free_track_name(&next, &copy.name);
            copy.locked = false;
            next.tracks.insert(index + 1, copy);
            index + 1
        }
        TrackEdit::Lock { index, locked } => {
            check(index, len)?;
            next.tracks[index].locked = locked;
            index
        }
        TrackEdit::Mute { index, muted } => {
            check(index, len)?;
            next.tracks[index].enabled = !muted;
            index
        }
    };
    if let Err(e) = next.validate() {
        return refuse("seq_track_invalid", e.to_string());
    }
    *sequence = next;
    Ok(at)
}

/// One edit of a sequence's marker list (markers fire in time order whatever their list order;
/// the list order is what the file and the panel show).
#[derive(Clone, Debug, PartialEq)]
pub enum MarkerEdit {
    /// Insert `marker` at index `at` (`None`: at the end).
    Add {
        /// Where (`0..=len`); `None` appends.
        at: Option<usize>,
        /// The marker.
        marker: Marker,
    },
    /// Remove the marker at `index`.
    Remove {
        /// Marker index.
        index: usize,
    },
    /// Rename the marker at `index`.
    Rename {
        /// Marker index.
        index: usize,
        /// The new, unique, non-empty name.
        name: String,
    },
    /// Move the marker at `index` to index `to` in the list.
    Move {
        /// Marker index.
        index: usize,
        /// The index it ends at.
        to: usize,
    },
}

/// Applies `edit` to the marker list on a copy, validated. Returns the edited marker's index.
/// Errors (nothing changed): `seq_marker_bad_index`, `seq_marker_name_empty` (empty or blank),
/// `seq_marker_name_taken`, `seq_marker_invalid` (a negative or non-finite time).
pub fn apply_marker_edit(sequence: &mut Sequence, edit: MarkerEdit) -> Result<usize, EditError> {
    let mut next = sequence.clone();
    let len = next.markers.len();
    let check = |index: usize| {
        if index < len {
            Ok(())
        } else {
            refuse(
                "seq_marker_bad_index",
                format!("no marker {index}: the sequence has {len}"),
            )
        }
    };
    let name_free = |next: &Sequence, name: &str, skip: Option<usize>| {
        if name.trim().is_empty() {
            return refuse("seq_marker_name_empty", "a marker needs a name");
        }
        if next
            .markers
            .iter()
            .enumerate()
            .any(|(i, m)| Some(i) != skip && m.name == name)
        {
            return refuse(
                "seq_marker_name_taken",
                format!("a marker is already called `{name}`"),
            );
        }
        Ok(())
    };
    let at = match edit {
        MarkerEdit::Add { at, marker } => {
            name_free(&next, &marker.name, None)?;
            let at = at.unwrap_or(len);
            if at > len {
                return refuse(
                    "seq_marker_bad_index",
                    format!("cannot insert at {at}: the sequence has {len} markers"),
                );
            }
            next.markers.insert(at, marker);
            at
        }
        MarkerEdit::Remove { index } => {
            check(index)?;
            next.markers.remove(index);
            index
        }
        MarkerEdit::Rename { index, name } => {
            check(index)?;
            name_free(&next, &name, Some(index))?;
            next.markers[index].name = name;
            index
        }
        MarkerEdit::Move { index, to } => {
            check(index)?;
            check(to)?;
            let marker = next.markers.remove(index);
            next.markers.insert(to, marker);
            to
        }
    };
    if let Err(e) = next.validate() {
        return refuse("seq_marker_invalid", e.to_string());
    }
    *sequence = next;
    Ok(at)
}

/// The name of the marker [`new_sequence`] puts at the length.
pub const END_MARKER: &str = "end";

/// An empty `.seq.ron` v5: `name`, `fps`, no tracks, and when `length > 0` an [`END_MARKER`]
/// marker at `length` (the sequence's length: a sequence ends at its last key, section or
/// marker). Errors: `seq_new_name_empty` (an empty or blank `name`), `seq_new_bad_fps` (not in
/// `(0, 1000]`), `seq_new_bad_length` (negative or not finite).
pub fn new_sequence(name: &str, length: f32, fps: f32) -> Result<Sequence, EditError> {
    if name.trim().is_empty() {
        return refuse("seq_new_name_empty", "a sequence needs a name");
    }
    if !(fps.is_finite() && fps > 0.0 && fps <= 1000.0) {
        return refuse("seq_new_bad_fps", format!("fps {fps} must be in (0, 1000]"));
    }
    if !(length.is_finite() && length >= 0.0) {
        return refuse(
            "seq_new_bad_length",
            format!("length {length} must be zero or more"),
        );
    }
    let mut sequence = Sequence {
        name: name.to_string(),
        fps,
        ..Sequence::default()
    };
    if length > 0.0 {
        sequence.markers.push(Marker {
            name: END_MARKER.to_string(),
            time: length,
            payload: String::new(),
        });
    }
    if let Err(e) = sequence.validate() {
        return refuse("seq_new_bad_length", e.to_string());
    }
    Ok(sequence)
}

/// Writes `value` at `time` into a keyed channel: the key at exactly `time` takes the value (its
/// ease and tangent kept), or a new [`Ease::Bezier`] auto-tangent key is inserted in time order.
/// Returns the key's index. Errors (nothing changed): `seq_key_wrong_kind` (the value's kind is
/// not the channel's), `seq_curve_out_of_range` (a negative or non-finite time, a non-finite
/// value).
pub fn set_key(channel: &mut Channel, time: f32, value: ChannelValue) -> Result<usize, EditError> {
    if !(time.is_finite() && time >= 0.0) {
        return refuse(
            "seq_curve_out_of_range",
            format!("time {time} must be zero or more"),
        );
    }
    let finite = match value {
        ChannelValue::Float(v) => v.is_finite(),
        ChannelValue::Vec3(v) => v.iter().all(|c| c.is_finite()),
        ChannelValue::Bool(_) => true,
    };
    if !finite {
        return refuse("seq_curve_out_of_range", "key values must be finite");
    }
    fn place<K>(
        keys: &mut Vec<K>,
        time: f32,
        at_time: impl Fn(&K) -> f32,
        new: K,
        set: impl FnOnce(&mut K),
    ) -> usize {
        if let Some(i) = keys.iter().position(|k| at_time(k) == time) {
            set(&mut keys[i]);
            return i;
        }
        let at = keys.iter().take_while(|k| at_time(k) < time).count();
        keys.insert(at, new);
        at
    }
    match (channel, value) {
        (Channel::Float(keys), ChannelValue::Float(v)) => Ok(place(
            keys,
            time,
            |k| k.time,
            ScalarKey::new(time, v, Ease::Bezier),
            |k| k.value = v,
        )),
        (Channel::Vec3(keys), ChannelValue::Vec3(v)) => Ok(place(
            keys,
            time,
            |k| k.time,
            Vec3Key::new(time, v, Ease::Bezier),
            |k| k.value = v,
        )),
        (Channel::Bool(keys), ChannelValue::Bool(v)) => Ok(place(
            keys,
            time,
            |k| k.time,
            BoolKey { time, value: v },
            |k| k.value = v,
        )),
        (channel, value) => refuse(
            "seq_key_wrong_kind",
            format!(
                "a {} track cannot take a {} value",
                channel.kind(),
                match value {
                    ChannelValue::Float(_) => "Float",
                    ChannelValue::Vec3(_) => "Vec3",
                    ChannelValue::Bool(_) => "Bool",
                }
            ),
        ),
    }
}

/// What refers to one binding name in a sequence (see [`binding_uses`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BindingUses {
    /// Keyed and clip tracks whose `target` is the binding.
    pub tracks: usize,
    /// Spatial sound sections emitting from the binding.
    pub sounds: usize,
    /// Video sections shown on the binding.
    pub videos: usize,
    /// Sub-sequence binding-table entries that map a name to the binding.
    pub sub_entries: usize,
    /// `true` when the binding is a spawnable of this sequence.
    pub spawnable: bool,
}

impl BindingUses {
    /// Tracks, sections and entries that use the binding (the spawnable entry not counted).
    #[must_use]
    pub fn references(&self) -> usize {
        self.tracks + self.sounds + self.videos + self.sub_entries
    }
}

/// Counts what in `sequence` refers to the binding `name` (disabled tracks included).
#[must_use]
pub fn binding_uses(sequence: &Sequence, name: &str) -> BindingUses {
    let mut uses = BindingUses {
        spawnable: sequence.spawnables.iter().any(|s| s.binding == name),
        ..BindingUses::default()
    };
    for track in &sequence.tracks {
        match &track.keys {
            Channel::Float(_)
            | Channel::Vec3(_)
            | Channel::Bool(_)
            | Channel::Clip(_)
            | Channel::Event(_) => {
                uses.tracks += usize::from(!track.target.is_empty() && track.target == name);
            }
            Channel::Audio(sections) => {
                uses.sounds += sections
                    .iter()
                    .filter(|s| {
                        matches!(&s.spatial, Some(AudioSpatial { source: SoundSource::Binding(b), .. }) if b == name)
                    })
                    .count();
            }
            Channel::Video(sections) => {
                uses.videos += sections
                    .iter()
                    .filter(|s| s.target.binding() == Some(name))
                    .count();
            }
            Channel::Sub(sections) => {
                uses.sub_entries += sections
                    .iter()
                    .map(|s| s.bindings.values().filter(|v| *v == name).count())
                    .sum::<usize>();
            }
        }
    }
    uses
}

/// Adds a spawnable binding (`.seq.ron` v5, ADR-2948). Errors (nothing changed):
/// `seq_binding_empty`, `seq_binding_in_use` (a spawnable of that name exists),
/// `seq_spawnable_invalid` (the entry fails validation: a bad prefab path, range).
pub fn add_spawnable(sequence: &mut Sequence, spawnable: Spawnable) -> Result<(), EditError> {
    if spawnable.binding.is_empty() {
        return refuse("seq_binding_empty", "binding names must not be empty");
    }
    if sequence.spawnables.iter().any(|s| s.binding == spawnable.binding) {
        return refuse(
            "seq_binding_in_use",
            format!("`{}` is already a spawnable of this sequence", spawnable.binding),
        );
    }
    let mut next = sequence.clone();
    next.spawnables.push(spawnable);
    if let Err(e) = next.validate() {
        return refuse("seq_spawnable_invalid", e.to_string());
    }
    *sequence = next;
    Ok(())
}

/// Removes the binding `name` from `sequence`: its spawnable entry, and with `cascade` everything
/// that uses it (its keyed and clip tracks, its spatial sound and video sections, a track left
/// with no sections, and sub-sequence table entries mapping to it). Returns what was removed.
/// Errors (nothing changed): `seq_binding_empty`, `seq_binding_unknown` (nothing refers to it and
/// it is not a spawnable), `seq_binding_used` (something uses it and `cascade` is off; the
/// message counts the uses), `seq_track_invalid`.
pub fn remove_binding(sequence: &mut Sequence, name: &str, cascade: bool) -> Result<BindingUses, EditError> {
    if name.is_empty() {
        return refuse("seq_binding_empty", "binding names must not be empty");
    }
    let uses = binding_uses(sequence, name);
    if uses.references() == 0 && !uses.spawnable {
        return refuse(
            "seq_binding_unknown",
            format!("nothing in the sequence refers to the binding `{name}`"),
        );
    }
    if uses.references() > 0 && !cascade {
        return refuse(
            "seq_binding_used",
            format!(
                "`{name}` is used by {} track(s), {} spatial sound(s), {} video section(s) and {} sub-sequence entr(ies); remove those first or pass cascade",
                uses.tracks, uses.sounds, uses.videos, uses.sub_entries
            ),
        );
    }
    let mut next = sequence.clone();
    next.spawnables.retain(|s| s.binding != name);
    next.tracks.retain_mut(|track| match &mut track.keys {
        Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Clip(_) | Channel::Event(_) => {
            track.target != name
        }
        Channel::Audio(sections) => {
            let before = sections.len();
            sections.retain(
                |s| !matches!(&s.spatial, Some(AudioSpatial { source: SoundSource::Binding(b), .. }) if b == name),
            );
            before == sections.len() || !sections.is_empty()
        }
        Channel::Video(sections) => {
            let before = sections.len();
            sections.retain(|s| s.target.binding() != Some(name));
            before == sections.len() || !sections.is_empty()
        }
        Channel::Sub(sections) => {
            for section in sections.iter_mut() {
                section.bindings.retain(|_, v| v != name);
            }
            true
        }
    });
    if let Err(e) = next.validate() {
        return refuse("seq_track_invalid", e.to_string());
    }
    *sequence = next;
    Ok(uses)
}

/// One edit of an event track's keys (`.seq.ron` v6, ADR-8105).
#[derive(Clone, Debug, PartialEq)]
pub enum EventKeyEdit {
    /// Insert `key` in time order (after the keys at the same time, so it fires after them).
    Add {
        /// The key.
        key: EventKey,
    },
    /// Remove the key at `index` (an event track keeps at least one key).
    Remove {
        /// Key index.
        index: usize,
    },
    /// Replace the key at `index` (moved to its time order when its time changed).
    Set {
        /// Key index.
        index: usize,
        /// The new key.
        key: EventKey,
    },
}

/// Applies `edit` to the keys of the event track at `track` on a copy, validated. Returns the
/// edited key's index (the removed index for a removal). Errors (nothing changed):
/// `seq_track_bad_index`, `seq_event_wrong_kind` (not an event track), `seq_track_locked`,
/// `seq_event_bad_index`, `seq_event_invalid` (an empty event name, a negative or non-finite time,
/// the last key removed).
pub fn apply_event_key_edit(
    sequence: &mut Sequence,
    track: usize,
    edit: EventKeyEdit,
) -> Result<usize, EditError> {
    let mut next = sequence.clone();
    let len = next.tracks.len();
    let Some(target) = next.tracks.get_mut(track) else {
        return refuse(
            "seq_track_bad_index",
            format!("no track {track}: the sequence has {len}"),
        );
    };
    if target.locked {
        return locked_refusal(&target.name);
    }
    if !matches!(target.keys, Channel::Event(_)) {
        return refuse(
            "seq_event_wrong_kind",
            format!(
                "track `{}` is a {} track, not an Event track",
                target.name,
                target.keys.kind()
            ),
        );
    }
    let Channel::Event(keys) = &mut target.keys else {
        return refuse("seq_event_wrong_kind", "not an Event track");
    };
    let place = |keys: &mut Vec<EventKey>, key: EventKey| {
        let at = keys.iter().take_while(|k| k.time <= key.time).count();
        keys.insert(at, key);
        at
    };
    let bad_index = |index: usize, count: usize| {
        refuse(
            "seq_event_bad_index",
            format!("no event key {index}: the track has {count}"),
        )
    };
    let at = match edit {
        EventKeyEdit::Add { key } => place(keys, key),
        EventKeyEdit::Remove { index } => {
            if index >= keys.len() {
                return bad_index(index, keys.len());
            }
            keys.remove(index);
            index
        }
        EventKeyEdit::Set { index, key } => {
            if index >= keys.len() {
                return bad_index(index, keys.len());
            }
            keys.remove(index);
            place(keys, key)
        }
    };
    if let Err(e) = next.validate() {
        return refuse("seq_event_invalid", e.to_string());
    }
    *sequence = next;
    Ok(at)
}

/// A track name not taken in `sequence`: `base`, else `base.2`, `base.3`, ...
#[must_use]
pub fn free_track_name(sequence: &Sequence, base: &str) -> String {
    let taken = |n: &str| sequence.tracks.iter().any(|t| t.name == n);
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|i| format!("{base}.{i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| base.to_string())
}

/// The index of the keyed track that writes `property` on `binding` with a channel of `kind`
/// (`Float`, `Vec3` or `Bool`): the last one, since the later of two such tracks wins.
#[must_use]
pub fn find_property_track(sequence: &Sequence, binding: &str, property: &str, kind: &str) -> Option<usize> {
    sequence
        .tracks
        .iter()
        .rposition(|t| t.target == binding && t.property == property && t.keys.kind() == kind)
}

/// Writes `value` at `time` on the keyed track of `binding` and `property`, creating the track
/// (named `name`, made unique) when there is none. Returns `(track index, key index, created)`.
/// One edit: validated on a copy, nothing changed on an error (`seq_key_wrong_kind`,
/// `seq_curve_out_of_range`, `seq_track_invalid`, `seq_binding_empty`).
pub fn key_property(
    sequence: &mut Sequence,
    binding: &str,
    property: &str,
    name: &str,
    time: f32,
    value: ChannelValue,
) -> Result<(usize, usize, bool), EditError> {
    if binding.is_empty() {
        return refuse("seq_binding_empty", "binding names must not be empty");
    }
    let kind = match value {
        ChannelValue::Float(_) => "Float",
        ChannelValue::Vec3(_) => "Vec3",
        ChannelValue::Bool(_) => "Bool",
    };
    let mut next = sequence.clone();
    let (track, created) = match find_property_track(&next, binding, property, kind) {
        Some(i) => (i, false),
        None => {
            let keys = match value {
                ChannelValue::Float(_) => Channel::Float(Vec::new()),
                ChannelValue::Vec3(_) => Channel::Vec3(Vec::new()),
                ChannelValue::Bool(_) => Channel::Bool(Vec::new()),
            };
            next.tracks.push(Track {
                name: free_track_name(&next, name),
                target: binding.to_string(),
                property: property.to_string(),
                keys,
                enabled: true,
                locked: false,
            });
            (next.tracks.len() - 1, true)
        }
    };
    let key = set_key(&mut next.tracks[track].keys, time, value)?;
    if let Err(e) = next.validate() {
        return refuse("seq_track_invalid", e.to_string());
    }
    *sequence = next;
    Ok((track, key, created))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: TrackKind) -> TrackSpec {
        let mut spec = TrackSpec::new(kind, format!("t_{}", kind.name()), "hero");
        spec.property = match kind {
            TrackKind::Float => "light.intensity".into(),
            TrackKind::Vec3 => "light.color".into(),
            TrackKind::Bool => "visible".into(),
            _ => String::new(),
        };
        spec.asset = match kind {
            TrackKind::Clip => "walk".into(),
            TrackKind::Audio | TrackKind::SpatialAudio => "chime".into(),
            TrackKind::Sub => "cine/fx.seq.ron".into(),
            TrackKind::Video => "movies/a.webm".into(),
            _ => String::new(),
        };
        spec
    }

    fn base() -> Sequence {
        let mut seq = new_sequence("intro", 4.0, 30.0).expect("new");
        let track = starter_track(&TrackSpec::new(TrackKind::Transform, "move", "hero")).expect("t");
        apply_track_edit(&mut seq, TrackEdit::Add { at: None, track }).expect("add");
        seq
    }

    #[test]
    fn add_then_remove_of_every_kind_leaves_the_file_byte_identical() {
        for kind in TrackKind::ALL {
            let mut seq = base();
            let before = seq.to_ron().expect("ron");
            if kind == TrackKind::Fade {
                assert_eq!(
                    starter_track(&spec(kind)).expect_err("fade").code,
                    "seq_track_fade_in_shots"
                );
                continue;
            }
            let track = starter_track(&spec(kind)).unwrap_or_else(|e| panic!("{}: {e}", kind.name()));
            let at = apply_track_edit(&mut seq, TrackEdit::Add { at: Some(0), track }).expect("add");
            assert_eq!(at, 0);
            assert_ne!(seq.to_ron().expect("ron"), before, "{} added", kind.name());
            // The file round-trips with the new track.
            let text = seq.to_ron().expect("ron");
            assert_eq!(Sequence::from_ron(&text).expect("parse"), seq, "{}", kind.name());
            apply_track_edit(&mut seq, TrackEdit::Remove { index: 0 }).expect("remove");
            assert_eq!(seq.to_ron().expect("ron"), before, "{} removed", kind.name());
        }
        // Markers: add then remove.
        let mut seq = base();
        let before = seq.to_ron().expect("ron");
        let marker = Marker {
            name: "door".into(),
            time: 1.5,
            payload: "open".into(),
        };
        let at = apply_marker_edit(&mut seq, MarkerEdit::Add { at: None, marker }).expect("add");
        apply_marker_edit(&mut seq, MarkerEdit::Remove { index: at }).expect("remove");
        assert_eq!(seq.to_ron().expect("ron"), before);
    }

    #[test]
    fn rename_and_move_round_trip_and_refusals_change_nothing() {
        let mut seq = base();
        let t = starter_track(&spec(TrackKind::Float)).expect("t");
        apply_track_edit(&mut seq, TrackEdit::Add { at: None, track: t }).expect("add");
        let before = seq.clone();
        apply_track_edit(&mut seq, TrackEdit::Move { index: 1, to: 0 }).expect("move");
        assert_eq!(seq.tracks[0].name, "t_float");
        apply_track_edit(&mut seq, TrackEdit::Move { index: 0, to: 1 }).expect("move back");
        assert_eq!(seq, before);
        apply_track_edit(
            &mut seq,
            TrackEdit::Rename {
                index: 0,
                name: "walk".into(),
            },
        )
        .expect("rename");
        assert_eq!(seq.tracks[0].name, "walk");
        let snapshot = seq.clone();
        let code = |seq: &mut Sequence, e: TrackEdit| apply_track_edit(seq, e).expect_err("refused").code;
        assert_eq!(
            code(
                &mut seq,
                TrackEdit::Rename {
                    index: 0,
                    name: "t_float".into()
                }
            ),
            "seq_track_name_taken"
        );
        assert_eq!(
            code(
                &mut seq,
                TrackEdit::Rename {
                    index: 0,
                    name: " ".into()
                }
            ),
            "seq_track_name_empty"
        );
        assert_eq!(
            code(&mut seq, TrackEdit::Remove { index: 9 }),
            "seq_track_bad_index"
        );
        assert_eq!(
            code(&mut seq, TrackEdit::Move { index: 0, to: 5 }),
            "seq_track_bad_index"
        );
        let dup = starter_track(&TrackSpec::new(TrackKind::Transform, "walk", "x")).expect("t");
        assert_eq!(
            code(&mut seq, TrackEdit::Add { at: None, track: dup }),
            "seq_track_name_taken"
        );
        assert_eq!(seq, snapshot, "refused edits change nothing");
        // Starter refusals.
        let mut no_binding = spec(TrackKind::Clip);
        no_binding.binding.clear();
        assert_eq!(
            starter_track(&no_binding).expect_err("binding").code,
            "seq_track_needs_binding"
        );
        let mut no_asset = spec(TrackKind::Sub);
        no_asset.asset.clear();
        assert_eq!(
            starter_track(&no_asset).expect_err("asset").code,
            "seq_track_needs_asset"
        );
        let mut bad_span = spec(TrackKind::Audio);
        bad_span.end = 0.0;
        assert_eq!(
            starter_track(&bad_span).expect_err("span").code,
            "seq_track_invalid"
        );
        let mut no_property = spec(TrackKind::Float);
        no_property.property.clear();
        assert_eq!(
            starter_track(&no_property).expect_err("property").code,
            "seq_track_needs_property"
        );
        assert_eq!(TrackKind::parse("spatial_audio"), Some(TrackKind::SpatialAudio));
        assert_eq!(TrackKind::parse("nope"), None);
    }

    #[test]
    fn marker_edits_rename_move_and_refuse() {
        let mut seq = base();
        for (name, time) in [("a", 1.0), ("b", 2.0)] {
            let marker = Marker {
                name: name.into(),
                time,
                payload: String::new(),
            };
            apply_marker_edit(&mut seq, MarkerEdit::Add { at: None, marker }).expect("add");
        }
        let names = |s: &Sequence| s.markers.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&seq), ["end", "a", "b"]);
        apply_marker_edit(&mut seq, MarkerEdit::Move { index: 2, to: 0 }).expect("move");
        assert_eq!(names(&seq), ["b", "end", "a"]);
        apply_marker_edit(
            &mut seq,
            MarkerEdit::Rename {
                index: 2,
                name: "c".into(),
            },
        )
        .expect("rename");
        let keep = seq.clone();
        let code = |seq: &mut Sequence, e: MarkerEdit| apply_marker_edit(seq, e).expect_err("refused").code;
        assert_eq!(
            code(
                &mut seq,
                MarkerEdit::Rename {
                    index: 0,
                    name: "c".into()
                }
            ),
            "seq_marker_name_taken"
        );
        let bad = Marker {
            name: "neg".into(),
            time: -1.0,
            payload: String::new(),
        };
        assert_eq!(
            code(
                &mut seq,
                MarkerEdit::Add {
                    at: None,
                    marker: bad
                }
            ),
            "seq_marker_invalid"
        );
        assert_eq!(
            code(&mut seq, MarkerEdit::Remove { index: 7 }),
            "seq_marker_bad_index"
        );
        assert_eq!(seq, keep);
    }

    #[test]
    fn a_used_binding_is_refused_without_cascade_and_cascade_removes_every_use() {
        let mut seq = base();
        for kind in [TrackKind::Float, TrackKind::SpatialAudio, TrackKind::Video] {
            let t = starter_track(&spec(kind)).expect("t");
            apply_track_edit(&mut seq, TrackEdit::Add { at: None, track: t }).expect("add");
        }
        // A flat sound on the same track as nothing: stays.
        let flat = starter_track(&spec(TrackKind::Audio)).expect("t");
        apply_track_edit(
            &mut seq,
            TrackEdit::Add {
                at: None,
                track: flat,
            },
        )
        .expect("add");
        let mut sub = starter_track(&spec(TrackKind::Sub)).expect("t");
        if let Channel::Sub(s) = &mut sub.keys {
            s[0].bindings.insert("inner".into(), "hero".into());
        }
        apply_track_edit(&mut seq, TrackEdit::Add { at: None, track: sub }).expect("add");
        let before = seq.clone();
        let uses = binding_uses(&seq, "hero");
        assert_eq!(
            uses,
            BindingUses {
                tracks: 2,
                sounds: 1,
                videos: 1,
                sub_entries: 1,
                spawnable: false
            }
        );
        let err = remove_binding(&mut seq, "hero", false).expect_err("used");
        assert_eq!(err.code, "seq_binding_used");
        assert_eq!(seq, before, "a refused removal changes nothing");
        assert_eq!(
            remove_binding(&mut seq, "nobody", true)
                .expect_err("unknown")
                .code,
            "seq_binding_unknown"
        );
        let removed = remove_binding(&mut seq, "hero", true).expect("cascade");
        assert_eq!(removed, uses);
        assert_eq!(binding_uses(&seq, "hero"), BindingUses::default());
        let names: Vec<&str> = seq.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["t_audio", "t_sub"], "tracks left with nothing are gone");
        // A spawnable binding with no uses goes without cascade.
        let mut seq = base();
        add_spawnable(&mut seq, Spawnable::new("crate", "props/crate.prefab.ron", 0.0)).expect("spawnable");
        assert_eq!(
            add_spawnable(&mut seq, Spawnable::new("crate", "props/crate.prefab.ron", 0.0))
                .expect_err("twice")
                .code,
            "seq_binding_in_use"
        );
        assert_eq!(
            add_spawnable(&mut seq, Spawnable::new("bad", "../x.prefab.ron", 0.0))
                .expect_err("bad")
                .code,
            "seq_spawnable_invalid"
        );
        let removed = remove_binding(&mut seq, "crate", false).expect("unused spawnable");
        assert!(removed.spawnable);
        assert!(seq.spawnables.is_empty());
    }

    #[test]
    fn keys_from_values_create_the_track_once_and_overwrite_a_key_at_the_same_time() {
        let mut seq = new_sequence("s", 0.0, 24.0).expect("new");
        assert!(seq.markers.is_empty(), "no end marker for a zero length");
        let (t, k, created) = key_property(
            &mut seq,
            "cube",
            "transform.position",
            "cube.position",
            0.0,
            ChannelValue::Vec3([0.0, 0.0, 0.0]),
        )
        .expect("key");
        assert_eq!((t, k, created), (0, 0, true));
        let (t, k, created) = key_property(
            &mut seq,
            "cube",
            "transform.position",
            "cube.position",
            2.0,
            ChannelValue::Vec3([4.0, 0.0, 0.0]),
        )
        .expect("key");
        assert_eq!((t, k, created), (0, 1, false));
        // The same time again overwrites.
        key_property(
            &mut seq,
            "cube",
            "transform.position",
            "cube.position",
            2.0,
            ChannelValue::Vec3([6.0, 0.0, 0.0]),
        )
        .expect("key");
        assert_eq!(seq.tracks[0].keys.len(), 2);
        assert_eq!(
            seq.sample(2.0).values[0].value,
            ChannelValue::Vec3([6.0, 0.0, 0.0])
        );
        // A second track of the same name is made unique.
        let (t, _, created) = key_property(
            &mut seq,
            "lamp",
            "light.intensity",
            "cube.position",
            0.0,
            ChannelValue::Float(5.0),
        )
        .expect("key");
        assert!(created);
        assert_eq!(seq.tracks[t].name, "cube.position.2");
        let keep = seq.clone();
        assert_eq!(
            set_key(&mut seq.tracks[0].keys, 1.0, ChannelValue::Float(1.0))
                .expect_err("kind")
                .code,
            "seq_key_wrong_kind"
        );
        assert_eq!(
            key_property(&mut seq, "cube", "x", "x", -1.0, ChannelValue::Float(1.0))
                .expect_err("time")
                .code,
            "seq_curve_out_of_range"
        );
        assert_eq!(seq, keep);
        assert_eq!(
            new_sequence("s", 1.0, 0.0).expect_err("fps").code,
            "seq_new_bad_fps"
        );
        assert_eq!(
            new_sequence("s", f32::NAN, 30.0).expect_err("length").code,
            "seq_new_bad_length"
        );
        let with_end = new_sequence("s", 3.5, 30.0).expect("new");
        assert_eq!(with_end.duration(), 3.5);
    }

    /// Lane w7-editor-smalls-z (4): a blank marker name is refused like a blank track name
    /// (`seq_marker_name_empty`, nothing changed), and `new_sequence` refuses an empty or blank
    /// name with `seq_new_name_empty`.
    #[test]
    fn blank_marker_and_sequence_names_are_refused() {
        let mut seq = base();
        let keep = seq.clone();
        for name in ["", " ", "\t"] {
            let e = apply_marker_edit(
                &mut seq,
                MarkerEdit::Rename {
                    index: 0,
                    name: name.into(),
                },
            )
            .expect_err("blank rename");
            assert_eq!(e.code, "seq_marker_name_empty", "{name:?}");
            let marker = Marker {
                name: name.into(),
                time: 1.0,
                payload: String::new(),
            };
            let e = apply_marker_edit(&mut seq, MarkerEdit::Add { at: None, marker }).expect_err("blank add");
            assert_eq!(e.code, "seq_marker_name_empty", "{name:?}");
        }
        assert_eq!(seq, keep);
        for name in ["", "  "] {
            assert_eq!(
                new_sequence(name, 1.0, 30.0).expect_err("blank").code,
                "seq_new_name_empty",
                "{name:?}"
            );
        }
        assert_eq!(new_sequence(" s ", 1.0, 30.0).expect("named").name, " s ");
    }

    #[test]
    fn a_locked_track_refuses_edits_with_its_code() {
        let mut seq = base();
        let t = starter_track(&spec(TrackKind::Float)).expect("t");
        apply_track_edit(&mut seq, TrackEdit::Add { at: None, track: t }).expect("add");
        apply_track_edit(
            &mut seq,
            TrackEdit::Lock {
                index: 0,
                locked: true,
            },
        )
        .expect("lock");
        assert!(seq.tracks[0].locked);
        let locked = seq.clone();
        for edit in [
            TrackEdit::Remove { index: 0 },
            TrackEdit::Rename {
                index: 0,
                name: "other".into(),
            },
            TrackEdit::Move { index: 0, to: 1 },
        ] {
            let e = apply_track_edit(&mut seq, edit.clone()).expect_err("locked");
            assert_eq!(e.code, "seq_track_locked", "{edit:?}: {}", e.message);
            assert_eq!(seq, locked, "{edit:?} changed nothing");
        }
        // Edits that leave the locked track's content alone still work: mute, duplicate, and
        // editing another track.
        apply_track_edit(
            &mut seq,
            TrackEdit::Mute {
                index: 0,
                muted: true,
            },
        )
        .expect("mute");
        assert!(!seq.tracks[0].enabled);
        let copy = apply_track_edit(&mut seq, TrackEdit::Duplicate { index: 0 }).expect("duplicate");
        assert_eq!(copy, 1);
        assert_eq!(seq.tracks[1].name, "move.2");
        assert!(!seq.tracks[1].locked, "a duplicate starts unlocked");
        assert_eq!(seq.tracks[1].keys, seq.tracks[0].keys);
        apply_track_edit(
            &mut seq,
            TrackEdit::Rename {
                index: 1,
                name: "copy".into(),
            },
        )
        .expect("rename the copy");
        // `check_locked` refuses any other path's change of a locked track's keys or binding.
        let mut changed = seq.clone();
        changed.tracks[0].target = "villain".into();
        assert_eq!(
            check_locked(&seq, &changed).expect_err("binding").code,
            "seq_track_locked"
        );
        let mut changed = seq.clone();
        if let Channel::Vec3(keys) = &mut changed.tracks[0].keys {
            keys[0].time = 0.5;
        }
        assert_eq!(
            check_locked(&seq, &changed).expect_err("keys").code,
            "seq_track_locked"
        );
        let mut changed = seq.clone();
        changed.tracks.remove(0);
        assert_eq!(
            check_locked(&seq, &changed).expect_err("removed").code,
            "seq_track_locked"
        );
        let mut changed = seq.clone();
        changed.tracks[0].locked = false;
        changed.tracks[0].enabled = true;
        changed.tracks[1].name = "renamed".into();
        check_locked(&seq, &changed).expect("unlock, unmute and another track's edit are allowed");
        // Unlocked, the same edits apply.
        apply_track_edit(
            &mut seq,
            TrackEdit::Lock {
                index: 0,
                locked: false,
            },
        )
        .expect("unlock");
        apply_track_edit(
            &mut seq,
            TrackEdit::Rename {
                index: 0,
                name: "moved".into(),
            },
        )
        .expect("rename");
    }

    #[test]
    fn event_tracks_start_with_a_key_and_their_keys_edit_in_time_order() {
        let mut seq = base();
        let mut spec = TrackSpec::new(TrackKind::Event, "cues", "");
        spec.time = 1.0;
        let track = starter_track(&spec).expect("event track");
        assert_eq!(track.keys, Channel::Event(vec![EventKey::new(1.0, "cues")]));
        assert!(track.target.is_empty(), "a sequence-wide event track");
        let at = apply_track_edit(&mut seq, TrackEdit::Add { at: None, track }).expect("add");
        let mut key = EventKey::new(1.0, "door");
        key.payload = "open".into();
        let i = apply_event_key_edit(&mut seq, at, EventKeyEdit::Add { key }).expect("add key");
        assert_eq!(i, 1, "a key at an existing time goes after it (fires after it)");
        let i = apply_event_key_edit(
            &mut seq,
            at,
            EventKeyEdit::Add {
                key: EventKey::new(0.5, "early"),
            },
        )
        .expect("add");
        assert_eq!(i, 0);
        let i = apply_event_key_edit(
            &mut seq,
            at,
            EventKeyEdit::Set {
                index: 0,
                key: EventKey::new(2.0, "late"),
            },
        )
        .expect("set");
        assert_eq!(i, 2, "a retimed key moves to its time order");
        let text = seq.to_ron().expect("ron");
        assert!(text.contains("payload: \"open\""), "{text}");
        assert_eq!(Sequence::from_ron(&text).expect("parse"), seq);
        for (edit, code) in [
            (EventKeyEdit::Remove { index: 9 }, "seq_event_bad_index"),
            (
                EventKeyEdit::Add {
                    key: EventKey::new(1.0, ""),
                },
                "seq_event_invalid",
            ),
            (
                EventKeyEdit::Add {
                    key: EventKey::new(-1.0, "x"),
                },
                "seq_event_invalid",
            ),
        ] {
            let same = seq.clone();
            assert_eq!(
                apply_event_key_edit(&mut seq, at, edit)
                    .expect_err("refused")
                    .code,
                code
            );
            assert_eq!(seq, same);
        }
        assert_eq!(
            apply_event_key_edit(&mut seq, 0, EventKeyEdit::Remove { index: 0 })
                .expect_err("kind")
                .code,
            "seq_event_wrong_kind"
        );
        apply_track_edit(
            &mut seq,
            TrackEdit::Lock {
                index: at,
                locked: true,
            },
        )
        .expect("lock");
        assert_eq!(
            apply_event_key_edit(&mut seq, at, EventKeyEdit::Remove { index: 0 })
                .expect_err("locked")
                .code,
            "seq_track_locked"
        );
        apply_track_edit(
            &mut seq,
            TrackEdit::Lock {
                index: at,
                locked: false,
            },
        )
        .expect("unlock");
        for _ in 0..3 {
            apply_event_key_edit(&mut seq, at, EventKeyEdit::Remove { index: 1 }).ok();
        }
        assert_eq!(
            apply_event_key_edit(&mut seq, at, EventKeyEdit::Remove { index: 0 })
                .expect_err("last")
                .code,
            "seq_event_invalid",
            "an event track keeps one key"
        );
    }
}
