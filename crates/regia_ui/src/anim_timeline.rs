// SPDX-License-Identifier: MPL-2.0
//! Timeline animation assets (ADR-273 T04, ADR-1842): named animations made of keyframe tracks,
//! with reverse, loop, ping-pong and speed, played by the existing tween system.
//!
//! A [`UiAnimLibrary`] is a `.uianim.ron` file (`format_version: 2`; a v1 file or one without the
//! field reads as is and is written back as v2) holding named [`UiAnimDef`]s. [`UiAnimDef::compile`] turns one into a [`TweenPlan`]: every track becomes a
//! sequence of segment tweens (so [`TweenPlan::sample`] is the pure, deterministic sampler and the
//! game clock, pause and fixed step reproduce a playback), and the tracks run in parallel. Loops
//! are unrolled into a finite number of repeats (at most [`MAX_LOOPS`]); an endless loop is not
//! representable, which keeps the play-back reproducible and the plan finite.
//!
//! Time is in seconds; a key's `ease` shapes the segment that *ends* at that key. Reverse plays
//! the keys backwards and keeps each segment's ease as authored.
//!
//! Version 2 (ADR-8900) adds frame events: an [`AnimEvent`] names a point on the animation's
//! timeline. [`UiAnimDef::compile_events`] places every event once per pass that crosses it (loops,
//! ping-pong passes, reverse and speed applied), and [`UiAnimDef::to_tween`] hands the plan and the
//! events to a [`UiTween`]: the tween system sends a [`crate::tween::UiAnimEventFired`] message for
//! each event exactly once when playback crosses it, also when one frame's step jumps over several
//! events or the end (reduced motion, a long frame).
//!
//! The agent surface is `ui.play_anim` (`book/agents/ui-timeline.md`).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::color::UiColor;
use crate::tween::{Ease, Tween, TweenEvent, TweenPlan, TweenProperty, TweenValue, UiTween};

/// The `format_version` this build writes and reads (2 since ADR-8900: frame events).
pub const UIANIM_FORMAT_VERSION: u32 = 2;
/// Most frame events on one animation.
pub const MAX_EVENTS: usize = 256;
/// Longest event name or payload, in bytes.
pub const MAX_EVENT_TEXT: usize = 256;
/// Most loop repeats of one playback.
pub const MAX_LOOPS: u32 = 64;
/// Most keys on one track.
pub const MAX_KEYS: usize = 1024;
/// Most tracks in one animation.
pub const MAX_TRACKS: usize = 32;
/// Slowest and fastest speed multipliers.
pub const SPEED_RANGE: (f32, f32) = (0.05, 20.0);

/// What happens after the last key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    /// Play once and hold the last values.
    #[default]
    Once,
    /// Repeat from the start `loops` times in all.
    Loop,
    /// Alternate forward and backward, `loops` passes in all.
    PingPong,
}

/// A key value: a number (opacity, px, scale) or a linear RGBA color.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnimValue {
    /// A number.
    Number(f32),
    /// Linear-space red, green, blue, alpha.
    Color([f32; 4]),
}

impl AnimValue {
    fn tween_value(self) -> TweenValue {
        match self {
            Self::Number(n) => TweenValue::Number(n),
            Self::Color(c) => TweenValue::Color(UiColor(c)),
        }
    }

    fn finite(self) -> bool {
        match self {
            Self::Number(n) => n.is_finite(),
            Self::Color(c) => c.iter().all(|v| v.is_finite()),
        }
    }
}

/// One keyframe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimKey {
    /// Seconds from the start of the animation.
    pub time: f32,
    /// The value at `time`.
    pub value: AnimValue,
    /// Easing of the segment that ends here (a [`Ease::parse`] name; default linear).
    #[serde(default)]
    pub ease: Option<String>,
}

/// One animated property.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimTrack {
    /// A [`TweenProperty::parse`] name (`"opacity"`, `"translate_x"`, ...).
    pub property: String,
    /// Keys in strictly ascending time order.
    pub keys: Vec<AnimKey>,
}

/// A frame event (v2, ADR-8900): fires once each time playback crosses `time`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimEvent {
    /// Seconds from the start of one pass at speed 1, from 0 to the animation's
    /// [`UiAnimDef::duration`].
    pub time: f32,
    /// What game code matches on: 1 to [`MAX_EVENT_TEXT`] bytes, no control characters.
    pub name: String,
    /// Free text handed to the handler with the name (default empty).
    #[serde(default)]
    pub payload: String,
}

fn default_speed() -> f32 {
    1.0
}

fn default_loops() -> u32 {
    1
}

/// A named animation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiAnimDef {
    /// The animations' tracks, played in parallel.
    pub tracks: Vec<AnimTrack>,
    /// Behavior after the last key.
    #[serde(default)]
    pub mode: LoopMode,
    /// Passes for `loop` and `ping_pong` (ignored for `once`).
    #[serde(default = "default_loops")]
    pub loops: u32,
    /// Playback speed multiplier.
    #[serde(default = "default_speed")]
    pub speed: f32,
    /// Frame events (v2; empty and not written when there are none). Any order; events at the same
    /// time fire in the order listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<AnimEvent>,
}

/// Why an animation or library was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AnimError {
    /// The animation has no tracks, or too many.
    #[error("an animation needs 1 to {MAX_TRACKS} tracks")]
    Tracks,
    /// A track names a property that does not exist.
    #[error("unknown property `{0}`")]
    Property(String),
    /// A track has no keys or too many.
    #[error("track `{0}` needs 1 to {MAX_KEYS} keys")]
    Keys(String),
    /// Key times are negative, not finite or not strictly ascending.
    #[error("track `{0}` has key times that are not finite, non-negative and strictly ascending")]
    Times(String),
    /// A value is not finite or does not fit the property (color vs number).
    #[error("track `{0}` has a value that does not fit its property")]
    Value(String),
    /// An ease name is unknown.
    #[error("unknown ease `{0}`")]
    Ease(String),
    /// Speed is outside the allowed range or not finite.
    #[error("speed must be from {} to {}", SPEED_RANGE.0, SPEED_RANGE.1)]
    Speed,
    /// `loops` is 0 or above [`MAX_LOOPS`].
    #[error("loops must be from 1 to {MAX_LOOPS}")]
    Loops,
    /// The file could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),
    /// A frame event is out of range: more than [`MAX_EVENTS`], a time that is not finite or lies
    /// outside `0..=duration`, or an empty, too long or control-character name or payload. Holds
    /// the event's index.
    #[error(
        "event {0} needs a finite time from 0 to the animation length and a 1 to {MAX_EVENT_TEXT} byte name"
    )]
    Event(usize),
    /// The file's `format_version` is newer than this build reads.
    #[error("format_version {found} is newer than the supported {UIANIM_FORMAT_VERSION}")]
    Version {
        /// What the file says.
        found: u32,
    },
}

struct Segment {
    start: f32,
    end: f32,
    from: TweenValue,
    to: TweenValue,
    ease: Ease,
}

impl UiAnimDef {
    /// Checks everything [`Self::compile`] relies on.
    pub fn validate(&self) -> Result<(), AnimError> {
        if self.tracks.is_empty() || self.tracks.len() > MAX_TRACKS {
            return Err(AnimError::Tracks);
        }
        if !self.speed.is_finite() || self.speed < SPEED_RANGE.0 || self.speed > SPEED_RANGE.1 {
            return Err(AnimError::Speed);
        }
        if self.loops == 0 || self.loops > MAX_LOOPS {
            return Err(AnimError::Loops);
        }
        for track in &self.tracks {
            let property = TweenProperty::parse(&track.property)
                .ok_or_else(|| AnimError::Property(track.property.clone()))?;
            if track.keys.is_empty() || track.keys.len() > MAX_KEYS {
                return Err(AnimError::Keys(track.property.clone()));
            }
            let mut last = f32::NEG_INFINITY;
            for key in &track.keys {
                if !key.time.is_finite() || key.time < 0.0 || key.time <= last {
                    return Err(AnimError::Times(track.property.clone()));
                }
                last = key.time;
                if !key.value.finite() || !key.value.tween_value().fits(property) {
                    return Err(AnimError::Value(track.property.clone()));
                }
                if let Some(name) = &key.ease
                    && Ease::parse(name).is_none()
                {
                    return Err(AnimError::Ease(name.clone()));
                }
            }
        }
        if self.events.len() > MAX_EVENTS {
            return Err(AnimError::Event(MAX_EVENTS));
        }
        let total = self.duration();
        let text_ok = |s: &str, may_be_empty: bool| {
            (may_be_empty || !s.is_empty()) && s.len() <= MAX_EVENT_TEXT && !s.chars().any(char::is_control)
        };
        for (i, event) in self.events.iter().enumerate() {
            if !event.time.is_finite()
                || event.time < 0.0
                || event.time > total
                || !text_ok(&event.name, false)
                || !text_ok(&event.payload, true)
            {
                return Err(AnimError::Event(i));
            }
        }
        Ok(())
    }

    /// Length of one pass in seconds at speed 1 (the latest key time of any track).
    pub fn duration(&self) -> f32 {
        self.tracks
            .iter()
            .filter_map(|t| t.keys.last())
            .map(|k| k.time)
            .fold(0.0, f32::max)
    }

    /// Total playback length in seconds for `speed_override` (or the definition's speed), `reverse`
    /// and the loop settings.
    pub fn playback_duration(&self, speed_override: Option<f32>) -> f32 {
        let speed = speed_override.unwrap_or(self.speed).max(SPEED_RANGE.0);
        let passes = if self.mode == LoopMode::Once {
            1
        } else {
            self.loops
        };
        self.duration() * passes as f32 / speed
    }

    fn segments(track: &AnimTrack, reverse: bool, total: f32) -> Vec<Segment> {
        let mut keys: Vec<(f32, TweenValue, Ease)> = track
            .keys
            .iter()
            .map(|k| {
                (
                    k.time,
                    k.value.tween_value(),
                    k.ease.as_deref().and_then(Ease::parse).unwrap_or(Ease::Linear),
                )
            })
            .collect();
        if reverse {
            // Time runs backwards over the whole animation; a segment keeps the ease it was authored with.
            let eases: Vec<Ease> = keys.iter().map(|k| k.2).collect();
            keys.reverse();
            let n = keys.len();
            for (i, k) in keys.iter_mut().enumerate() {
                k.0 = total - k.0;
                // The segment ending at reversed key i is the original segment starting at that key
                // (original index n-1-i), which was eased by the key after it.
                let orig = n - 1 - i;
                k.2 = if orig + 1 < n {
                    eases[orig + 1]
                } else {
                    Ease::Linear
                };
            }
        }
        keys.windows(2)
            .map(|w| Segment {
                start: w[0].0,
                end: w[1].0,
                from: w[0].1,
                to: w[1].1,
                ease: w[1].2,
            })
            .collect()
    }

    /// Compiles the animation to a tween plan. `reverse` plays the keys backwards; `speed_override`
    /// replaces the definition's speed.
    pub fn compile(&self, reverse: bool, speed_override: Option<f32>) -> Result<TweenPlan, AnimError> {
        let tracks = self.compile_passes(reverse, speed_override)?;
        Ok(TweenPlan::Parallel(
            tracks
                .into_iter()
                .map(|passes| TweenPlan::Sequence(passes.into_iter().flatten().collect()))
                .collect(),
        ))
    }

    /// The validated speed for `speed_override` (or the definition's speed).
    fn checked_speed(&self, speed_override: Option<f32>) -> Result<f32, AnimError> {
        let speed = speed_override.unwrap_or(self.speed);
        if !speed.is_finite() || speed < SPEED_RANGE.0 || speed > SPEED_RANGE.1 {
            return Err(AnimError::Speed);
        }
        Ok(speed)
    }

    /// How many passes the loop settings play.
    fn passes(&self) -> u32 {
        if self.mode == LoopMode::Once {
            1
        } else {
            self.loops
        }
    }

    /// The steps of [`Self::compile`], per track and per pass (track, pass, steps). The plan is
    /// each track's passes flattened into one sequence; [`Self::compile_events`] reads the pass
    /// boundaries from the same step lengths.
    fn compile_passes(
        &self,
        reverse: bool,
        speed_override: Option<f32>,
    ) -> Result<Vec<Vec<Vec<TweenPlan>>>, AnimError> {
        self.validate()?;
        let speed = self.checked_speed(speed_override)?;
        let total = self.duration();
        let passes = self.passes();
        let mut tracks = Vec::new();
        for track in &self.tracks {
            let property = TweenProperty::parse(&track.property)
                .ok_or_else(|| AnimError::Property(track.property.clone()))?;
            let mut track_passes: Vec<Vec<TweenPlan>> = Vec::with_capacity(passes as usize);
            for pass in 0..passes {
                let mut steps: Vec<TweenPlan> = Vec::new();
                let backwards = reverse ^ (self.mode == LoopMode::PingPong && pass % 2 == 1);
                let first_value = if backwards {
                    track.keys.last().map(|k| k.value)
                } else {
                    track.keys.first().map(|k| k.value)
                };
                let Some(first) = first_value.map(AnimValue::tween_value) else {
                    return Err(AnimError::Keys(track.property.clone()));
                };
                let segments = Self::segments(track, backwards, total);
                // Hold the first value until the first key, and pad to the animation length at the end.
                let lead = segments.first().map_or_else(
                    || {
                        if backwards {
                            total - track.keys[0].time
                        } else {
                            track.keys[0].time
                        }
                    },
                    |s| s.start,
                );
                if lead > 0.0 {
                    steps.push(Tween::to(property, first, lead / speed).from(first).into());
                }
                let mut at = lead;
                let mut held = first;
                for s in &segments {
                    steps.push(
                        Tween::to(property, s.to, (s.end - s.start) / speed)
                            .from(s.from)
                            .ease(s.ease)
                            .into(),
                    );
                    at = s.end;
                    held = s.to;
                }
                if total - at > 1e-6 {
                    steps.push(Tween::to(property, held, (total - at) / speed).from(held).into());
                }
                track_passes.push(steps);
            }
            tracks.push(track_passes);
        }
        Ok(tracks)
    }

    /// Places the frame events on the playback timeline of [`Self::compile`] with the same
    /// `reverse` and `speed_override`: each event once per pass (a `loop`/`ping_pong` animation
    /// repeats them per pass; a backward pass meets them at `duration - time`, in reverse order),
    /// sorted by playback time. Events at the same time keep the order they meet in.
    ///
    /// Pass boundaries are the running sums of the plan's own step lengths (the longest track at
    /// each boundary), so the times never decrease from one pass to the next, an event authored
    /// at a pass end lands exactly on that boundary, and the last one is at most
    /// [`TweenPlan::duration`] (ADR-10132). A sample at the plan's duration therefore sees every
    /// event that playback fires.
    pub fn compile_events(
        &self,
        reverse: bool,
        speed_override: Option<f32>,
    ) -> Result<Vec<TweenEvent>, AnimError> {
        self.validate()?;
        let speed = self.checked_speed(speed_override)?;
        if self.events.is_empty() {
            return Ok(Vec::new());
        }
        let tracks = self.compile_passes(reverse, speed_override)?;
        let total = self.duration();
        let passes = self.passes();
        // Where each pass ends on the plan's clock: the same left-to-right f32 sums
        // `TweenPlan::duration` adds up per track, and the longest track (the plan's
        // `Parallel` takes the maximum). Each track's running sum never decreases, so neither
        // does the maximum, and the last entry is the plan's duration bit for bit.
        let mut pass_end = vec![0.0f32; passes as usize];
        for track in &tracks {
            let mut acc = 0.0f32;
            for (end, steps) in pass_end.iter_mut().zip(track) {
                for step in steps {
                    acc += step.duration();
                }
                *end = end.max(acc);
            }
        }
        // Authored order sorted by time (stable, so equal times keep the listed order).
        let mut order: Vec<&AnimEvent> = self.events.iter().collect();
        order.sort_by(|a, b| a.time.total_cmp(&b.time));
        let mut out = Vec::with_capacity(order.len() * passes as usize);
        let mut start = 0.0f32;
        for (pass, &end) in (0..passes).zip(&pass_end) {
            let backwards = reverse ^ (self.mode == LoopMode::PingPong && pass % 2 == 1);
            let mut place = |e: &AnimEvent| {
                let local = if backwards { total - e.time } else { e.time };
                // Inside the pass: an event at the pass end is the boundary itself, and rounding
                // never carries an earlier one past it.
                let at = if local >= total {
                    end
                } else {
                    (start + local / speed).max(start).min(end)
                };
                out.push(TweenEvent {
                    at,
                    name: e.name.clone(),
                    payload: e.payload.clone(),
                });
            };
            if backwards {
                for &e in order.iter().rev() {
                    place(e);
                }
            } else {
                for &e in &order {
                    place(e);
                }
            }
            start = end;
        }
        Ok(out)
    }

    /// A [`UiTween`] that plays the animation and fires its frame events ([`Self::compile`] plus
    /// [`Self::compile_events`]). Insert it on the target node.
    pub fn to_tween(&self, reverse: bool, speed_override: Option<f32>) -> Result<UiTween, AnimError> {
        let plan = self.compile(reverse, speed_override)?;
        let events = self.compile_events(reverse, speed_override)?;
        Ok(UiTween::new(plan).with_events(events))
    }
}

/// The serde shape of `.uianim.ron` for `cargo xtask check-formats` (`docs/formats/uianim-ron.md`,
/// ADR-2316): the field names of [`UiAnimLibrary`], [`UiAnimDef`], [`AnimTrack`], [`AnimKey`] and (v2,
/// ADR-8900) [`AnimEvent`] and the [`LoopMode`] spellings. [`AnimValue`] is untagged (a number or four
/// numbers), so it adds no names.
#[must_use]
pub fn uianim_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<UiAnimLibrary>(),
        serde_field_names::<UiAnimDef>(),
        serde_field_names::<AnimTrack>(),
        serde_field_names::<AnimKey>(),
        serde_field_names::<AnimEvent>(),
        serde_variant_names::<LoopMode>(),
    ])
}

/// The versions [`UiAnimLibrary::from_ron`] migrates from: 1 (v2 only adds the optional `events`
/// list, so a v1 file reads unchanged and takes the new number; ADR-8900).
#[must_use]
pub fn uianim_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The version of a `.uianim.ron` without `format_version`.
fn implicit_v1() -> u32 {
    1
}

/// A `.uianim.ron` document: named animations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Resource)]
pub struct UiAnimLibrary {
    /// Always [`UIANIM_FORMAT_VERSION`] when written. A file without the field is a v1 file (the
    /// version probe always read it so; the full parse now does too, ADR-8900).
    #[serde(default = "implicit_v1")]
    pub format_version: u32,
    /// Animations by name.
    pub animations: BTreeMap<String, UiAnimDef>,
}

impl Default for UiAnimLibrary {
    fn default() -> Self {
        Self {
            format_version: UIANIM_FORMAT_VERSION,
            animations: BTreeMap::new(),
        }
    }
}

impl UiAnimLibrary {
    /// Parses and validates a `.uianim.ron` document. A v1 file (or one without `format_version`)
    /// is migrated: its body is already a valid v2 body, so only the number moves to
    /// [`UIANIM_FORMAT_VERSION`]. A newer version is [`AnimError::Version`].
    pub fn from_ron(text: &str) -> Result<Self, AnimError> {
        #[derive(Deserialize)]
        struct Probe {
            format_version: Option<u32>,
        }
        let options =
            ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        let probe: Probe = options
            .from_str(text)
            .map_err(|e| AnimError::Parse(e.to_string()))?;
        let found = probe.format_version.unwrap_or(UIANIM_FORMAT_VERSION);
        if found > UIANIM_FORMAT_VERSION {
            return Err(AnimError::Version { found });
        }
        let mut lib: Self = options
            .from_str(text)
            .map_err(|e| AnimError::Parse(e.to_string()))?;
        lib.format_version = UIANIM_FORMAT_VERSION;
        for def in lib.animations.values() {
            def.validate()?;
        }
        Ok(lib)
    }

    /// Writes the document as pretty RON.
    pub fn to_ron_string(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(time: f32, v: f32, ease: Option<&str>) -> AnimKey {
        AnimKey {
            time,
            value: AnimValue::Number(v),
            ease: ease.map(str::to_owned),
        }
    }

    fn fade_slide() -> UiAnimDef {
        UiAnimDef {
            tracks: vec![
                AnimTrack {
                    property: "opacity".into(),
                    keys: vec![num(0.0, 0.0, None), num(1.0, 1.0, None)],
                },
                AnimTrack {
                    property: "translate_y".into(),
                    keys: vec![num(0.5, 20.0, None), num(1.0, 0.0, None)],
                },
            ],
            mode: LoopMode::Once,
            loops: 1,
            speed: 1.0,
            events: Vec::new(),
        }
    }

    fn value_of(plan: &TweenPlan, t: f32, p: TweenProperty) -> Option<f32> {
        let mut out = Vec::new();
        plan.sample(t, &mut out);
        out.iter()
            .rev()
            .find(|(q, _)| *q == p)
            .and_then(|(_, v)| match v {
                TweenValue::Number(n) => Some(*n),
                TweenValue::Color(_) => None,
            })
    }

    #[test]
    fn once_samples_keys_and_holds_the_end() {
        let plan = fade_slide().compile(false, None).unwrap();
        assert!((value_of(&plan, 0.25, TweenProperty::Opacity).unwrap() - 0.25).abs() < 1e-5);
        assert_eq!(
            value_of(&plan, 0.25, TweenProperty::TranslateY),
            Some(20.0),
            "held until its first key"
        );
        assert!((value_of(&plan, 0.75, TweenProperty::TranslateY).unwrap() - 10.0).abs() < 1e-4);
        assert_eq!(value_of(&plan, 5.0, TweenProperty::Opacity), Some(1.0));
        assert_eq!(value_of(&plan, 5.0, TweenProperty::TranslateY), Some(0.0));
        assert!((plan.duration() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn reverse_runs_backwards() {
        let plan = fade_slide().compile(true, None).unwrap();
        assert_eq!(value_of(&plan, 0.0, TweenProperty::Opacity), Some(1.0));
        assert!((value_of(&plan, 0.25, TweenProperty::Opacity).unwrap() - 0.75).abs() < 1e-5);
        assert_eq!(value_of(&plan, 2.0, TweenProperty::Opacity), Some(0.0));
        // translate_y: 0 -> 20 over the first half (the mirror of 20 -> 0), then held.
        assert!((value_of(&plan, 0.25, TweenProperty::TranslateY).unwrap() - 10.0).abs() < 1e-4);
        assert_eq!(value_of(&plan, 0.75, TweenProperty::TranslateY), Some(20.0));
        assert_eq!(value_of(&plan, 1.0, TweenProperty::TranslateY), Some(20.0));
    }

    #[test]
    fn speed_scales_time() {
        let plan = fade_slide().compile(false, Some(2.0)).unwrap();
        assert!((plan.duration() - 0.5).abs() < 1e-5);
        assert!((value_of(&plan, 0.25, TweenProperty::Opacity).unwrap() - 0.5).abs() < 1e-5);
        assert!((fade_slide().playback_duration(Some(2.0)) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn loop_and_ping_pong_unroll() {
        let mut def = fade_slide();
        def.mode = LoopMode::Loop;
        def.loops = 3;
        let plan = def.compile(false, None).unwrap();
        assert!((plan.duration() - 3.0).abs() < 1e-4);
        assert!(
            (value_of(&plan, 1.25, TweenProperty::Opacity).unwrap() - 0.25).abs() < 1e-4,
            "second pass restarts"
        );
        def.mode = LoopMode::PingPong;
        def.loops = 2;
        let plan = def.compile(false, None).unwrap();
        assert!(
            (value_of(&plan, 1.25, TweenProperty::Opacity).unwrap() - 0.75).abs() < 1e-4,
            "second pass runs back"
        );
        assert_eq!(value_of(&plan, 9.0, TweenProperty::Opacity), Some(0.0));
    }

    #[test]
    fn eased_segment_and_colors() {
        let def = UiAnimDef {
            tracks: vec![AnimTrack {
                property: "background".into(),
                keys: vec![
                    AnimKey {
                        time: 0.0,
                        value: AnimValue::Color([0.0, 0.0, 0.0, 1.0]),
                        ease: None,
                    },
                    AnimKey {
                        time: 1.0,
                        value: AnimValue::Color([1.0, 1.0, 1.0, 1.0]),
                        ease: Some("quad_in".into()),
                    },
                ],
            }],
            mode: LoopMode::Once,
            loops: 1,
            speed: 1.0,
            events: Vec::new(),
        };
        let plan = def.compile(false, None).unwrap();
        let mut out = Vec::new();
        plan.sample(0.5, &mut out);
        match out.last() {
            Some((TweenProperty::Background, TweenValue::Color(c))) => assert!((c.0[0] - 0.25).abs() < 1e-5),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_definitions_are_refused() {
        let mut d = fade_slide();
        d.tracks[0].property = "nope".into();
        assert!(matches!(d.validate(), Err(AnimError::Property(_))));
        let mut d = fade_slide();
        d.tracks[0].keys[1].time = 0.0;
        assert!(matches!(d.validate(), Err(AnimError::Times(_))));
        let mut d = fade_slide();
        d.tracks[0].keys[0].value = AnimValue::Color([0.0; 4]);
        assert!(matches!(d.validate(), Err(AnimError::Value(_))));
        let mut d = fade_slide();
        d.tracks[0].keys[0].ease = Some("wobble".into());
        assert!(matches!(d.validate(), Err(AnimError::Ease(_))));
        let mut d = fade_slide();
        d.speed = 0.0;
        assert_eq!(d.validate(), Err(AnimError::Speed));
        let mut d = fade_slide();
        d.loops = MAX_LOOPS + 1;
        assert_eq!(d.validate(), Err(AnimError::Loops));
        let mut d = fade_slide();
        d.tracks.clear();
        assert_eq!(d.validate(), Err(AnimError::Tracks));
        assert_eq!(fade_slide().compile(false, Some(f32::NAN)), Err(AnimError::Speed));
    }

    #[test]
    fn ron_round_trip_and_version_gate() {
        let mut lib = UiAnimLibrary::default();
        lib.animations.insert("pop".into(), fade_slide());
        let text = lib.to_ron_string().unwrap();
        assert_eq!(UiAnimLibrary::from_ron(&text).unwrap(), lib);
        let newer = text.replace(
            &format!("format_version: {UIANIM_FORMAT_VERSION}"),
            "format_version: 9",
        );
        assert_eq!(
            UiAnimLibrary::from_ron(&newer),
            Err(AnimError::Version { found: 9 })
        );
        assert!(matches!(UiAnimLibrary::from_ron("(("), Err(AnimError::Parse(_))));
    }

    fn ev(time: f32, name: &str) -> AnimEvent {
        AnimEvent {
            time,
            name: name.into(),
            payload: String::new(),
        }
    }

    fn names_at(events: &[TweenEvent]) -> Vec<(String, f32)> {
        events.iter().map(|e| (e.name.clone(), e.at)).collect()
    }

    #[test]
    fn events_are_placed_once_per_pass_with_reverse_ping_pong_and_speed() {
        let mut def = fade_slide();
        def.events = vec![ev(0.75, "late"), ev(0.0, "start"), ev(1.0, "end")];
        let once = def.compile_events(false, None).unwrap();
        assert_eq!(
            names_at(&once),
            vec![("start".into(), 0.0), ("late".into(), 0.75), ("end".into(), 1.0)]
        );
        let back = def.compile_events(true, None).unwrap();
        assert_eq!(
            names_at(&back),
            vec![("end".into(), 0.0), ("late".into(), 0.25), ("start".into(), 1.0)]
        );
        def.mode = LoopMode::PingPong;
        def.loops = 2;
        let pp = def.compile_events(false, Some(2.0)).unwrap();
        assert_eq!(
            names_at(&pp),
            vec![
                ("start".into(), 0.0),
                ("late".into(), 0.375),
                ("end".into(), 0.5),
                ("end".into(), 0.5),
                ("late".into(), 0.625),
                ("start".into(), 1.0),
            ],
            "the turn is crossed twice: once leaving pass 1, once entering pass 2"
        );
        assert!(fade_slide().compile_events(false, None).unwrap().is_empty());
    }

    /// Proof: `compile_events` places pass `n` at `n as f32 * duration`, which is not monotone
    /// in f32. A 0.1 s loop, 8 passes, events at 0 and at 0.1: the pass-6 end lands one f32
    /// above 0.7 and the pass-7 start lands on 0.7, in that order. `ui.play_anim` sample walks
    /// this list with `take_while`, so a sample at 0.7 drops the pass-7 start. Playback sorts
    /// in `UiTween::with_events` and still fires it.
    #[test]
    fn looped_tenth_second_events_stay_in_time_order() {
        let def = UiAnimDef {
            tracks: vec![AnimTrack {
                property: "opacity".into(),
                keys: vec![num(0.0, 1.0, None), num(0.1, 0.0, None)],
            }],
            mode: LoopMode::Loop,
            loops: 8,
            speed: 1.0,
            events: vec![ev(0.0, "start"), ev(0.1, "end")],
        };
        let events = def.compile_events(false, None).unwrap();
        let t = 0.7f32;
        let scanned = events.iter().take_while(|e| e.at <= t).count();
        let due = events.iter().filter(|e| e.at <= t).count();
        assert_eq!(
            scanned,
            due,
            "at {t} a take_while cursor (ui.play_anim sample) skips an event that is already due; times {:?}",
            events.iter().map(|e| (e.name.as_str(), e.at)).collect::<Vec<_>>()
        );
    }

    /// Proof: the same placement puts an event authored at the pass end past the tween plan.
    /// A 0.3 s loop, 7 passes, speed 1: the last `end` is the f32 just above 2.1 and
    /// `TweenPlan::duration` (the sum of the steps) is the f32 just below 2.1. Sample at the
    /// reported duration omits it; `advance_tweens` still sends it, because a finished tween
    /// flushes every event still past the cursor.
    #[test]
    fn an_event_authored_at_the_pass_end_is_inside_the_plan() {
        let def = UiAnimDef {
            tracks: vec![AnimTrack {
                property: "opacity".into(),
                keys: vec![num(0.0, 1.0, None), num(0.3, 0.0, None)],
            }],
            mode: LoopMode::Loop,
            loops: 7,
            speed: 1.0,
            events: vec![ev(0.3, "end")],
        };
        let events = def.compile_events(false, None).unwrap();
        let plan = def.compile(false, None).unwrap();
        let end_at = events.iter().map(|e| e.at).reduce(f32::max).unwrap();
        assert!(
            end_at <= plan.duration(),
            "end event at {end_at} is past the plan duration {}, so sample at the reported duration misses an event the tween fires when it ends",
            plan.duration()
        );
    }

    /// Sweep of pass lengths that are not exact in f32, every loop count up to `MAX_LOOPS`, both
    /// loop modes, reverse and several speeds, with two tracks whose step sums round differently
    /// (ADR-10132): the times never decrease, every event appears once per pass, the last is at
    /// most the plan's duration, an event at the pass end shares its time with the next pass's
    /// start event (the boundary), and the last pass ends exactly at the plan's duration.
    #[test]
    fn looped_events_stay_ordered_and_inside_the_plan_for_any_loop_count() {
        for len in [0.1f32, 0.3, 0.7, 1.0 / 3.0, 0.45, 2.2] {
            for mode in [LoopMode::Loop, LoopMode::PingPong] {
                for speed in [0.05f32, 0.3, 1.0, 1.7, 3.0, 20.0] {
                    for reverse in [false, true] {
                        for loops in 1..=MAX_LOOPS {
                            let def = UiAnimDef {
                                tracks: vec![
                                    AnimTrack {
                                        property: "opacity".into(),
                                        keys: vec![num(0.0, 1.0, None), num(len, 0.0, None)],
                                    },
                                    AnimTrack {
                                        property: "scale".into(),
                                        keys: vec![num(len / 3.0, 1.0, None), num(len * 0.9, 2.0, None)],
                                    },
                                ],
                                mode,
                                loops,
                                speed,
                                events: vec![ev(len, "end"), ev(0.0, "start"), ev(len / 3.0, "third")],
                            };
                            let label = format!("{len} {mode:?} x{loops} speed {speed} reverse {reverse}");
                            let events = def.compile_events(reverse, None).unwrap();
                            let duration = def.compile(reverse, None).unwrap().duration();
                            assert_eq!(events.len(), 3 * loops as usize, "{label}");
                            assert!(
                                events.windows(2).all(|w| w[0].at <= w[1].at),
                                "{label}: {:?}",
                                names_at(&events)
                            );
                            let last = events.last().unwrap().at;
                            assert!(last <= duration, "{label}: {last} > {duration}");
                            // Each pass is [start, third, end] or reversed; its edge events sit on
                            // the pass boundaries.
                            let edges: Vec<f32> = events
                                .chunks(3)
                                .flat_map(|pass| [pass[0].at, pass[2].at])
                                .collect();
                            for pair in edges[1..edges.len() - 1].chunks(2) {
                                assert_eq!(pair[0], pair[1], "{label}: a pass boundary is one time");
                            }
                            assert_eq!(edges[0], 0.0, "{label}");
                            assert_eq!(last, duration, "{label}: the last pass ends with the plan");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bad_events_are_refused_with_their_index() {
        for bad in [
            ev(-0.1, "a"),
            ev(1.5, "a"),
            ev(f32::NAN, "a"),
            ev(0.5, ""),
            ev(0.5, "tab\there"),
            ev(0.5, &"x".repeat(MAX_EVENT_TEXT + 1)),
        ] {
            let mut d = fade_slide();
            d.events = vec![ev(0.1, "ok"), bad.clone()];
            assert_eq!(d.validate(), Err(AnimError::Event(1)), "{bad:?}");
        }
        let mut d = fade_slide();
        d.events = vec![ev(0.5, "a"); MAX_EVENTS + 1];
        assert_eq!(d.validate(), Err(AnimError::Event(MAX_EVENTS)));
    }

    /// The v1 corpus bytes (`format-corpus/uianim/v1.ron`): a v1 file reads as v2 with no events,
    /// and writes back as v2 (the migration is the number only; ADR-8900).
    #[test]
    fn a_v1_file_reads_as_v2_and_the_migration_is_idempotent() {
        let v1 = "(format_version: 1, animations: { \"pop\": (tracks: [(property: \"opacity\", keys: [(time: 0.0, value: 0.0), (time: 0.2, value: 1.0)])]) })";
        let lib = UiAnimLibrary::from_ron(v1).unwrap();
        assert_eq!(lib.format_version, UIANIM_FORMAT_VERSION);
        assert_eq!(UIANIM_FORMAT_VERSION, 2);
        assert!(lib.animations["pop"].events.is_empty());
        let once = lib.to_ron_string().unwrap();
        assert!(!once.contains("events"), "an empty list is not written: {once}");
        let twice = UiAnimLibrary::from_ron(&once).unwrap().to_ron_string().unwrap();
        assert_eq!(once, twice);
        assert_eq!(uianim_migration_sources(), vec![1]);
    }

    /// A file without `format_version` is v1 (it used to fail the full parse with a missing-field
    /// error although the version probe read it as v1).
    #[test]
    fn a_file_without_a_version_reads_as_v1_and_becomes_v2() {
        let bare =
            "(animations: { \"a\": (tracks: [(property: \"opacity\", keys: [(time: 0.0, value: 1.0)])]) })";
        let lib = UiAnimLibrary::from_ron(bare).unwrap();
        assert_eq!(lib.format_version, UIANIM_FORMAT_VERSION);
        assert_eq!(lib.animations.len(), 1);
    }

    #[test]
    fn a_v2_file_with_events_round_trips() {
        let mut def = fade_slide();
        def.events = vec![AnimEvent {
            time: 0.5,
            name: "sfx".into(),
            payload: "whoosh".into(),
        }];
        let mut lib = UiAnimLibrary::default();
        lib.animations.insert("pop".into(), def);
        let text = lib.to_ron_string().unwrap();
        assert!(text.contains("events"), "{text}");
        assert_eq!(UiAnimLibrary::from_ron(&text).unwrap(), lib);
    }
}
