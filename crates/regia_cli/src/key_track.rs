// SPDX-License-Identifier: MPL-2.0
//! Key-track files (`.track.ron`, format `key-track` v1, ADR-8403).
//!
//! A track is a list of scripted input steps. [`PreparedTrack::replay`] walks them on a virtual
//! clock that starts at 0 and never sleeps: a tap is a down edge and an up edge, and an expect
//! matches a log line whose time is at or before the deadline. The same bytes replay the same
//! edges. Durations live in the file; the defaults below apply only when a field is absent.
//!
//! `regia play-gate replay` and `regia play-gate check` read this module. A fresh-home install
//! and physical keys are `regia play-gate run` ([`crate::play_install`], ADR-8452).

use std::fs;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The `.track.ron` format version. Any other value is refused.
pub const KEY_TRACK_FORMAT_VERSION: u32 = 1;

/// Hold of a [`Step::Tap`] or one pulse of [`Step::TapN`] when `millis` is omitted.
pub const DEFAULT_TAP_MILLIS: u64 = 200;
/// Deadline added to the clock by [`Step::Expect`] when `within_millis` is omitted.
pub const DEFAULT_EXPECT_WITHIN_MILLIS: u64 = 1_000;
/// Gap between the starts of [`Step::TapN`] pulses when `gap_millis` is omitted.
pub const DEFAULT_TAPN_GAP_MILLIS: u64 = 300;
/// Time a [`Step::Mouse`] move takes when `millis` is omitted.
pub const DEFAULT_MOUSE_MILLIS: u64 = 300;

/// The track text could not be read as key-track v1.
pub const CODE_PARSE: &str = "track_parse";
/// `format_version` is not [`KEY_TRACK_FORMAT_VERSION`].
pub const CODE_VERSION: &str = "track_version";
/// The track or log file could not be read.
pub const CODE_IO: &str = "track_io";
/// A required expect found no log line inside its deadline.
pub const CODE_EXPECT_MISSED: &str = "track_expect_missed";
/// `--require-clean-log` found an `ERROR` record, or a `{` line that is not JSON.
pub const CODE_LOG_ERROR: &str = "track_log_error";

fn default_tap_millis() -> u64 {
    DEFAULT_TAP_MILLIS
}

fn default_expect_within() -> u64 {
    DEFAULT_EXPECT_WITHIN_MILLIS
}

fn default_gap_millis() -> u64 {
    DEFAULT_TAPN_GAP_MILLIS
}

fn default_mouse_millis() -> u64 {
    DEFAULT_MOUSE_MILLIS
}

/// Why a track was refused or a replay stopped.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrackError {
    /// The text is not a v1 track, or a step breaks a rule (empty target, bad pattern).
    #[error("{CODE_PARSE}: {0}")]
    Parse(String),
    /// `format_version` is not 1. The message names the version so a newer file is refused.
    #[error("{CODE_VERSION}: key-track format_version {found} is not version {current}")]
    Version {
        /// The version stored in the file.
        found: u32,
        /// The version this build reads.
        current: u32,
    },
    /// A file could not be read. The message names the path.
    #[error("{CODE_IO}: {0}")]
    Io(String),
    /// A required expect missed. `pattern` is the pattern that missed.
    #[error("{CODE_EXPECT_MISSED}: {pattern}")]
    ExpectMissed {
        /// The pattern that found no line.
        pattern: String,
    },
    /// The log failed the clean-log scan.
    #[error("{CODE_LOG_ERROR}: {0}")]
    Log(String),
}

impl TrackError {
    /// The stable code: `track_parse`, `track_version`, `track_io`, `track_expect_missed`
    /// or `track_log_error`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => CODE_PARSE,
            Self::Version { .. } => CODE_VERSION,
            Self::Io(_) => CODE_IO,
            Self::ExpectMissed { .. } => CODE_EXPECT_MISSED,
            Self::Log(_) => CODE_LOG_ERROR,
        }
    }
}

/// One step of a key track, in source order.
///
/// Times are milliseconds on the virtual clock. A missing duration field takes the default
/// named on that variant. An unknown field is refused (`track_parse` naming it, ADR-8553): a
/// misspelled `millis` never falls back to the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Step {
    /// Stores a section name on the events that follow, until the next section.
    Section {
        /// Section name. Empty is allowed.
        name: String,
    },
    /// Advances the clock. No edges.
    Wait {
        /// Milliseconds added to the clock.
        millis: u64,
    },
    /// Matches one log line at or before `clock + within_millis`. Does not advance the clock.
    Expect {
        /// A regular expression (the `regex` crate). Empty is refused.
        pattern: String,
        /// How far past the clock a line may be and still match.
        #[serde(default = "default_expect_within")]
        within_millis: u64,
        /// When true, a miss is a note. When false, a miss stops the replay.
        #[serde(default)]
        optional: bool,
    },
    /// Down at the clock, up `millis` later, then the clock advances by `millis`.
    Tap {
        /// Opaque target, such as `input.confirm` or `key:Escape`. Empty is refused.
        target: String,
        /// How long the target stays down.
        #[serde(default = "default_tap_millis")]
        millis: u64,
    },
    /// Down, if this target is not already held. The clock does not move.
    Press {
        /// Opaque target. Empty is refused.
        target: String,
    },
    /// Up, if this target is held. A target that is not held records nothing.
    Release {
        /// Opaque target. Empty is refused.
        target: String,
    },
    /// Up for every target [`Step::Press`] or [`Step::Hold`] still holds, in press order.
    ReleaseAll,
    /// Down for each target that is not already held, advance, then up for those targets.
    Hold {
        /// Targets pressed together. Empty is refused.
        targets: Vec<String>,
        /// How long those targets stay down.
        millis: u64,
    },
    /// A note at the current clock. No edges and no clock advance.
    Note {
        /// Note text.
        text: String,
    },
    /// `count` taps of `target`. Pulse `i` starts at `i * gap_millis` and holds for
    /// [`DEFAULT_TAP_MILLIS`]. The clock then advances by `(count - 1) * gap_millis + hold`.
    TapN {
        /// Opaque target. Empty is refused.
        target: String,
        /// How many taps. Zero is refused.
        count: u32,
        /// Milliseconds between the start of one tap and the next.
        #[serde(default = "default_gap_millis")]
        gap_millis: u64,
    },
    /// A pointer move at the current clock. The clock advances by `millis`.
    Mouse {
        /// Horizontal counts. Negative is left.
        dx: i32,
        /// Vertical counts. Negative is up.
        dy: i32,
        /// How long the move takes.
        #[serde(default = "default_mouse_millis")]
        millis: u64,
    },
    /// A wheel notch at the current clock. The clock does not move.
    Wheel {
        /// Notches. Negative is toward the user.
        notches: i32,
    },
}

/// A `.track.ron` file: `format_version` and `steps`. Unknown fields are refused (ADR-8553).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyTrack {
    /// Must be [`KEY_TRACK_FORMAT_VERSION`]. A different number is [`TrackError::Version`].
    pub format_version: u32,
    /// Steps in replay order. Empty is a valid no-op.
    pub steps: Vec<Step>,
}

impl KeyTrack {
    /// Pretty RON, with `\n` line endings on every platform.
    ///
    /// # Errors
    ///
    /// The encoder refused the value.
    pub fn to_ron(&self) -> Result<String, TrackError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| TrackError::Parse(e.to_string()))
    }
}

/// A track whose expect patterns have been compiled.
#[derive(Debug, Clone)]
pub struct PreparedTrack {
    track: KeyTrack,
    patterns: Vec<Regex>,
}

impl PreparedTrack {
    /// Parses and checks a track. `format_version` must be 1. It is read before the full parse,
    /// so a newer file whose body no longer reads as v1 is still [`TrackError::Version`]. Expect
    /// patterns are compiled here.
    ///
    /// # Errors
    ///
    /// [`TrackError::Parse`] or [`TrackError::Version`].
    pub fn parse(text: &str) -> Result<Self, TrackError> {
        // A peek that fails (no top-level map, no version) falls through to the full parse, whose
        // error is the precise one.
        if let Ok(found) = regia_core::version::peek_ron_version(text)
            && found != KEY_TRACK_FORMAT_VERSION
        {
            return Err(TrackError::Version {
                found,
                current: KEY_TRACK_FORMAT_VERSION,
            });
        }
        let track: KeyTrack = ron::from_str(text).map_err(|e| TrackError::Parse(e.to_string()))?;
        Self::from_track(track)
    }

    /// Reads a track from `path`. IO errors name the path.
    ///
    /// # Errors
    ///
    /// [`TrackError::Io`], [`TrackError::Parse`] or [`TrackError::Version`].
    pub fn load(path: &Path) -> Result<Self, TrackError> {
        let text = fs::read_to_string(path)
            .map_err(|e| TrackError::Io(format!("reading {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| match e {
            TrackError::Parse(msg) => TrackError::Parse(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    fn from_track(track: KeyTrack) -> Result<Self, TrackError> {
        if track.format_version != KEY_TRACK_FORMAT_VERSION {
            return Err(TrackError::Version {
                found: track.format_version,
                current: KEY_TRACK_FORMAT_VERSION,
            });
        }
        let mut patterns = Vec::new();
        for step in &track.steps {
            match step {
                Step::Expect { pattern, .. } => {
                    if pattern.is_empty() {
                        return Err(TrackError::Parse("expect pattern is empty".to_string()));
                    }
                    patterns.push(
                        Regex::new(pattern)
                            .map_err(|e| TrackError::Parse(format!("expect pattern `{pattern}`: {e}")))?,
                    );
                }
                Step::Tap { target, .. }
                | Step::Press { target }
                | Step::Release { target }
                | Step::TapN { target, .. } => {
                    require_target(target)?;
                    if let Step::TapN { count, .. } = step
                        && *count == 0
                    {
                        return Err(TrackError::Parse("tapn count is 0".to_string()));
                    }
                }
                Step::Hold { targets, .. } => {
                    if targets.is_empty() {
                        return Err(TrackError::Parse("hold targets are empty".to_string()));
                    }
                    for target in targets {
                        require_target(target)?;
                    }
                }
                Step::Section { .. }
                | Step::Wait { .. }
                | Step::ReleaseAll
                | Step::Note { .. }
                | Step::Mouse { .. }
                | Step::Wheel { .. } => {}
            }
        }
        Ok(Self { track, patterns })
    }

    /// The parsed file, including defaults serde filled in.
    #[must_use]
    pub fn track(&self) -> &KeyTrack {
        &self.track
    }

    /// Replays `log` on a virtual clock. A required miss stops the walk and sets [`Replay::failed`].
    /// Lines before the previous match stay available until a match moves the mark past them.
    #[must_use]
    pub fn replay(&self, log: &[LogLine]) -> Replay {
        let mut out = Replay::default();
        let mut clock: u64 = 0;
        let mut held: Vec<String> = Vec::new();
        let mut section = String::new();
        let mut mark = 0usize;
        let mut pattern_i = 0usize;
        for step in &self.track.steps {
            match step {
                Step::Section { name } => section = name.clone(),
                Step::Note { text } => out.notes.push(NoteRecord {
                    at_millis: clock,
                    text: text.clone(),
                }),
                Step::Wait { millis } => clock = clock.saturating_add(*millis),
                Step::Expect {
                    pattern,
                    within_millis,
                    optional,
                } => {
                    let regex = &self.patterns[pattern_i];
                    pattern_i += 1;
                    let deadline = clock.saturating_add(*within_millis);
                    let found = log.iter().enumerate().skip(mark).find_map(|(index, line)| {
                        (line.at_millis <= deadline && regex.is_match(&line.text)).then_some(index)
                    });
                    match found {
                        Some(index) => {
                            out.expects.push(ExpectMatch {
                                pattern: pattern.clone(),
                                optional: *optional,
                                matched: true,
                                line_index: Some(index),
                            });
                            mark = index + 1;
                        }
                        None if *optional => {
                            out.expects.push(ExpectMatch {
                                pattern: pattern.clone(),
                                optional: true,
                                matched: false,
                                line_index: None,
                            });
                            out.notes.push(NoteRecord {
                                at_millis: clock,
                                text: format!("optional expect missed: {pattern}"),
                            });
                        }
                        None => {
                            out.expects.push(ExpectMatch {
                                pattern: pattern.clone(),
                                optional: false,
                                matched: false,
                                line_index: None,
                            });
                            out.failed = Some(pattern.clone());
                            break;
                        }
                    }
                }
                Step::Tap { target, millis } => {
                    push_key(&mut out, clock, Edge::Down, target, &section);
                    let up_at = clock.saturating_add(*millis);
                    push_key(&mut out, up_at, Edge::Up, target, &section);
                    clock = up_at;
                }
                Step::Press { target } => {
                    if !held.iter().any(|item| item == target) {
                        push_key(&mut out, clock, Edge::Down, target, &section);
                        held.push(target.clone());
                    }
                }
                Step::Release { target } => {
                    if release_held(&mut held, target) {
                        push_key(&mut out, clock, Edge::Up, target, &section);
                    }
                }
                Step::ReleaseAll => {
                    for target in held.drain(..) {
                        push_key(&mut out, clock, Edge::Up, &target, &section);
                    }
                }
                Step::Hold { targets, millis } => {
                    let mut pressed = Vec::new();
                    for target in targets {
                        if !held.iter().any(|item| item == target) {
                            push_key(&mut out, clock, Edge::Down, target, &section);
                            held.push(target.clone());
                            pressed.push(target.clone());
                        }
                    }
                    clock = clock.saturating_add(*millis);
                    for target in pressed {
                        if release_held(&mut held, &target) {
                            push_key(&mut out, clock, Edge::Up, &target, &section);
                        }
                    }
                }
                Step::TapN {
                    target,
                    count,
                    gap_millis,
                } => {
                    let count = u64::from(*count);
                    let hold = DEFAULT_TAP_MILLIS;
                    for i in 0..count {
                        let start = clock.saturating_add(i.saturating_mul(*gap_millis));
                        push_key(&mut out, start, Edge::Down, target, &section);
                        push_key(&mut out, start.saturating_add(hold), Edge::Up, target, &section);
                    }
                    let gaps = count.saturating_sub(1).saturating_mul(*gap_millis);
                    clock = clock.saturating_add(gaps).saturating_add(hold);
                }
                Step::Mouse { dx, dy, millis } => {
                    out.motions.push(MotionEvent {
                        at_millis: clock,
                        dx: *dx,
                        dy: *dy,
                        millis: *millis,
                        section: section.clone(),
                    });
                    clock = clock.saturating_add(*millis);
                }
                Step::Wheel { notches } => {
                    out.wheels.push(WheelEvent {
                        at_millis: clock,
                        notches: *notches,
                        section: section.clone(),
                    });
                }
            }
        }
        out
    }

    /// Replays an untimed log: each line is visible at time 0, so order matters and the clock does not.
    #[must_use]
    pub fn replay_text(&self, text: &str) -> Replay {
        let log: Vec<LogLine> = text
            .lines()
            .map(|line| LogLine {
                at_millis: 0,
                text: line.to_string(),
            })
            .collect();
        self.replay(&log)
    }
}

fn require_target(target: &str) -> Result<(), TrackError> {
    if target.is_empty() {
        Err(TrackError::Parse("target is empty".to_string()))
    } else {
        Ok(())
    }
}

fn push_key(out: &mut Replay, at_millis: u64, edge: Edge, target: &str, section: &str) {
    out.keys.push(KeyEvent {
        at_millis,
        edge,
        target: target.to_string(),
        section: section.to_string(),
    });
}

fn release_held(held: &mut Vec<String>, target: &str) -> bool {
    match held.iter().position(|item| item == target) {
        Some(index) => {
            held.remove(index);
            true
        }
        None => false,
    }
}

/// One log line the caller supplies. `at_millis` is the line's time on the same clock as the track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// When the line was produced, in milliseconds from the start of the replay.
    pub at_millis: u64,
    /// The line text, without its trailing newline.
    pub text: String,
}

/// A key or button going down or up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The target went down.
    Down,
    /// The target went up.
    Up,
}

/// One key edge the replay produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// Virtual clock at this edge.
    pub at_millis: u64,
    /// Down or up.
    pub edge: Edge,
    /// The step's target, unchanged.
    pub target: String,
    /// The section named by the latest [`Step::Section`], or empty.
    pub section: String,
}

/// One pointer move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotionEvent {
    /// Virtual clock when the move starts.
    pub at_millis: u64,
    /// Horizontal counts.
    pub dx: i32,
    /// Vertical counts.
    pub dy: i32,
    /// How long the move takes.
    pub millis: u64,
    /// The current section, or empty.
    pub section: String,
}

/// One wheel notch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelEvent {
    /// Virtual clock at the notch.
    pub at_millis: u64,
    /// Notches.
    pub notches: i32,
    /// The current section, or empty.
    pub section: String,
}

/// Whether one expect matched, and which log line it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectMatch {
    /// The pattern from the step.
    pub pattern: String,
    /// The step's `optional` flag.
    pub optional: bool,
    /// True when a line matched.
    pub matched: bool,
    /// Index into the log slice. `None` on a miss.
    pub line_index: Option<usize>,
}

/// A note step, or the text recorded for an optional miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRecord {
    /// Virtual clock when the note was recorded.
    pub at_millis: u64,
    /// Note text.
    pub text: String,
}

/// The edges, matches and notes of one replay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Replay {
    /// Key edges, in time order.
    pub keys: Vec<KeyEvent>,
    /// Pointer moves, in time order.
    pub motions: Vec<MotionEvent>,
    /// Wheel notches, in time order.
    pub wheels: Vec<WheelEvent>,
    /// One entry per expect that was reached.
    pub expects: Vec<ExpectMatch>,
    /// Note steps and optional misses.
    pub notes: Vec<NoteRecord>,
    /// The pattern of the required expect that missed, when the walk stopped.
    pub failed: Option<String>,
}

impl Replay {
    /// True when every required expect that was reached matched.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.failed.is_none()
    }

    /// Required expects that missed. At most one: the walk stops on the first.
    pub fn missed(&self) -> impl Iterator<Item = &ExpectMatch> {
        self.expects
            .iter()
            .filter(|expect| !expect.matched && !expect.optional)
    }
}

/// One JSON-lines record that failed the clean-log scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogHit {
    /// 1-based line number in the log text.
    pub line: usize,
    /// The record's `target` field, or empty.
    pub target: String,
    /// The record's `msg` field, or why the line was not JSON.
    pub message: String,
}

/// Scans a JSON-lines log the way the package smoke does.
///
/// Blank lines and lines that do not start with `{` are ignored. A line that starts with `{`
/// but is not JSON fails. A record whose `level` is exactly `ERROR` fails. Other levels pass.
///
/// # Errors
///
/// The hits, in order. `Ok(())` when nothing failed.
pub fn scan_error_log(text: &str) -> Result<(), Vec<LogHit>> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                hits.push(LogHit {
                    line: line_no,
                    target: String::new(),
                    message: format!("unparseable JSON on line {line_no}"),
                });
                continue;
            }
        };
        if value.get("level").and_then(|level| level.as_str()) == Some("ERROR") {
            hits.push(LogHit {
                line: line_no,
                target: value
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                message: value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    if hits.is_empty() { Ok(()) } else { Err(hits) }
}

/// Names a clean-log hit the way the package smoke does: `target: message`, or just the message.
#[must_use]
pub fn name_log_hit(hit: &LogHit) -> String {
    if hit.target.is_empty() {
        hit.message.clone()
    } else {
        format!("{}: {}", hit.target, hit.message)
    }
}

/// The serde shape of `.track.ron` for `cargo xtask check-formats` (ADR-8403): the fields of
/// [`KeyTrack`] and the variant names and payload shapes of [`Step`].
#[must_use]
pub fn key_track_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    join_field_lists(&[
        serde_field_names::<KeyTrack>(),
        serde_variant_names::<Step>(),
        serde_variant_shapes::<Step>(),
    ])
}

/// Versions a migrator reads. v1 is the first version, so this is empty.
#[must_use]
pub fn key_track_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// A log line `MILLIS|text` carries its own clock. Any other line is untimed (`at_millis` 0).
#[must_use]
pub fn log_line_from_text(line: &str) -> LogLine {
    if let Some((millis, text)) = split_timed(line) {
        LogLine {
            at_millis: millis,
            text: text.to_string(),
        }
    } else {
        LogLine {
            at_millis: 0,
            text: line.to_string(),
        }
    }
}

fn split_timed(line: &str) -> Option<(u64, &str)> {
    let (head, tail) = line.split_once('|')?;
    if head.is_empty() || !head.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let millis = head.parse().ok()?;
    Some((millis, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK: &str = r#"(
        format_version: 1,
        steps: [
            Tap(target: "input.confirm", millis: 200),
            Expect(pattern: "player ready", within_millis: 1000, optional: false),
            Press(target: "key:Escape"),
        ],
    )"#;

    fn line(at_millis: u64, text: &str) -> LogLine {
        LogLine {
            at_millis,
            text: text.to_string(),
        }
    }

    #[test]
    fn track_replays_keys_and_expects_log_lines() {
        let track = PreparedTrack::parse(TRACK).expect("track parses");
        let log = [line(50, "boot"), line(400, "player ready")];
        let replay = track.replay(&log);
        assert!(replay.ok(), "{replay:?}");
        assert_eq!(
            replay.keys,
            vec![
                KeyEvent {
                    at_millis: 0,
                    edge: Edge::Down,
                    target: "input.confirm".to_string(),
                    section: String::new(),
                },
                KeyEvent {
                    at_millis: 200,
                    edge: Edge::Up,
                    target: "input.confirm".to_string(),
                    section: String::new(),
                },
                KeyEvent {
                    at_millis: 200,
                    edge: Edge::Down,
                    target: "key:Escape".to_string(),
                    section: String::new(),
                },
            ]
        );
        assert_eq!(replay.expects.len(), 1);
        assert!(replay.expects[0].matched);
        assert_eq!(replay.expects[0].line_index, Some(1));

        let missed = track.replay(&[line(50, "boot")]);
        assert!(!missed.ok());
        assert_eq!(missed.failed.as_deref(), Some("player ready"));
        assert_eq!(missed.missed().count(), 1);
        assert_eq!(missed.keys.len(), 2, "the press is after the miss");
    }

    #[test]
    fn an_optional_miss_is_a_note_and_a_future_version_is_refused() {
        let text = TRACK.replace("optional: false", "optional: true");
        let track = PreparedTrack::parse(&text).expect("parses");
        let replay = track.replay(&[line(50, "boot")]);
        assert!(replay.ok(), "{replay:?}");
        assert!(
            replay
                .notes
                .iter()
                .any(|note| note.text.contains("optional expect missed"))
        );
        assert!(replay.keys.iter().any(|key| key.target == "key:Escape"));

        let newer = TRACK.replace("format_version: 1", "format_version: 2");
        let err = PreparedTrack::parse(&newer).expect_err("v2 is refused");
        assert_eq!(err.code(), CODE_VERSION);
        assert!(err.to_string().contains("version"), "{err}");

        let zero = TRACK.replace("format_version: 1", "format_version: 0");
        assert_eq!(PreparedTrack::parse(&zero).expect_err("v0").code(), CODE_VERSION);
    }

    #[test]
    fn expects_do_not_reuse_a_line_and_a_late_line_misses() {
        let text = r#"(
            format_version: 1,
            steps: [
                Expect(pattern: "ready", within_millis: 1000, optional: false),
                Expect(pattern: "ready", within_millis: 1000, optional: false),
            ],
        )"#;
        let track = PreparedTrack::parse(text).expect("parses");
        let once = track.replay(&[line(10, "ready")]);
        assert!(!once.ok());
        assert_eq!(once.expects[0].line_index, Some(0));

        let early = r#"(
            format_version: 1,
            steps: [Expect(pattern: "ready", within_millis: 100, optional: false)],
        )"#;
        let early = PreparedTrack::parse(early).expect("parses");
        // The line exists, and it is after the deadline, so it does not match.
        let late = early.replay(&[line(0, "boot"), line(500, "ready")]);
        assert!(!late.ok());
        // A line from before the step still matches.
        let prior = early.replay(&[line(0, "ready")]);
        assert!(prior.ok(), "{prior:?}");
    }

    #[test]
    fn round_trip_and_edges_follow_the_clock() {
        let text = r#"(
            format_version: 1,
            steps: [
                Section(name: "boot"),
                Tap(target: "input.confirm", millis: 200),
                Press(target: "key:Escape"),
                Press(target: "key:Escape"),
                Release(target: "missing"),
                Hold(targets: ["key:Shift"], millis: 100),
                TapN(target: "input.confirm", count: 2, gap_millis: 300),
                Mouse(dx: 4, dy: -1, millis: 300),
                Wheel(notches: -1),
                ReleaseAll,
                Note(text: "done"),
            ],
        )"#;
        let prepared = PreparedTrack::parse(text).expect("parses");
        let again = PreparedTrack::parse(&prepared.track().to_ron().expect("ron")).expect("reparse");
        assert_eq!(prepared.track(), again.track());

        let replay = prepared.replay(&[]);
        assert!(replay.ok());
        assert_eq!(replay.keys[0].section, "boot");
        // Second press of Escape records nothing; a release of a target that is not held records nothing.
        let escape_downs = replay
            .keys
            .iter()
            .filter(|key| key.target == "key:Escape" && key.edge == Edge::Down)
            .count();
        assert_eq!(escape_downs, 1);
        assert!(!replay.keys.iter().any(|key| key.target == "missing"));
        // Hold: down at the tap's end (200), up 100 later.
        let shift: Vec<_> = replay
            .keys
            .iter()
            .filter(|key| key.target == "key:Shift")
            .map(|key| (key.edge, key.at_millis))
            .collect();
        assert_eq!(shift, vec![(Edge::Down, 200), (Edge::Up, 300)]);
        // TapN starts at 300: down/up at 300/500 and 600/800.
        let taps: Vec<_> = replay
            .keys
            .iter()
            .filter(|key| key.target == "input.confirm" && key.at_millis >= 300)
            .map(|key| (key.edge, key.at_millis))
            .collect();
        assert_eq!(
            taps,
            vec![
                (Edge::Down, 300),
                (Edge::Up, 500),
                (Edge::Down, 600),
                (Edge::Up, 800),
            ]
        );
        assert_eq!(replay.motions[0].at_millis, 800);
        assert_eq!(replay.wheels[0].at_millis, 1100);
        // ReleaseAll lifts Escape, which was still held, at the wheel's clock.
        assert!(
            replay
                .keys
                .iter()
                .any(|key| { key.target == "key:Escape" && key.edge == Edge::Up && key.at_millis == 1100 })
        );
        assert_eq!(replay.notes[0].text, "done");
    }

    #[test]
    fn empty_track_is_a_noop_and_bad_steps_are_parse_errors() {
        let empty = PreparedTrack::parse("(format_version: 1, steps: [])").expect("empty is valid");
        assert!(empty.replay(&[]).ok());
        assert_eq!(
            PreparedTrack::parse("(format_version: 1, steps: [Tap(target: \"\", millis: 1)])")
                .expect_err("empty target")
                .code(),
            CODE_PARSE
        );
        assert_eq!(
            PreparedTrack::parse("(format_version: 1, steps: [Hold(targets: [], millis: 1)])")
                .expect_err("empty hold")
                .code(),
            CODE_PARSE
        );
        assert_eq!(
            PreparedTrack::parse(
                "(format_version: 1, steps: [TapN(target: \"k\", count: 0, gap_millis: 1)])"
            )
            .expect_err("zero tapn")
            .code(),
            CODE_PARSE
        );
        assert_eq!(
            PreparedTrack::parse(
                "(format_version: 1, steps: [Expect(pattern: \"[\", within_millis: 1, optional: false)])"
            )
            .expect_err("bad regex")
            .code(),
            CODE_PARSE
        );
    }

    #[test]
    fn error_log_scan_flags_error_records_only() {
        let clean = "boot\n{\"level\":\"INFO\",\"target\":\"app\",\"msg\":\"hi\"}\n";
        assert!(scan_error_log(clean).is_ok());
        let dirty = "{\"level\":\"ERROR\",\"target\":\"app\",\"msg\":\"nope\"}\nnot json\n{";
        let hits = scan_error_log(dirty).expect_err("error record");
        assert_eq!(hits.len(), 2);
        assert_eq!(name_log_hit(&hits[0]), "app: nope");
        assert!(hits[1].message.contains("unparseable"));
    }

    #[test]
    fn schema_fingerprint_lists_every_step_shape() {
        let fp = key_track_schema_fingerprint();
        assert_eq!(
            fp,
            "format_version,steps;Section,Wait,Expect,Tap,Press,Release,ReleaseAll,Hold,Note,TapN,Mouse,Wheel;0:{name},1:{millis},2:{pattern,within_millis,optional},3:{target,millis},4:{target},5:{target},6:unit,7:{targets,millis},8:{text},9:{target,count,gap_millis},10:{dx,dy,millis},11:{notches}"
        );
        assert!(key_track_migration_sources().is_empty());
        eprintln!("KEY_TRACK_HASH {}", blake3::hash(fp.as_bytes()).to_hex());
    }

    #[test]
    fn timed_log_lines_split_on_a_leading_integer() {
        assert_eq!(log_line_from_text("400|player ready").at_millis, 400);
        assert_eq!(log_line_from_text("400|player ready").text, "player ready");
        assert_eq!(log_line_from_text("player ready").at_millis, 0);
        assert_eq!(log_line_from_text("{\"level\":\"INFO\"}").at_millis, 0);
    }

    /// A newer track whose body no longer reads as v1 is still a version error naming the
    /// version (the peek runs before the full parse, ADR-8553).
    #[test]
    fn newer_key_track_with_a_foreign_body_names_the_version() {
        let err =
            PreparedTrack::parse(r#"(format_version: 2, steps: "no")"#).expect_err("v2 is not this engine");
        assert_eq!(err.code(), CODE_VERSION, "{err}");
        assert!(err.to_string().contains("format_version 2"), "{err}");
        // A v1 file with a broken body stays a parse error.
        let err = PreparedTrack::parse(r#"(format_version: 1, steps: "no")"#).expect_err("not a list");
        assert_eq!(err.code(), CODE_PARSE, "{err}");
    }

    /// Unknown fields are refused, on the file and on a step (ADR-8553).
    #[test]
    fn key_track_rejects_a_misspelled_field() {
        let err =
            PreparedTrack::parse(r#"(format_version: 1, steps: [Tap(target: "input.confirm", milis: 5)])"#)
                .expect_err("milis is not a field");
        assert_eq!(err.code(), CODE_PARSE, "{err}");
        assert!(err.to_string().contains("milis"), "{err}");
        let err = PreparedTrack::parse(r#"(format_version: 1, stpes: [], steps: [])"#).expect_err("stpes");
        assert_eq!(err.code(), CODE_PARSE, "{err}");
        assert!(err.to_string().contains("stpes"), "{err}");
        // The documented field still reads, and the unit variant still parses.
        let track = PreparedTrack::parse(
            r#"(format_version: 1, steps: [Tap(target: "input.confirm", millis: 5), ReleaseAll])"#,
        )
        .expect("millis is a field");
        assert_eq!(track.track().steps.len(), 2);
    }
}
