// SPDX-License-Identifier: MPL-2.0
//! Caption messages (`docs/specs/56-dialogue-and-subtitles.md` § 3.1, ADR-1697).
//!
//! `regia_audio` and `regia_ui` are siblings in the layer order and must not name each other, so
//! everything that wants text on screen for some time (a dialogue line, a sound cue, a video
//! subtitle track, game code) sends these plain-data messages and the caption board in
//! `regia_ui::captions` shows them. There is no logic here besides the id counter.
//!
//! All times are integer microseconds (`u64`) on the caption clock: fixed-step simulation time by
//! default, or the audio device clock for lines driven by an audio start (never wall time).

use regia_ecs::prelude::*;

/// A speaker id (`guard`, `narrator`); plain string as in `regia_ui::localization`.
pub type SpeakerId = String;
/// A locale tag (`en`, `pt-BR`).
pub type Locale = String;
/// A Fluent message key.
pub type LocKey = String;

/// The id of one caption, from [`CaptionIds::next_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaptionId(pub u64);

/// Allocates [`CaptionId`]s from one deterministic counter (ids start at 1).
#[derive(Resource, Debug, Default, Clone)]
pub struct CaptionIds {
    next: u64,
}

impl CaptionIds {
    /// The next id: the order of calls is the order of ids.
    pub fn next_id(&mut self) -> CaptionId {
        self.next += 1;
        CaptionId(self.next)
    }
}

/// What a caption is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum CaptionKind {
    /// A spoken line.
    #[default]
    Speech,
    /// Narration (a voice with no on-screen speaker).
    Narration,
    /// A non-speech sound caption (`[door slams]`).
    Sound,
}

impl CaptionKind {
    /// The lowercase name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Speech => "speech",
            Self::Narration => "narration",
            Self::Sound => "sound",
        }
    }
}

/// Who sent a caption.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CaptionSource {
    /// A conversation line (run id, line id).
    Dialogue {
        /// The conversation run.
        run: u64,
        /// The line id, `<conversation>#<line>`.
        line: String,
    },
    /// A sound cue asset path.
    SoundCue(String),
    /// A video subtitle track.
    Video,
    /// Game code, with a free tag.
    Game(String),
}

impl CaptionSource {
    /// A short text form for the agent protocol.
    pub fn describe(&self) -> String {
        match self {
            Self::Dialogue { run, line } => format!("dialogue:{run}:{line}"),
            Self::SoundCue(path) => format!("sound_cue:{path}"),
            Self::Video => "video".to_string(),
            Self::Game(tag) => format!("game:{tag}"),
        }
    }
}

/// The text of one piece: literal, or a key the caption board resolves when it accepts the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptionText {
    /// Text to show as is.
    Literal(String),
    /// A Fluent key with arguments.
    Key {
        /// The message key.
        key: LocKey,
        /// Fluent arguments.
        args: Vec<(String, String)>,
    },
}

/// One timed piece of a caption: shown from `offset_us` after the caption starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionPiece {
    /// Offset from the caption start, in microseconds (ascending across pieces).
    pub offset_us: u64,
    /// What to show.
    pub text: CaptionText,
}

impl CaptionPiece {
    /// A literal piece.
    pub fn literal(offset_us: u64, text: impl Into<String>) -> Self {
        Self {
            offset_us,
            text: CaptionText::Literal(text.into()),
        }
    }
}

/// When a caption starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionTime {
    /// At the board's current clock.
    #[default]
    Now,
    /// At a given clock value in microseconds.
    AtMicros(u64),
}

/// Style hints a sender may attach.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptionStyleHint {
    /// Overrides the speaker for label color lookup.
    pub speaker: Option<SpeakerId>,
    /// Ask for emphasis (the presenter may ignore it).
    pub emphasis: bool,
    /// Show the text one line at a time: every page holds one line, shown in sequence over the
    /// piece's time instead of stacked (spec 36 § 8.1 `one_at_a_time`). Off by default.
    pub one_at_a_time: bool,
}

/// "Show this text from `start` for `planned_us`."
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct CaptionStart {
    /// The caption id (allocate with [`CaptionIds::next_id`]).
    pub id: CaptionId,
    /// Who sent it.
    pub source: CaptionSource,
    /// Speech, narration or sound.
    pub kind: CaptionKind,
    /// The locale of the text (decides the paragraph direction and the reading speed).
    pub locale: Locale,
    /// The timed pieces; at least one.
    pub pieces: Vec<CaptionPiece>,
    /// The speaker, if any.
    pub speaker: Option<SpeakerId>,
    /// Higher wins when more captions are active than fit.
    pub priority: u8,
    /// When it starts.
    pub start: CaptionTime,
    /// The planned length in microseconds.
    pub planned_us: u64,
    /// Use the text's own line breaks verbatim.
    pub keep_breaks: bool,
    /// Style hints.
    pub hint: CaptionStyleHint,
}

/// The audio stopped or the sender cancelled: end the caption.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionStop {
    /// The caption to end.
    pub id: CaptionId,
}

/// A named timed event (a sequencer marker, an animation event) the project may map to a caption.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct CaptionCueEvent {
    /// The event name (`door_slam`).
    pub name: String,
    /// When it happened, in microseconds on the caption clock.
    pub at_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_count_up_from_one() {
        let mut ids = CaptionIds::default();
        assert_eq!(ids.next_id(), CaptionId(1));
        assert_eq!(ids.next_id(), CaptionId(2));
    }

    #[test]
    fn kind_names_are_lowercase() {
        assert_eq!(CaptionKind::Sound.name(), "sound");
        assert_eq!(CaptionKind::Narration.name(), "narration");
    }

    #[test]
    fn source_description_is_stable() {
        let source = CaptionSource::Dialogue {
            run: 3,
            line: "intro#greet".into(),
        };
        assert_eq!(source.describe(), "dialogue:3:intro#greet");
    }
}
