// SPDX-License-Identifier: MPL-2.0
//! Pure text-editing helpers behind [`crate::text_input::TextInput`] (ADR-336): grapheme and word
//! boundaries (Unicode UAX #29 through `unicode-segmentation`), logical lines, input filters and
//! the undo history with typing coalescing. No ECS here, so every rule is tested on plain strings.

use unicode_segmentation::UnicodeSegmentation;

use crate::line_break_sa::{has_sa, is_sa_char, with_sa_breaks};

/// The grapheme boundary before `i` (0 at the start). A grapheme cluster is what a reader sees as
/// one character: a base letter with its combining marks, an emoji sequence, a Hangul syllable.
pub fn prev_grapheme(s: &str, i: usize) -> usize {
    let i = i.min(s.len());
    s.grapheme_indices(true)
        .map(|(b, _)| b)
        .take_while(|b| *b < i)
        .last()
        .unwrap_or(0)
}

/// The grapheme boundary after `i` (`s.len()` at the end).
pub fn next_grapheme(s: &str, i: usize) -> usize {
    s.grapheme_indices(true)
        .map(|(b, _)| b)
        .find(|b| *b > i)
        .unwrap_or(s.len())
}

/// `i` moved back to the nearest grapheme boundary at or before it.
pub fn snap_grapheme(s: &str, i: usize) -> usize {
    let i = i.min(s.len());
    if i == s.len() {
        return i;
    }
    s.grapheme_indices(true)
        .map(|(b, _)| b)
        .take_while(|b| *b <= i)
        .last()
        .unwrap_or(0)
}

/// Word-boundary segments as byte ranges (ADR-3323). For SA scripts (Thai, Lao, Khmer, Myanmar,
/// which write words without spaces) the segments are the dictionary words: the segmenter's
/// boundaries, plus the UAX #29 boundaries at the run's edges (a Latin word next to Thai, the
/// spaces around a clause). UAX #29's own boundaries *inside* an SA run are syllable artifacts
/// (it has no Thai dictionary) and are dropped. Texts without SA get exactly the UAX #29
/// segments, byte-identical to before.
fn segments(s: &str) -> Vec<(usize, usize)> {
    if !has_sa(s) {
        return s
            .split_word_bound_indices()
            .map(|(b, w)| (b, b + w.len()))
            .collect();
    }
    let mut keep: std::collections::BTreeSet<usize> = [0, s.len()].into_iter().collect();
    let mut sa_breaks: Vec<usize> = Vec::new();
    with_sa_breaks(s, |sa| sa_breaks.extend(sa.iter().copied()));
    for &b in &sa_breaks {
        keep.insert(b);
    }
    // UAX #29 boundaries: keep a boundary unless both neighbors are SA characters and the
    // dictionary found no break there (an intra-run syllable artifact).
    for (b, w) in s.split_word_bound_indices() {
        let end = b + w.len();
        for point in [b, end] {
            let prev = s.get(..point).and_then(|t| t.chars().next_back());
            let next = s.get(point..).and_then(|t| t.chars().next());
            let inside_run = prev.is_some_and(is_sa_char) && next.is_some_and(is_sa_char);
            if !(inside_run && !sa_breaks.contains(&point)) {
                keep.insert(point);
            }
        }
    }
    let bounds: Vec<usize> = keep.into_iter().collect();
    bounds.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Word segments (UAX #29 word bounds that contain a letter or digit) as byte ranges; the visual
/// word moves of [`crate::text::CaretMap::visual_word_step`] use the same words (ADR-9340).
pub(crate) fn word_ranges(s: &str) -> Vec<(usize, usize)> {
    words(s)
}

/// Word segments (UAX #29 word bounds that contain a letter or digit) as byte ranges.
fn words(s: &str) -> Vec<(usize, usize)> {
    segments(s)
        .into_iter()
        .filter(|(a, b)| s[*a..*b].chars().any(char::is_alphanumeric))
        .collect()
}

/// The start of the word before `i` (Ctrl+Left; Option+Left on macOS keyboards).
pub fn prev_word(s: &str, i: usize) -> usize {
    words(s)
        .iter()
        .take_while(|(a, _)| *a < i)
        .last()
        .map_or(0, |(a, _)| *a)
}

/// The end of the word after `i` (Ctrl+Right).
pub fn next_word(s: &str, i: usize) -> usize {
    words(s).iter().find(|(_, b)| *b > i).map_or(s.len(), |(_, b)| *b)
}

/// The word-bound segment around `i` (double-click): a word, a run of spaces or one punctuation
/// mark. At the very end of the text it is the last segment.
pub fn segment_at(s: &str, i: usize) -> (usize, usize) {
    let mut last = (s.len(), s.len());
    for (a, b) in segments(s) {
        if a <= i && i < b {
            return (a, b);
        }
        last = (a, b);
    }
    last
}

/// The logical line (between `\n`s) around `i`: `(start, end)`, `end` excluding the break.
pub fn line_bounds(s: &str, i: usize) -> (usize, usize) {
    // An index inside a multi-byte character counts as that character's start.
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    let start = s[..i].rfind('\n').map_or(0, |p| p + 1);
    let end = s[i..].find('\n').map_or(s.len(), |p| i + p);
    (start, end)
}

/// Which edits a [`crate::text_input::TextInput`] accepts. A filter judges the whole text an edit
/// would produce; an edit it refuses is dropped entirely (a paste is never half applied). The
/// length limit is `TextInput::max_chars`.
#[derive(Debug, Clone, Copy, Default)]
pub enum InputFilter {
    /// Any text.
    #[default]
    Any,
    /// An optionally signed whole number while being typed: `""`, `"-"`, `"42"`, `"-7"`.
    Integer,
    /// An optionally signed decimal number while being typed: also `"."`, `"3."`, `"-0.5"`.
    Numeric,
    /// A custom rule over the whole candidate text (no regex engine needed).
    Custom(fn(&str) -> bool),
}

impl PartialEq for InputFilter {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Custom(a), Self::Custom(b)) => std::ptr::fn_addr_eq(*a, *b),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

impl InputFilter {
    /// Whether `candidate` (the full text after an edit) is allowed.
    pub fn accepts(&self, candidate: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Integer => {
                let digits = candidate.strip_prefix(['-', '+']).unwrap_or(candidate);
                digits.chars().all(|c| c.is_ascii_digit())
            }
            Self::Numeric => {
                let body = candidate.strip_prefix(['-', '+']).unwrap_or(candidate);
                body.chars().filter(|c| *c == '.').count() <= 1
                    && body.chars().all(|c| c.is_ascii_digit() || c == '.')
            }
            Self::Custom(rule) => rule(candidate),
        }
    }

    /// The filter's name in the agent protocol and `.ui.ron` docs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Integer => "integer",
            Self::Numeric => "numeric",
            Self::Custom(_) => "custom",
        }
    }
}

/// What kind of edit is being recorded, for undo coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    /// Typed text: consecutive typing joins one undo step until a word starts.
    Typing,
    /// Single Backspace/Delete presses: consecutive ones join one undo step.
    Deleting,
    /// Paste, cut, word deletion, replacing a selection: always its own step.
    Other,
}

/// Text plus caret and selection anchor (bytes), as undo restores them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct EditSnapshot {
    pub(crate) text: String,
    pub(crate) caret: usize,
    pub(crate) anchor: usize,
}

/// Undo/redo stacks. Each undo entry is the state before one step; typing runs coalesce into one
/// step that breaks when a new word starts, when the caret moves, or on any other kind of edit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct EditHistory {
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
    open: Option<EditKind>,
}

impl EditHistory {
    /// Most undo steps kept per input.
    pub(crate) const LIMIT: usize = 100;

    /// Records that an edit of `kind` is about to change `before`. `new_word` marks typing that
    /// starts a word boundary (whitespace typed after a non-space), which opens a new step.
    pub(crate) fn record(&mut self, kind: EditKind, before: EditSnapshot, new_word: bool) {
        self.redo.clear();
        let coalesce = match (self.open, kind) {
            (Some(EditKind::Typing), EditKind::Typing) => !new_word,
            (Some(EditKind::Deleting), EditKind::Deleting) => true,
            _ => false,
        };
        if !coalesce {
            self.undo.push(before);
            if self.undo.len() > Self::LIMIT {
                self.undo.remove(0);
            }
        }
        self.open = (kind != EditKind::Other).then_some(kind);
    }

    /// Ends the open typing or deleting run (the caret moved).
    pub(crate) fn break_run(&mut self) {
        self.open = None;
    }

    /// Steps back: returns the state to restore and remembers `current` for redo.
    pub(crate) fn undo(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        let previous = self.undo.pop()?;
        self.redo.push(current);
        self.open = None;
        Some(previous)
    }

    /// Steps forward again after an undo.
    pub(crate) fn redo(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        self.open = None;
        Some(next)
    }

    /// Steps available to undo.
    pub(crate) fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Steps available to redo.
    pub(crate) fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Forgets everything (the text was replaced from outside the widget).
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphemes_keep_combining_marks_and_emoji_together() {
        // "e" + COMBINING ACUTE ACCENT, then a family emoji (ZWJ sequence), then "x".
        let s = "e\u{301}👨\u{200d}👩\u{200d}👧x";
        let family_start = "e\u{301}".len();
        let x_start = s.len() - 1;
        assert_eq!(next_grapheme(s, 0), family_start, "accent stays with its base");
        assert_eq!(
            next_grapheme(s, family_start),
            x_start,
            "the ZWJ family is one step"
        );
        assert_eq!(prev_grapheme(s, x_start), family_start);
        assert_eq!(prev_grapheme(s, family_start), 0);
        assert_eq!(prev_grapheme(s, 0), 0);
        assert_eq!(next_grapheme(s, s.len()), s.len());
        assert_eq!(snap_grapheme(s, 1), 0, "inside the accent snaps to the base");
        assert_eq!(snap_grapheme(s, s.len()), s.len());
    }

    #[test]
    fn word_jumps_follow_word_bounds() {
        let s = "hello, big world";
        // (from, prev_word, next_word)
        let table = [
            (0, 0, 5),
            (5, 0, 10),
            (7, 0, 10),
            (8, 7, 10),
            (10, 7, 16),
            (16, 11, 16),
        ];
        for (from, prev, next) in table {
            assert_eq!(prev_word(s, from), prev, "prev_word from {from}");
            assert_eq!(next_word(s, from), next, "next_word from {from}");
        }
    }

    #[test]
    fn thai_word_movement_steps_over_dictionary_words() {
        // ฉัน / กิน / ข้าว — hand-checked fixture, ICU segmentation (ADR-3323). Thai writes words
        // without spaces, so UAX #29 alone sees one word; Ctrl+arrows and double-click must step
        // over the dictionary words.
        let s = "ฉันกินข้าว";
        let khao = "ฉันกิน".len();
        assert_eq!(prev_word(s, s.len()), khao, "Ctrl+Left from the end lands at ข้าว");
        assert_eq!(
            next_word(s, 0),
            "ฉัน".len(),
            "Ctrl+Right from the start lands after ฉัน"
        );
        assert_eq!(
            segment_at(s, khao + 1),
            (khao, s.len()),
            "double-click selects ข้าว"
        );
        // Mixed Latin + Thai: every script's words count.
        let mixed = "save ฉันกิน now";
        let gin = "save ฉัน".len();
        assert_eq!(prev_word(mixed, mixed.len()), "save ฉันกิน ".len());
        assert_eq!(next_word(mixed, 0), 4, "Latin words still move normally");
        assert_eq!(next_word(mixed, 4), gin, "into the Thai run lands after ฉัน");
        // Lao / Khmer / Myanmar (the same UAX #14 class SA).
        assert_eq!(next_word("ຂ້າພະເຈົ້າກິນ", 0), "ຂ້າພະເຈົ້າ".len(), "Lao");
        assert_eq!(next_word("ខ្ញុំញ៉ាំបាយ", 0), "ខ្ញុំ".len(), "Khmer");
        assert_eq!(next_word("ကျွန်တော်စားဆန်", 0), "ကျွန်တော်".len(), "Myanmar");
    }

    #[test]
    fn segments_and_lines() {
        let s = "one two\nthree";
        assert_eq!(segment_at(s, 1), (0, 3));
        assert_eq!(segment_at(s, 3), (3, 4), "the space between words");
        assert_eq!(
            segment_at(s, s.len()),
            (8, 13),
            "end of text selects the last word"
        );
        assert_eq!(line_bounds(s, 2), (0, 7));
        assert_eq!(line_bounds(s, 7), (0, 7), "before the break is still line one");
        assert_eq!(line_bounds(s, 8), (8, 13));
        assert_eq!(line_bounds("", 0), (0, 0));
    }

    #[test]
    fn filters_accept_partial_numbers_only() {
        let table: [(InputFilter, &str, bool); 14] = [
            (InputFilter::Integer, "", true),
            (InputFilter::Integer, "-", true),
            (InputFilter::Integer, "-42", true),
            (InputFilter::Integer, "4.2", false),
            (InputFilter::Integer, "4a", false),
            (InputFilter::Integer, "4-", false),
            (InputFilter::Numeric, ".", true),
            (InputFilter::Numeric, "-0.5", true),
            (InputFilter::Numeric, "3.", true),
            (InputFilter::Numeric, "1.2.3", false),
            (InputFilter::Numeric, "--1", false),
            (InputFilter::Numeric, "١", false),
            (InputFilter::Any, "anything", true),
            (InputFilter::Custom(|s| s.len() <= 3), "abcd", false),
        ];
        for (filter, text, ok) in table {
            assert_eq!(filter.accepts(text), ok, "{} on {text:?}", filter.name());
        }
        assert_eq!(InputFilter::Numeric, InputFilter::Numeric);
        assert_ne!(InputFilter::Numeric, InputFilter::Integer);
    }

    fn snap(text: &str) -> EditSnapshot {
        EditSnapshot {
            text: text.to_owned(),
            caret: text.len(),
            anchor: text.len(),
        }
    }

    #[test]
    fn history_coalesces_typing_per_word_and_redoes() {
        let mut h = EditHistory::default();
        // Type "hi yo" one character at a time: "hi" is one step, " yo" another.
        let typed = "hi yo";
        let mut text = String::new();
        for c in typed.chars() {
            let new_word = c.is_whitespace() && !text.ends_with(char::is_whitespace);
            h.record(EditKind::Typing, snap(&text), new_word);
            text.push(c);
        }
        assert_eq!(h.undo_len(), 2);
        let back = h.undo(snap(&text)).expect("undo 1");
        assert_eq!(back.text, "hi");
        let back = h.undo(back).expect("undo 2");
        assert_eq!(back.text, "");
        assert!(h.undo(back.clone()).is_none(), "nothing left");
        let forward = h.redo(back).expect("redo");
        assert_eq!(forward.text, "hi");
        assert_eq!(h.redo_len(), 1);
    }

    #[test]
    fn history_breaks_runs_on_caret_moves_and_kinds() {
        let mut h = EditHistory::default();
        h.record(EditKind::Typing, snap(""), false);
        h.record(EditKind::Typing, snap("a"), false);
        h.break_run();
        h.record(EditKind::Typing, snap("ab"), false);
        h.record(EditKind::Deleting, snap("abc"), false);
        h.record(EditKind::Deleting, snap("ab"), false);
        h.record(EditKind::Other, snap("a"), false);
        h.record(EditKind::Other, snap("a!"), false);
        // Steps: "" (typing a,b), "ab" (typing c), "abc" (two deletes), "a" (paste), "a!" (paste).
        assert_eq!(h.undo_len(), 5);
        // A new edit after undo drops the redo stack.
        let back = h.undo(snap("a!?")).expect("undo");
        h.record(EditKind::Other, back, false);
        assert_eq!(h.redo_len(), 0);
    }

    #[test]
    fn history_is_bounded() {
        let mut h = EditHistory::default();
        for i in 0..EditHistory::LIMIT + 10 {
            h.record(EditKind::Other, snap(&i.to_string()), false);
        }
        assert_eq!(h.undo_len(), EditHistory::LIMIT);
    }
}
