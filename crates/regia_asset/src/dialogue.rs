// SPDX-License-Identifier: MPL-2.0
//! Voices, dialogue lines and subtitle cue sets (spec 36 §§ 7-8, ADR-1882): the `.voice.ron`
//! (format `voice` v1), `.dialogue.ron` (format `dialogue` v1) and `<sound>.subs.ron` (format
//! `subtitles` v1) assets, their parsers with `path:line:col` errors, the variant selection rule and the
//! cue checks. Pure: no file IO, no clock. The line player is `regia_framework::dialogue`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The `format_version` of `.voice.ron`.
pub const VOICE_VERSION: u32 = 1;
/// The `format_version` of `.dialogue.ron`.
pub const DIALOGUE_VERSION: u32 = 1;
/// The `format_version` of `.subs.ron`.
pub const SUBTITLES_VERSION: u32 = 1;

/// The schema fingerprint of the `voice` format (`cargo xtask check-formats`).
#[must_use]
pub fn voice_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<VoiceAsset>(),
        regia_core::schema::serde_variant_names::<Gender>(),
        regia_core::schema::serde_variant_names::<Number>(),
    ])
}

/// The schema fingerprint of the `dialogue` format.
#[must_use]
pub fn dialogue_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<DialogueAsset>(),
        regia_core::schema::serde_field_names::<DialogueLine>(),
        regia_core::schema::serde_field_names::<Variant>(),
        regia_core::schema::serde_field_names::<Cue>(),
    ])
}

/// The schema fingerprint of the `subtitles` format.
#[must_use]
pub fn subtitles_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<SubtitleTrack>(),
        regia_core::schema::serde_field_names::<Cue>(),
    ])
}

/// Versions the three formats migrate from: none (all are v1).
#[must_use]
pub fn dialogue_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Grammatical gender hint of a voice, passed to Fluent as `$speaker_gender` / `$listener_gender`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Gender {
    /// `neutral`.
    #[default]
    Neutral,
    /// `feminine`.
    Feminine,
    /// `masculine`.
    Masculine,
    /// `common`.
    Common,
    /// `other`.
    Other,
}

impl Gender {
    /// The Fluent argument value: `neutral`, `feminine`, `masculine`, `common` or `other`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Feminine => "feminine",
            Self::Masculine => "masculine",
            Self::Common => "common",
            Self::Other => "other",
        }
    }
}

/// Grammatical number hint of a voice (`$speaker_number` / `$listener_number`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Number {
    /// `singular`.
    #[default]
    Singular,
    /// `plural`.
    Plural,
}

impl Number {
    /// The Fluent argument value: `singular` or `plural`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Singular => "singular",
            Self::Plural => "plural",
        }
    }
}

/// `<id>.voice.ron`: a speaker or listener identity with grammatical hints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAsset {
    /// Must be [`VOICE_VERSION`].
    pub format_version: u32,
    /// Lowercase `[a-z0-9_]`, 1 to 48 characters, unique in the project.
    pub id: String,
    /// The Fluent key of the display name (`$speaker`, `$listener`).
    pub name_key: String,
    /// Grammatical gender.
    #[serde(default)]
    pub gender: Gender,
    /// Grammatical number.
    #[serde(default)]
    pub number: Number,
}

/// One timed subtitle entry, in seconds from the start of its sound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cue {
    /// Start, seconds from the sound start (`>= 0`, non-decreasing across cues).
    pub at: f32,
    /// End; `None` runs to the next cue's start, or the sound's end for the last cue.
    #[serde(default)]
    pub until: Option<f32>,
    /// A Fluent key (exactly one of `text_key` and `text`).
    #[serde(default)]
    pub text_key: Option<String>,
    /// Raw text, not localizable.
    #[serde(default)]
    pub text: Option<String>,
}

/// `<sound>.subs.ron`: the cue set of one sound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubtitleTrack {
    /// Must be [`SUBTITLES_VERSION`].
    pub format_version: u32,
    /// `>= 0`; 0 means the set is never shown; larger wins.
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// Keep author line breaks (`\n`) and add no automatic ones.
    #[serde(default)]
    pub keep_breaks: bool,
    /// Present the entries of a long cue in sequence instead of stacked.
    #[serde(default)]
    pub one_at_a_time: bool,
    /// The cues, in time order.
    pub cues: Vec<Cue>,
}

fn default_priority() -> i32 {
    1
}

/// One context-specific recording of a line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    /// The speaker this recording is for; `None` fits any speaker.
    #[serde(default)]
    pub speaker: Option<String>,
    /// The listener this recording is for; `None` fits any listener.
    #[serde(default)]
    pub listener: Option<String>,
    /// The base sound path (resolved through the audio locale chain, spec 36 § 6).
    pub sound: String,
    /// Inline cues; they win over the sound's `.subs.ron` sidecar.
    #[serde(default)]
    pub cues: Option<Vec<Cue>>,
}

/// One spoken line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogueLine {
    /// Unique in the dialogue; lowercase `[a-z0-9_]`.
    pub id: String,
    /// The default speaker voice id.
    pub speaker: String,
    /// The default listener voice ids.
    #[serde(default)]
    pub listeners: Vec<String>,
    /// A translator and cast note (carried into the exchange file and the recording sheet).
    #[serde(default)]
    pub note: String,
    /// The Fluent key of the spoken text; default `dlg-<dialogue>-<line>`.
    #[serde(default)]
    pub text_key: Option<String>,
    /// When set, the subtitle shows this key's text instead of the transcript.
    #[serde(default)]
    pub display_key: Option<String>,
    /// A content marker a game may use to hide lines; Regia attaches no policy.
    #[serde(default)]
    pub mature: bool,
    /// Context-specific recordings.
    #[serde(default)]
    pub variants: Vec<Variant>,
}

/// `<name>.dialogue.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogueAsset {
    /// Must be [`DIALOGUE_VERSION`].
    pub format_version: u32,
    /// The dialogue name (lowercase `[a-z0-9_]`).
    pub name: String,
    /// The lines.
    pub lines: Vec<DialogueLine>,
}

impl DialogueLine {
    /// The transcript key: `text_key`, else `dlg-<dialogue>-<line>`.
    #[must_use]
    pub fn text_key(&self, dialogue: &str) -> String {
        self.text_key
            .clone()
            .unwrap_or_else(|| format!("dlg-{dialogue}-{}", self.id))
    }

    /// The key the subtitle shows: `display_key`, else the transcript key.
    #[must_use]
    pub fn display_key(&self, dialogue: &str) -> String {
        self.display_key
            .clone()
            .unwrap_or_else(|| self.text_key(dialogue))
    }

    /// The best variant for `speaker` and `listeners` (spec 36 § 7.2): a speaker match scores 2, a
    /// listener match 1, a `None` field 0; a variant naming a speaker or listener that does not match
    /// is not a candidate. The highest score wins, ties go to the earlier variant. `None` when no
    /// variant is a candidate.
    #[must_use]
    pub fn select(&self, speaker: &str, listeners: &[&str]) -> Option<&Variant> {
        self.select_index(speaker, listeners).map(|i| &self.variants[i])
    }

    /// The index of [`DialogueLine::select`]'s variant.
    #[must_use]
    pub fn select_index(&self, speaker: &str, listeners: &[&str]) -> Option<usize> {
        let mut best: Option<(u32, usize)> = None;
        for (index, variant) in self.variants.iter().enumerate() {
            let speaker_score = match &variant.speaker {
                None => 0,
                Some(s) if s == speaker => 2,
                Some(_) => continue,
            };
            let listener_score = match &variant.listener {
                None => 0,
                Some(l) if listeners.contains(&l.as_str()) => 1,
                Some(_) => continue,
            };
            let score = speaker_score + listener_score;
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, index));
            }
        }
        best.map(|(_, index)| index)
    }
}

impl DialogueAsset {
    /// The line with `id`.
    #[must_use]
    pub fn line(&self, id: &str) -> Option<&DialogueLine> {
        self.lines.iter().find(|l| l.id == id)
    }
}

/// A parse or check failure with its position (`file:line:col: message`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{file}:{line}:{col}: {message}")]
pub struct DialogueFormatError {
    /// The file name as given.
    pub file: String,
    /// 1-based line (1 for whole-file checks).
    pub line: usize,
    /// 1-based column.
    pub col: usize,
    /// What is wrong.
    pub message: String,
}

fn ron_error(file: &str, err: &ron::error::SpannedError) -> DialogueFormatError {
    DialogueFormatError {
        file: file.to_owned(),
        line: err.span.start.line,
        col: err.span.start.col,
        message: err.code.to_string(),
    }
}

fn check(
    file: &str,
    text: &str,
    needle: &str,
    ok: bool,
    message: impl FnOnce() -> String,
) -> Result<(), DialogueFormatError> {
    if ok {
        return Ok(());
    }
    // Point at the first mention of the offending value when there is one.
    let (line, col) = text
        .find(needle)
        .filter(|_| !needle.is_empty())
        .map_or((1, 1), |at| {
            let before = &text[..at];
            let line = before.matches('\n').count() + 1;
            let col = before.rfind('\n').map_or(at, |nl| at - nl - 1) + 1;
            (line, col)
        });
    Err(DialogueFormatError {
        file: file.to_owned(),
        line,
        col,
        message: message(),
    })
}

/// Whether `id` is a valid voice/line/dialogue id: lowercase `[a-z0-9_]`, 1 to 48 characters.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    (1..=48).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn check_version(file: &str, text: &str, found: u32, want: u32) -> Result<(), DialogueFormatError> {
    check(file, text, "format_version", found == want, || {
        format!("format_version {found} is not supported (this build reads {want})")
    })
}

/// Parses a `.voice.ron`.
///
/// # Errors
///
/// Bad RON, an unknown field, an unsupported version or a bad id.
pub fn parse_voice(file: &str, text: &str) -> Result<VoiceAsset, DialogueFormatError> {
    let voice: VoiceAsset = ron::from_str(text).map_err(|e| ron_error(file, &e))?;
    check_version(file, text, voice.format_version, VOICE_VERSION)?;
    check(file, text, &voice.id, valid_id(&voice.id), || {
        format!(
            "voice id {:?} must be lowercase [a-z0-9_], 1 to 48 characters",
            voice.id
        )
    })?;
    Ok(voice)
}

/// Checks a cue list (spec 36 § 8.1): `at >= 0` and non-decreasing, `until >= at`, exactly one of
/// `text_key` and `text`. Returns the first problem.
#[must_use]
pub fn cue_problem(cues: &[Cue]) -> Option<String> {
    let mut previous = 0.0f32;
    for (i, cue) in cues.iter().enumerate() {
        if !cue.at.is_finite() || cue.at < 0.0 {
            return Some(format!("cue {i}: `at` must be a number >= 0"));
        }
        if cue.at < previous {
            return Some(format!(
                "cue {i}: `at` {} is before the previous cue's {previous}",
                cue.at
            ));
        }
        previous = cue.at;
        if let Some(until) = cue.until
            && (!until.is_finite() || until < cue.at)
        {
            return Some(format!("cue {i}: `until` {until} is before `at` {}", cue.at));
        }
        if cue.text_key.is_some() == cue.text.is_some() {
            return Some(format!("cue {i}: give exactly one of `text_key` and `text`"));
        }
    }
    None
}

/// Parses a `<sound>.subs.ron`.
///
/// # Errors
///
/// Bad RON, an unknown field, an unsupported version, a negative priority or a bad cue.
pub fn parse_subtitles(file: &str, text: &str) -> Result<SubtitleTrack, DialogueFormatError> {
    let track: SubtitleTrack = ron::from_str(text).map_err(|e| ron_error(file, &e))?;
    check_version(file, text, track.format_version, SUBTITLES_VERSION)?;
    check(file, text, "priority", track.priority >= 0, || {
        format!("priority {} must be >= 0", track.priority)
    })?;
    if let Some(problem) = cue_problem(&track.cues) {
        check(file, text, "cues", false, || problem)?;
    }
    Ok(track)
}

/// Parses a `.dialogue.ron` and checks ids, unique lines, unique transcript keys and inline cues.
///
/// # Errors
///
/// The first problem, with its position.
pub fn parse_dialogue(file: &str, text: &str) -> Result<DialogueAsset, DialogueFormatError> {
    let dialogue: DialogueAsset = ron::from_str(text).map_err(|e| ron_error(file, &e))?;
    check_version(file, text, dialogue.format_version, DIALOGUE_VERSION)?;
    check(file, text, &dialogue.name, valid_id(&dialogue.name), || {
        format!(
            "dialogue name {:?} must be lowercase [a-z0-9_], 1 to 48 characters",
            dialogue.name
        )
    })?;
    let mut ids = BTreeSet::new();
    let mut keys = BTreeSet::new();
    for line in &dialogue.lines {
        let needle = format!("\"{}\"", line.id);
        check(file, text, &needle, valid_id(&line.id), || {
            format!(
                "line id {:?} must be lowercase [a-z0-9_], 1 to 48 characters",
                line.id
            )
        })?;
        check(file, text, &needle, ids.insert(line.id.clone()), || {
            format!("duplicate line id {:?}", line.id)
        })?;
        let key = line.text_key(&dialogue.name);
        check(file, text, &needle, keys.insert(key.clone()), || {
            format!("line {:?}: text key {key:?} is used by another line", line.id)
        })?;
        for variant in &line.variants {
            if let Some(problem) = variant.cues.as_deref().and_then(cue_problem) {
                check(file, text, &needle, false, || {
                    format!("line {:?}: {problem}", line.id)
                })?;
            }
        }
    }
    Ok(dialogue)
}

/// The `.subs.ron` sidecar path of a sound: `audio/a.ogg` gives `audio/a.subs.ron`.
#[must_use]
pub fn subtitles_sidecar(sound: &str) -> String {
    let (dir, name) = sound.rsplit_once('/').map_or(("", sound), |(d, n)| (d, n));
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    if dir.is_empty() {
        format!("{stem}.subs.ron")
    } else {
        format!("{dir}/{stem}.subs.ron")
    }
}

/// One cue expanded to absolute times in microseconds from the sound start (spec 36 § 8.2): `until`,
/// else the next cue's start, else `sound_us` (the sound's length) for the last cue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedCue {
    /// Start, microseconds from the sound start.
    pub start_us: u64,
    /// End (exclusive), microseconds from the sound start.
    pub end_us: u64,
    /// The cue's key, when it has one.
    pub text_key: Option<String>,
    /// The cue's raw text, when it has one.
    pub text: Option<String>,
}

/// Expands `cues` to [`TimedCue`]s for a sound of `sound_us` microseconds.
#[must_use]
pub fn expand_cues(cues: &[Cue], sound_us: u64) -> Vec<TimedCue> {
    let us = |s: f32| (f64::from(s.max(0.0)) * 1_000_000.0).round() as u64;
    cues.iter()
        .enumerate()
        .map(|(i, cue)| {
            let start_us = us(cue.at);
            let end_us = match (cue.until, cues.get(i + 1)) {
                (Some(until), _) => us(until),
                (None, Some(next)) => us(next.at),
                (None, None) => sound_us.max(start_us),
            };
            TimedCue {
                start_us,
                end_us,
                text_key: cue.text_key.clone(),
                text: cue.text.clone(),
            }
        })
        .collect()
}

/// The cue visible at `t_us` (`at` inclusive, `until` exclusive); `None` in a gap.
#[must_use]
pub fn cue_at(cues: &[TimedCue], t_us: u64) -> Option<&TimedCue> {
    cues.iter().rev().find(|c| c.start_us <= t_us && t_us < c.end_us)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIALOGUE: &str = r#"(
    format_version: 1,
    name: "ranger_intro",
    lines: [
        (id: "greeting", speaker: "ranger", listeners: ["player"],
         note: "Said when the player first arrives at the gate.",
         text_key: Some("dlg-ranger_intro-greeting"), display_key: None, mature: false,
         variants: [
            (speaker: None, listener: None, sound: "audio/dialogue/ranger_greeting.ogg", cues: None),
            (speaker: Some("ranger"), listener: Some("child"), sound: "audio/dialogue/ranger_greeting_child.ogg", cues: None),
         ]),
        (id: "farewell", speaker: "ranger", variants: []),
    ],
)"#;

    fn variant(speaker: Option<&str>, listener: Option<&str>, sound: &str) -> Variant {
        Variant {
            speaker: speaker.map(str::to_owned),
            listener: listener.map(str::to_owned),
            sound: sound.into(),
            cues: None,
        }
    }

    #[test]
    fn the_spec_example_parses_with_defaults() {
        let d = parse_dialogue("intro.dialogue.ron", DIALOGUE).expect("parses");
        assert_eq!(d.lines.len(), 2);
        let farewell = d.line("farewell").expect("line");
        assert_eq!(farewell.text_key("ranger_intro"), "dlg-ranger_intro-farewell");
        assert_eq!(farewell.display_key("ranger_intro"), "dlg-ranger_intro-farewell");
        assert!(farewell.listeners.is_empty() && !farewell.mature);
        let round = ron::to_string(&d).expect("writes");
        assert_eq!(parse_dialogue("x", &round).expect("again"), d);
    }

    #[test]
    fn selection_table() {
        let line = DialogueLine {
            id: "l".into(),
            speaker: "ranger".into(),
            listeners: vec![],
            note: String::new(),
            text_key: None,
            display_key: None,
            mature: false,
            variants: vec![
                variant(None, None, "any"),
                variant(Some("ranger"), None, "ranger"),
                variant(None, Some("child"), "to_child"),
                variant(Some("ranger"), Some("child"), "ranger_to_child"),
                variant(Some("ranger"), None, "ranger_second"),
                variant(Some("guard"), None, "guard"),
            ],
        };
        let pick =
            |speaker: &str, listeners: &[&str]| line.select(speaker, listeners).map(|v| v.sound.as_str());
        assert_eq!(pick("ranger", &["child"]), Some("ranger_to_child"), "2 + 1");
        assert_eq!(
            pick("ranger", &["player"]),
            Some("ranger"),
            "2 beats 0, first of a tie"
        );
        assert_eq!(pick("ranger", &[]), Some("ranger"));
        assert_eq!(pick("merchant", &["child"]), Some("to_child"), "1 beats 0");
        assert_eq!(pick("merchant", &[]), Some("any"));
        assert_eq!(
            pick("guard", &["child"]),
            Some("guard"),
            "a speaker match (2) beats a listener match (1)"
        );
        let only_named = DialogueLine {
            variants: vec![variant(Some("guard"), None, "guard")],
            ..line.clone()
        };
        assert_eq!(only_named.select("ranger", &[]), None, "no candidate");
    }

    #[test]
    fn bad_files_name_the_position() {
        let err = parse_dialogue(
            "a.dialogue.ron",
            &DIALOGUE.replace("\"farewell\"", "\"greeting\""),
        )
        .expect_err("an error");
        assert!(err.message.contains("duplicate line id"), "{err}");
        assert_eq!(err.line, 5, "{err}");
        let err = parse_dialogue(
            "a.dialogue.ron",
            &DIALOGUE.replace("mature: false", "mature: false, colour: 1"),
        )
        .expect_err("an error");
        assert!(err.to_string().starts_with("a.dialogue.ron:"), "{err}");
        let err = parse_dialogue("a", &DIALOGUE.replace("format_version: 1", "format_version: 2"))
            .expect_err("an error");
        assert!(err.message.contains("format_version 2"), "{err}");
        let err = parse_dialogue(
            "a",
            &DIALOGUE.replace("\"farewell\", speaker", "\"Fare Well\", speaker"),
        )
        .expect_err("an error");
        assert!(err.message.contains("lowercase"), "{err}");
    }

    /// Spec 36 § 7.2: text keys are validated unique, the default `dlg-<name>-<id>` included
    /// (lane spec-15-35, ADR-5778).
    #[test]
    fn a_text_key_used_by_two_lines_is_an_error() {
        let err = parse_dialogue(
            "a.dialogue.ron",
            &DIALOGUE.replace(
                "(id: \"farewell\", speaker: \"ranger\", variants: [])",
                "(id: \"farewell\", speaker: \"ranger\", text_key: Some(\"dlg-ranger_intro-greeting\"), variants: [])",
            ),
        )
        .expect_err("an error");
        assert!(err.message.contains("is used by another line"), "{err}");
        assert_eq!(err.line, 12, "{err}");
        // An explicit key equal to another line's default key collides too.
        let err = parse_dialogue(
            "a.dialogue.ron",
            &DIALOGUE.replace(
                "text_key: Some(\"dlg-ranger_intro-greeting\")",
                "text_key: Some(\"dlg-ranger_intro-farewell\")",
            ),
        )
        .expect_err("an error");
        assert!(err.message.contains("dlg-ranger_intro-farewell"), "{err}");
    }

    #[test]
    fn voices_parse_and_reject_bad_ids() {
        let v = parse_voice(
            "ranger.voice.ron",
            r#"(format_version: 1, id: "ranger", name_key: "voice-ranger-name", gender: Feminine, number: Singular)"#,
        )
        .expect("voice");
        assert_eq!((v.gender.as_str(), v.number.as_str()), ("feminine", "singular"));
        assert!(parse_voice("v", r#"(format_version: 1, id: "Ranger", name_key: "k")"#).is_err());
        assert!(
            parse_voice(
                "v",
                &format!(r#"(format_version: 1, id: "{}", name_key: "k")"#, "a".repeat(49))
            )
            .is_err()
        );
        assert!(valid_id(&"a".repeat(48)));
    }

    #[test]
    fn subtitle_tracks_check_cues() {
        let ok = r#"(format_version: 1, priority: 1, keep_breaks: false, one_at_a_time: false, cues: [(at: 0.0, until: Some(2.2), text_key: Some("dlg-ranger_intro-greeting")), (at: 2.4, until: None, text: Some("Raw text (not localizable)"))])"#;
        let track = parse_subtitles("a.subs.ron", ok).expect("parses");
        assert_eq!(track.cues.len(), 2);
        let bad_order = ok.replace("at: 2.4, until: None", "at: 1.0, until: Some(0.5)");
        assert!(
            parse_subtitles("a", &bad_order)
                .expect_err("an error")
                .message
                .contains("before")
        );
        let both = ok.replace("text: Some(\"Raw", "text_key: Some(\"k\"), text: Some(\"Raw");
        assert!(
            parse_subtitles("a", &both)
                .expect_err("an error")
                .message
                .contains("exactly one")
        );
        assert!(parse_subtitles("a", &ok.replace("priority: 1", "priority: -1")).is_err());
        assert_eq!(
            subtitles_sidecar("audio/dialogue/a.ogg"),
            "audio/dialogue/a.subs.ron"
        );
        assert_eq!(subtitles_sidecar("a.wav"), "a.subs.ron");
    }

    #[test]
    fn cue_timing_boundaries_and_gaps() {
        let cue = |at: f32, until: Option<f32>, text: &str| Cue {
            at,
            until,
            text_key: None,
            text: Some(text.into()),
        };
        let timed = expand_cues(
            &[cue(0.0, Some(1.0), "a"), cue(1.5, None, "b"), cue(2.0, None, "c")],
            3_000_000,
        );
        assert_eq!(
            timed.iter().map(|c| (c.start_us, c.end_us)).collect::<Vec<_>>(),
            [(0, 1_000_000), (1_500_000, 2_000_000), (2_000_000, 3_000_000)]
        );
        let text = |t| cue_at(&timed, t).and_then(|c| c.text.as_deref());
        assert_eq!(text(0), Some("a"), "at inclusive");
        assert_eq!(text(1_000_000), None, "until exclusive, then a gap");
        assert_eq!(text(1_500_000), Some("b"));
        assert_eq!(text(2_000_000), Some("c"), "None runs to the next start");
        assert_eq!(text(3_000_000), None, "the last cue ends with the sound");
    }

    #[test]
    fn fingerprints_list_the_fields() {
        assert!(voice_schema_fingerprint().contains("name_key"));
        assert!(voice_schema_fingerprint().contains("Feminine"));
        assert!(dialogue_schema_fingerprint().contains("display_key"));
        assert!(subtitles_schema_fingerprint().contains("one_at_a_time"));
        assert!(dialogue_migration_sources().is_empty());
    }
}
