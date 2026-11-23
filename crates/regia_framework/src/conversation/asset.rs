// SPDX-License-Identifier: MPL-2.0
//! The conversation asset, speakers and project settings
//! (`docs/specs/56-dialogue-and-subtitles.md` §§ 2, 4).
//!
//! Formats: `conversation` (`<id>.conv.ron`), `speakers` (`speakers.ron`),
//! `conversation_settings` (`conversation.ron`), all `format_version: 1`. Unknown fields are an
//! error; every parse error carries the path, line and column.

use std::cell::Cell;
use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_ui::captions::{CaptionPolicy, OverlapPolicy};
use serde::{Deserialize, Serialize};

use super::ConversationError;

/// The current version of the `conversation`, `speakers` and `conversation_settings` formats.
pub const CONVERSATION_FORMAT_VERSION: u32 = 1;

/// Where each kind of file lives under a project's `assets/`.
pub const CONVERSATIONS_DIR: &str = "conversations";
/// The speaker set file name inside [`CONVERSATIONS_DIR`].
pub const SPEAKERS_FILE: &str = "speakers.ron";
/// The project settings file name inside [`CONVERSATIONS_DIR`].
pub const SETTINGS_FILE: &str = "conversation.ron";
/// The cue event map file name inside [`CONVERSATIONS_DIR`].
pub const CUE_EVENTS_FILE: &str = "cue_events.ron";

/// The serde field-list fingerprint of the `conversation` format (hashed by `cargo xtask
/// check-formats`): the root, line and choice-option structs and the enums' variant spellings.
pub fn conversation_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ConversationAsset>(),
        regia_core::schema::serde_field_names::<ConversationLine>(),
        regia_core::schema::serde_field_names::<ChoiceOption>(),
        regia_core::schema::serde_variant_names::<Step>(),
        regia_core::schema::serde_variant_names::<VoiceSpec>(),
        regia_core::schema::serde_variant_names::<DurationSource>(),
    ])
}

/// The fingerprint of the `speakers` format.
pub fn speakers_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<SpeakerSet>(),
        regia_core::schema::serde_field_names::<Speaker>(),
        regia_core::schema::serde_variant_names::<LabelSide>(),
    ])
}

/// The fingerprint of the `conversation_settings` format.
pub fn settings_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ConversationSettings>(),
        regia_core::schema::serde_variant_names::<VoiceFallback>(),
        regia_core::schema::serde_variant_names::<ClipPolicy>(),
        regia_core::schema::serde_variant_names::<OverlapPolicy>(),
    ])
}

/// The fingerprint of the `cue_events` format.
pub fn cue_events_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<CueEventFile>(),
        regia_core::schema::serde_field_names::<regia_ui::captions::CueEventDef>(),
        regia_core::schema::serde_variant_names::<regia_ui::captions::CaptionKindDef>(),
    ])
}

/// The versions a built-in migration step leaves, for all four conversation formats: none yet
/// (every format is at v1).
pub fn conversation_migration_sources() -> Vec<u32> {
    Vec::new()
}

fn version_one() -> u32 {
    CONVERSATION_FORMAT_VERSION
}

/// Where a voice clip for a line comes from.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VoiceSpec {
    /// Resolve through the project `voice_path` template.
    #[default]
    Auto,
    /// The line has no voice and never reports a missing clip.
    None,
    /// An explicit clip path per locale, then the template for the other locales.
    Explicit(BTreeMap<String, String>),
}

/// How a line's planned length is chosen (evaluated when the line starts).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DurationSource {
    /// The resolved clip's length; an `Estimate` if no clip resolved.
    #[default]
    Clip,
    /// An authored length in milliseconds.
    Fixed(u32),
    /// `ceil(graphemes / estimate_cps)` seconds, floored at the minimum display time.
    Estimate,
}

/// A timed sub-cue: `(at_ms, text key)`.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubCue(pub u32, pub String);

/// One spoken line.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLine {
    /// The line id, unique inside the conversation.
    pub id: String,
    /// The speaker id (see `speakers.ron`).
    pub speaker: String,
    /// The Fluent key of the text.
    pub text: String,
    /// Context for translators and the cast.
    #[serde(default)]
    pub note: String,
    /// A free emotion tag.
    #[serde(default)]
    pub emotion: Option<String>,
    /// Free tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Where the clip comes from.
    #[serde(default)]
    pub voice: VoiceSpec,
    /// How long the line lasts.
    #[serde(default)]
    pub duration: DurationSource,
    /// Timed sub-cues; ascending by `at_ms`.
    #[serde(default)]
    pub cues: Vec<SubCue>,
    /// Overrides the project default priority.
    #[serde(default)]
    pub priority: Option<u8>,
    /// Use the translator's line breaks verbatim.
    #[serde(default)]
    pub keep_breaks: bool,
}

/// One option of a `Choice`.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOption {
    /// The Fluent key of the option text.
    pub text: String,
    /// The condition name; absent means always available.
    #[serde(default)]
    pub cond: Option<String>,
    /// The label to continue at.
    pub goto: String,
}

/// One step of a conversation (spec § 2.5).
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// Play that line, then continue.
    Line(String),
    /// A jump target.
    Label(String),
    /// Continue at the label.
    Goto(String),
    /// Jump to `then` or `else_` depending on the condition.
    If {
        /// The condition name.
        cond: String,
        /// The label taken when the condition is true.
        then: String,
        /// The label taken when it is false or unknown.
        #[serde(rename = "else")]
        else_: String,
    },
    /// Offer options and wait for a choice.
    Choice {
        /// The Fluent key of an optional prompt.
        #[serde(default)]
        prompt: Option<String>,
        /// The options in file order.
        options: Vec<ChoiceOption>,
    },
    /// Emit a `ConversationEvent` for game code.
    Event(String),
    /// Finish the run.
    End,
}

/// A conversation asset.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationAsset {
    /// Format version.
    #[serde(default = "version_one")]
    pub format_version: u32,
    /// The conversation id.
    pub id: String,
    /// Free notes.
    #[serde(default)]
    pub notes: String,
    /// The lines.
    pub lines: Vec<ConversationLine>,
    /// The steps; absent means "every line in file order".
    #[serde(default)]
    pub steps: Vec<Step>,
}

thread_local! {
    static EFFECTIVE_STEPS_CALLS: Cell<u64> = const { Cell::new(0) };
}

/// How many times `effective_steps` has built a step list on this thread.
#[cfg(test)]
pub(crate) fn effective_steps_calls() -> u64 {
    EFFECTIVE_STEPS_CALLS.with(|count| count.get())
}

impl ConversationAsset {
    /// The steps the runner executes: `steps`, or one `Line` per line in file order.
    pub fn effective_steps(&self) -> Vec<Step> {
        EFFECTIVE_STEPS_CALLS.with(|count| count.set(count.get().saturating_add(1)));
        if self.steps.is_empty() {
            self.lines.iter().map(|l| Step::Line(l.id.clone())).collect()
        } else {
            self.steps.clone()
        }
    }

    /// The line with that id.
    pub fn line(&self, id: &str) -> Option<&ConversationLine> {
        self.lines.iter().find(|l| l.id == id)
    }

    /// The index of a line in file order (the zero-based ordinal used by `{index}`).
    pub fn line_index(&self, id: &str) -> Option<usize> {
        self.lines.iter().position(|l| l.id == id)
    }

    /// The step index each label names (first occurrence).
    pub fn labels(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for (i, step) in self.effective_steps().iter().enumerate() {
            if let Step::Label(name) = step {
                out.entry(name.clone()).or_insert(i);
            }
        }
        out
    }
}

/// Which side of the text the speaker label sits on.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LabelSide {
    /// The reading-start side (left for left-to-right scripts).
    #[default]
    Start,
    /// The reading-end side.
    End,
}

/// A speaker: a named voice identity.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Speaker {
    /// The Fluent key of the display name; absent shows the id.
    #[serde(default)]
    pub name_key: Option<String>,
    /// `#RRGGBB` for the label.
    #[serde(default)]
    pub color: Option<String>,
    /// Whether the speaker's lines are voiced by default.
    #[serde(default = "yes")]
    pub voiced: bool,
    /// Narration: the caption kind is `Narration`.
    #[serde(default)]
    pub narration: bool,
    /// The side of the text the label sits on.
    #[serde(default)]
    pub side: LabelSide,
}

fn yes() -> bool {
    true
}

impl Default for Speaker {
    fn default() -> Self {
        Self {
            name_key: None,
            color: None,
            voiced: true,
            narration: false,
            side: LabelSide::Start,
        }
    }
}

/// `speakers.ron`.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakerSet {
    /// Format version.
    #[serde(default = "version_one")]
    pub format_version: u32,
    /// The speakers by id.
    #[serde(default)]
    pub speakers: BTreeMap<String, Speaker>,
}

/// What to do when the clip of the requested locale is missing but another locale has one.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VoiceFallback {
    /// Play the fallback clip with the subtitle in the requested locale.
    #[default]
    Source,
    /// Play nothing; show the subtitle for its estimated length.
    Silent,
}

/// Whether a missing clip in a staged locale is an error.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ClipPolicy {
    /// A warning.
    #[default]
    Warn,
    /// An error (`conv_missing_clip`).
    Require,
}

fn default_voice_path() -> String {
    "audio/vo/{locale}/{conversation}_{line}.ogg".to_string()
}

fn default_estimate_cps() -> u32 {
    14
}

fn default_max_steps() -> u32 {
    64
}

fn default_priority() -> u8 {
    1
}

/// The caption-board fields of [`ConversationSettings`] (`overlap`, `max_visible`, `min_display_ms`,
/// `max_extend_ms`, `reading_cps`, `reading_cps_cjk`), compared to detect a change.
pub type CaptionFields = (OverlapPolicy, usize, u32, u32, u32, u32);

/// `assets/conversations/conversation.ron` (format `conversation_settings`).
#[derive(Reflect, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationSettings {
    /// Format version.
    #[serde(default = "version_one")]
    pub format_version: u32,
    /// The clip path template; `{locale}`, `{conversation}`, `{line}`, `{speaker}`, `{index}`.
    #[serde(default = "default_voice_path")]
    pub voice_path: String,
    /// What to do about a missing own-locale clip.
    #[serde(default)]
    pub voice_fallback: VoiceFallback,
    /// Whether a missing clip in a staged locale fails the cook.
    #[serde(default)]
    pub clip_policy: ClipPolicy,
    /// Graphemes per second for `Estimate` durations.
    #[serde(default = "default_estimate_cps")]
    pub estimate_cps: u32,
    /// What the caption board does on overlap.
    #[serde(default)]
    pub overlap: OverlapPolicy,
    /// Rows shown at once.
    #[serde(default = "three")]
    pub max_visible: usize,
    /// Minimum display time in milliseconds.
    #[serde(default = "thousand")]
    pub min_display_ms: u32,
    /// The most a row is extended for reading speed, in milliseconds.
    #[serde(default = "two_thousand")]
    pub max_extend_ms: u32,
    /// Reading speed for alphabetic scripts.
    #[serde(default = "seventeen")]
    pub reading_cps: u32,
    /// Reading speed for Han, Kana and Hangul.
    #[serde(default = "nine")]
    pub reading_cps_cjk: u32,
    /// Priority of lines that set none.
    #[serde(default = "default_priority")]
    pub default_priority: u8,
    /// Non-blocking steps one run may take per frame.
    #[serde(default = "default_max_steps")]
    pub max_steps_per_frame: u32,
    /// Scale sub-cue offsets by the clip length ratio.
    #[serde(default)]
    pub rescale_cues: bool,
}

fn three() -> usize {
    3
}
fn thousand() -> u32 {
    1000
}
fn two_thousand() -> u32 {
    2000
}
fn seventeen() -> u32 {
    17
}
fn nine() -> u32 {
    9
}

impl Default for ConversationSettings {
    fn default() -> Self {
        Self {
            format_version: CONVERSATION_FORMAT_VERSION,
            voice_path: default_voice_path(),
            voice_fallback: VoiceFallback::Source,
            clip_policy: ClipPolicy::Warn,
            estimate_cps: default_estimate_cps(),
            overlap: OverlapPolicy::Stack,
            max_visible: 3,
            min_display_ms: 1000,
            max_extend_ms: 2000,
            reading_cps: 17,
            reading_cps_cjk: 9,
            default_priority: default_priority(),
            max_steps_per_frame: default_max_steps(),
            rescale_cues: false,
        }
    }
}

impl ConversationSettings {
    /// The board policy these settings describe.
    pub fn caption_policy(&self) -> CaptionPolicy {
        let mut policy = CaptionPolicy::default();
        self.apply_caption_policy(&mut policy);
        policy
    }

    /// Writes the board fields these settings own (`overlap`, `max_visible`, `min_display_ms`,
    /// `max_extend_ms`, `reading_cps`, `reading_cps_cjk`) into `policy`, leaving the others
    /// (`max_lines`, `bottom_offset`, ...) as the game set them (spec 56 § 4.1).
    pub fn apply_caption_policy(&self, policy: &mut CaptionPolicy) {
        policy.overlap = self.overlap;
        policy.max_visible = self.max_visible;
        policy.min_display_us = u64::from(self.min_display_ms) * 1000;
        policy.max_extend_us = u64::from(self.max_extend_ms) * 1000;
        policy.cps_alphabetic = self.reading_cps;
        policy.cps_cjk = self.reading_cps_cjk;
    }

    /// The caption-board fields of these settings, for change detection.
    pub fn caption_fields(&self) -> CaptionFields {
        (
            self.overlap,
            self.max_visible,
            self.min_display_ms,
            self.max_extend_ms,
            self.reading_cps,
            self.reading_cps_cjk,
        )
    }
}

/// `cue_events.ron`: the caption for each named cue event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CueEventFile {
    /// Format version.
    #[serde(default = "version_one")]
    pub format_version: u32,
    /// The events by name.
    #[serde(default)]
    pub events: BTreeMap<String, regia_ui::captions::CueEventDef>,
}

fn locate(
    path: &str,
    source: &str,
    offset_line: usize,
    offset_col: usize,
    message: impl std::fmt::Display,
) -> String {
    let _ = source;
    format!("{path}:{offset_line}:{offset_col}: {message}")
}

fn parse_ron<T: for<'de> Deserialize<'de>>(path: &str, source: &str) -> Result<T, ConversationError> {
    ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .from_str::<T>(source)
        .map_err(|e| {
            ConversationError::Asset(locate(path, source, e.span.start.line, e.span.start.col, &e.code))
        })
}

fn check_version(path: &str, source: &str, version: u32) -> Result<(), ConversationError> {
    if version == 0 || version > CONVERSATION_FORMAT_VERSION {
        let line = source
            .lines()
            .position(|l| l.contains("format_version"))
            .map_or(1, |i| i + 1);
        return Err(ConversationError::Asset(locate(
            path,
            source,
            line,
            1,
            format!(
                "unsupported format_version {version} (this build reads up to {CONVERSATION_FORMAT_VERSION})"
            ),
        )));
    }
    Ok(())
}

/// Parses a `.conv.ron` file. Errors name `path:line:col`. Duplicate line ids are an error at the
/// second occurrence.
pub fn parse_conversation(path: &str, source: &str) -> Result<ConversationAsset, ConversationError> {
    let asset: ConversationAsset = parse_ron(path, source)?;
    check_version(path, source, asset.format_version)?;
    let mut seen = std::collections::BTreeSet::new();
    for line in &asset.lines {
        if !seen.insert(line.id.as_str()) {
            let needle = format!("id: \"{}\"", line.id);
            let mut count = 0;
            let mut found = 1;
            for (i, text) in source.lines().enumerate() {
                if text.contains(&needle) {
                    count += 1;
                    if count == 2 {
                        found = i + 1;
                        break;
                    }
                }
            }
            return Err(ConversationError::Asset(locate(
                path,
                source,
                found,
                1,
                format!("duplicate line id `{}`", line.id),
            )));
        }
    }
    Ok(asset)
}

/// Parses `speakers.ron`.
pub fn parse_speakers(path: &str, source: &str) -> Result<SpeakerSet, ConversationError> {
    let set: SpeakerSet = parse_ron(path, source)?;
    check_version(path, source, set.format_version)?;
    Ok(set)
}

/// Parses `conversation.ron`.
pub fn parse_settings(path: &str, source: &str) -> Result<ConversationSettings, ConversationError> {
    let settings: ConversationSettings = parse_ron(path, source)?;
    check_version(path, source, settings.format_version)?;
    Ok(settings)
}

/// Parses `cue_events.ron`.
pub fn parse_cue_events(path: &str, source: &str) -> Result<CueEventFile, ConversationError> {
    let file: CueEventFile = parse_ron(path, source)?;
    check_version(path, source, file.format_version)?;
    Ok(file)
}

/// Writes an asset as RON (the editor and the tests).
pub fn to_ron(asset: &ConversationAsset) -> Result<String, ConversationError> {
    ron::ser::to_string_pretty(asset, regia_core::ron_pretty())
        .map_err(|e| ConversationError::Asset(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = r#"(
    format_version: 1,
    id: "intro",
    notes: "Gate scene. Guard is tired, not hostile.",
    lines: [
        (id: "greet", speaker: "guard", text: "conv-intro-greet", emotion: Some("weary"),
         note: "Said while waving the player in.",
         voice: Auto, duration: Clip, tags: ["gate"]),
        (id: "warn", speaker: "guard", text: "conv-intro-warn", voice: Explicit({"en": "audio/vo/en/gate_warn.ogg"}),
         duration: Fixed(2600), cues: [(0, "conv-intro-warn-a"), (1300, "conv-intro-warn-b")]),
    ],
    steps: [ Line("greet"), If(cond: "met_captain", then: "short", else: "long"),
             Label("long"), Line("warn"), End, Label("short"), End ],
)"#;

    #[test]
    fn golden_parses_and_round_trips() {
        let asset = parse_conversation("intro.conv.ron", GOLDEN).expect("a value");
        assert_eq!(asset.lines.len(), 2);
        assert_eq!(asset.lines[1].cues[1], SubCue(1300, "conv-intro-warn-b".into()));
        assert_eq!(asset.lines[1].duration, DurationSource::Fixed(2600));
        let text = to_ron(&asset).expect("a value");
        let again = parse_conversation("intro.conv.ron", &text).expect("a value");
        assert_eq!(asset, again);
    }

    #[test]
    fn unknown_field_names_path_and_line() {
        let source = GOLDEN.replace("notes:", "bogus_field:");
        let error = parse_conversation("intro.conv.ron", &source)
            .expect_err("an error")
            .to_string();
        assert!(error.contains("intro.conv.ron:"), "{error}");
        assert!(
            error.contains("bogus_field") || error.contains("unknown field"),
            "{error}"
        );
    }

    #[test]
    fn bad_version_names_the_version_line() {
        let source = GOLDEN.replace("format_version: 1", "format_version: 9");
        let error = parse_conversation("intro.conv.ron", &source)
            .expect_err("an error")
            .to_string();
        assert!(error.contains("intro.conv.ron:2:"), "{error}");
        assert!(error.contains("format_version 9"), "{error}");
    }

    #[test]
    fn duplicate_line_id_names_the_second_line() {
        let source = GOLDEN.replace("id: \"warn\"", "id: \"greet\"");
        let error = parse_conversation("intro.conv.ron", &source)
            .expect_err("an error")
            .to_string();
        assert!(error.contains("duplicate line id `greet`"), "{error}");
        assert!(error.contains("intro.conv.ron:9:"), "{error}");
    }

    #[test]
    fn default_steps_are_every_line_in_order() {
        let source =
            r#"(id: "c", lines: [(id: "a", speaker: "s", text: "k1"), (id: "b", speaker: "s", text: "k2")])"#;
        let asset = parse_conversation("c.conv.ron", source).expect("a value");
        assert_eq!(
            asset.effective_steps(),
            vec![Step::Line("a".into()), Step::Line("b".into())]
        );
        assert_eq!(asset.line_index("b"), Some(1));
    }

    #[test]
    fn settings_defaults_and_unknown_field() {
        let settings = parse_settings("conversation.ron", "(format_version: 1)").expect("a value");
        assert_eq!(settings, ConversationSettings::default());
        assert!(parse_settings("conversation.ron", "(nonsense: 1)").is_err());
        assert_eq!(settings.caption_policy().min_display_us, 1_000_000);
    }

    #[test]
    fn settings_reject_key_prefix() {
        let error = parse_settings("conversation.ron", "(format_version: 1, key_prefix: \"conv\")")
            .expect_err("key_prefix is not a field");
        let text = error.to_string();
        assert!(
            text.contains("key_prefix") || text.contains("unknown field"),
            "{text}"
        );
        assert!(!settings_schema_fingerprint().contains("key_prefix"));
    }

    #[test]
    fn fingerprints_name_the_fields() {
        assert!(conversation_schema_fingerprint().contains("keep_breaks"));
        assert!(conversation_schema_fingerprint().contains("Choice"));
        assert!(speakers_schema_fingerprint().contains("name_key"));
        assert!(settings_schema_fingerprint().contains("max_steps_per_frame"));
        assert!(cue_events_schema_fingerprint().contains("planned_ms"));
        assert!(conversation_migration_sources().is_empty());
    }

    #[test]
    fn speakers_parse() {
        let source = r##"(speakers: {"guard": (name_key: "speaker-guard", color: "#F2C14E", side: Start)})"##;
        let set = parse_speakers("speakers.ron", source).expect("a value");
        assert!(set.speakers["guard"].voiced);
    }
}
