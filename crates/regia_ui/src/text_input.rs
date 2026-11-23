// SPDX-License-Identifier: MPL-2.0
//! The [`TextInput`] widget (`specs/18` § 1; ADR-273 gap W01 parts 1 and 2, ADR-336, ADR-7208): caret, selection,
//! clipboard, undo and pointer editing for single- and multi-line text fields.
//!
//! Keys (the focused input; Ctrl is also Cmd, word moves also take Alt/Option):
//!
//! | Keys | Effect |
//! |---|---|
//! | Left / Right | one caret stop left / right on screen (a grapheme cluster; see below); with a selection, collapse it to that side |
//! | Ctrl+Left / Ctrl+Right | the next word edge left / right on screen: in left-to-right text the start of the previous word / the end of the next word, in right-to-left text the end of the next / the start of the previous word |
//! | Up / Down (multi-line) | previous / next visual line, keeping the caret's x |
//! | Home / End | start / end of the visual line (whole text in a single-line field), drawn at the line's visual start / end edge (right / left in a right-to-left paragraph) |
//! | Ctrl+Home / Ctrl+End | start / end of the text |
//! | Shift + any move | extend the selection |
//! | Backspace / Delete | delete the selection or one grapheme cluster; Ctrl deletes a word |
//! | Ctrl+A, Ctrl+C, Ctrl+X, Ctrl+V | select all, copy, cut, paste (through [`UiClipboard`]) |
//! | Ctrl+Z, Ctrl+Shift+Z or Ctrl+Y | undo, redo (typing coalesces per word) |
//! | Enter | single-line: `Submitted`; multi-line: new line (Ctrl+Enter submits) |
//!
//! Pointer: a press places the caret (Shift extends the selection) and focuses the input, dragging
//! selects, a double click selects the word, a triple click the line (the whole text in a
//! single-line field). Carets and hit tests come from the shaped glyph clusters
//! ([`crate::text::CaretMap`]). A field with a definite width and height scrolls its content to
//! keep the caret visible (single-line horizontally; multi-line wraps and scrolls vertically).
//!
//! Text from IME commits is inserted like typing. The composition in progress
//! ([`TextInput::preedit`]) is drawn inline at the caret, underlined, with the caret inside it at
//! the IME's cursor ([`TextInput::preedit_cursor`]), and is not part of `text` until committed.
//! The focused field reports its caret (or composition) rect in window pixels
//! ([`TextInput::ime_rect`]) to [`regia_platform::ImeCursorArea`], which the platform layer hands
//! to the OS so the candidate window opens next to it.
//!
//! Left / Right move visually when the field has a layout: one caret stop left or right on
//! screen, so in mixed left-to-right / right-to-left text the logical offset may jump at a run
//! boundary ([`crate::text::CaretMap::visual_stops`]); at a line's edge they continue logically
//! to the neighboring line (ADR-273 W01 part 2, ADR-7208). Ctrl+Left / Ctrl+Right move to the
//! next word edge on screen ([`crate::text::CaretMap::visual_word_step`]), and Home / End draw
//! the caret at the line's visual start / end edge, by the paragraph's base level (UAX #9 rules
//! P2 and P3, [`crate::text::CaretLine::rtl`]), so a right-to-left line that opens with a number
//! starts at its right edge (ADR-9340). Deletion stays logical. Book page
//! `agents/ui-text-input.md`.

use std::hash::{DefaultHasher, Hash, Hasher};

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use unicode_segmentation::UnicodeSegmentation;

use crate::clipboard::UiClipboard;
use crate::color::UiColor;
use crate::focus::FocusState;
use crate::interaction::{UiEvent, UiEventKind, UiModifiers, UiPointer, UiValue};
use crate::layout::LayoutDirty;
use crate::localization::Localization;
use crate::node::{
    Background, BackgroundKind, ClipChildren, ComputedNode, Focused, NodeStyle, Opacity, UiNode,
};
use crate::style::{Dimension, Display, Edges, PositionType, Style};
use crate::text::{CaretMap, Text, TextSpan, TextStyle, TextSystem, WrapMode};
use crate::text_edit::{
    EditHistory, EditKind, EditSnapshot, InputFilter, line_bounds, next_grapheme, next_word, prev_grapheme,
    prev_word, segment_at, snap_grapheme,
};
use crate::theme::Themed;

/// Caret blink half-period in seconds of game time (on, then off).
const BLINK_HALF_PERIOD: f32 = 0.53;
/// Caret width in physical px.
const CARET_WIDTH: f32 = 2.0;

/// A single/multi-line text field with caret, selection, clipboard and undo (see the module docs).
///
/// Positions (`caret`, `selection_anchor`) are byte offsets into `text` on grapheme boundaries.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct TextInput {
    /// Current content.
    pub text: String,
    /// Placeholder shown when empty.
    pub placeholder: String,
    /// Multi-line (Enter inserts a newline, lines wrap) vs single-line (Enter submits).
    pub multiline: bool,
    /// Maximum characters (Unicode scalar values; 0 = unlimited).
    pub max_chars: usize,
    /// Mask characters (passwords); the mask character when set. Masked text is never copied.
    pub secret: Option<char>,
    /// Caret position in bytes.
    pub caret: usize,
    /// Selection anchor in bytes (equals `caret` when nothing is selected).
    pub selection_anchor: usize,
    /// The content can be selected and copied but not changed.
    pub read_only: bool,
    /// Which edits are accepted.
    pub filter: InputFilter,
    /// Content scroll inside the field (physical px, x then y), kept by the widget so the caret
    /// stays visible.
    pub scroll: [f32; 2],
    /// The IME composition in progress: not part of `text`, drawn underlined at the caret.
    pub preedit: String,
    /// The IME's cursor inside [`TextInput::preedit`] as a byte range (`None`: at its end). The
    /// caret is drawn at the range's end.
    pub preedit_cursor: Option<(usize, usize)>,
    /// Screen x (block space, physical px) of the caret after a visual move: a boundary offset
    /// between two runs has two screen positions and this picks the one the move reached.
    pub(crate) caret_hint: Option<f32>,
    /// The caret (or composition) rect in window physical px, `[x, y, w, h]`, while focused.
    pub(crate) ime_rect: Option<[f32; 4]>,
    /// The drawn caret in window physical px, `[x, top, height]`, while focused.
    pub(crate) visual_caret: Option<[f32; 3]>,
    /// Caret blink accumulator (seconds).
    pub(crate) blink: f32,
    pub(crate) history: EditHistory,
    /// Caret x kept across Up/Down presses (physical px).
    pub(crate) goal_x: Option<f32>,
    /// Hash of `text` when the widget last handled it (an outside change resets the history).
    pub(crate) seen: u64,
    /// A pointer press on the input is selecting.
    pub(crate) dragging: bool,
}

impl Default for TextInput {
    fn default() -> Self {
        Self {
            text: String::new(),
            placeholder: String::new(),
            multiline: false,
            max_chars: 0,
            secret: None,
            caret: 0,
            selection_anchor: 0,
            read_only: false,
            filter: InputFilter::Any,
            scroll: [0.0; 2],
            preedit: String::new(),
            preedit_cursor: None,
            caret_hint: None,
            ime_rect: None,
            visual_caret: None,
            blink: 0.0,
            history: EditHistory::default(),
            goal_x: None,
            seen: 0,
            dragging: false,
        }
    }
}

/// What one key press did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct KeyEffect {
    /// The text changed.
    pub(crate) edited: bool,
    /// The caret or selection moved.
    pub(crate) moved: bool,
    /// Enter submitted the field.
    pub(crate) submitted: bool,
}

impl KeyEffect {
    fn edited() -> Self {
        Self {
            edited: true,
            moved: true,
            submitted: false,
        }
    }

    fn moved() -> Self {
        Self {
            edited: false,
            moved: true,
            submitted: false,
        }
    }
}

impl TextInput {
    /// The selected byte range (ordered).
    pub fn selection(&self) -> (usize, usize) {
        (
            self.selection_anchor.min(self.caret),
            self.selection_anchor.max(self.caret),
        )
    }

    /// The selected text (empty when nothing is selected).
    pub fn selected_text(&self) -> &str {
        let (a, b) = self.selection();
        self.text.get(a..b).unwrap_or("")
    }

    /// The caret (or, while composing, the composition) rect in window physical px,
    /// `[x, y, w, h]`, as reported to the IME; `None` while the field is not focused or not laid
    /// out yet.
    pub fn ime_rect(&self) -> Option<[f32; 4]> {
        self.ime_rect
    }

    /// The drawn caret in window physical px, `[x, top, height]` (the visual position, which in
    /// bidirectional text is not always the logical offset's default position); `None` while
    /// the field is not focused or not laid out yet.
    pub fn visual_caret(&self) -> Option<[f32; 3]> {
        self.visual_caret
    }

    /// The composition as shown (masked when `secret`).
    fn preedit_display(&self) -> String {
        match self.secret {
            Some(c) => c.to_string().repeat(self.preedit.chars().count()),
            None => self.preedit.clone(),
        }
    }

    /// The label's text: the placeholder when there is nothing to show, else the shown text
    /// with the composition inserted at the caret. Returns `(before, preedit, after)`; `preedit`
    /// is empty when not composing (and then `after` is empty too).
    pub(crate) fn label_parts(&self) -> (String, String, String) {
        if self.preedit.is_empty() {
            let shown = if self.text.is_empty() {
                self.placeholder.clone()
            } else {
                self.display_text()
            };
            return (shown, String::new(), String::new());
        }
        let shown = self.display_text();
        let at = self.raw_display(self.caret).min(shown.len());
        let (before, after) = shown.split_at(at);
        (before.to_owned(), self.preedit_display(), after.to_owned())
    }

    /// Visible text (masked when `secret`).
    pub fn display_text(&self) -> String {
        match self.secret {
            Some(c) => c.to_string().repeat(self.text.chars().count()),
            None => self.text.clone(),
        }
    }

    /// Replaces the content from code: the caret goes to the end and the undo history is cleared.
    /// Line breaks become spaces in a single-line field; `max_chars` applies, the filter does not.
    pub fn set_text(&mut self, text: &str) {
        let mut clean = self.sanitize(text);
        if self.max_chars > 0 {
            clean = take_graphemes(&clean, self.max_chars).to_owned();
        }
        self.text = clean;
        self.caret = self.text.len();
        self.selection_anchor = self.caret;
        self.history.clear();
        self.goal_x = None;
        self.seen = text_hash(&self.text);
    }

    /// Replaces the content for an outside writer (the agent bridge's `ui.set_value` and the
    /// a11y set-value request): sanitized like typed text, cut to `max_chars` on grapheme
    /// boundaries, `None` — leaving the field untouched — for a read-only field or when the
    /// filter refuses the result, with the undo history cleared the way [`TextInput::set_text`]
    /// does so the user's typing cannot come back on the next undo.
    pub(crate) fn set_external_text(&mut self, text: &str) -> Option<String> {
        if self.read_only {
            return None;
        }
        let clean = self.external_text(text)?;
        self.text = clean;
        self.caret = self.text.len();
        self.selection_anchor = self.caret;
        self.history.clear();
        self.goal_x = None;
        self.seen = text_hash(&self.text);
        Some(self.text.clone())
    }

    /// The text an outside writer (a screen reader's set-value request) may put in the field, held
    /// to what typing it would allow: sanitized like typed text, cut to `max_chars` on grapheme
    /// boundaries, and `None` when the field's filter refuses the result (audit wave 5).
    pub(crate) fn external_text(&self, text: &str) -> Option<String> {
        let mut clean = self.sanitize(text);
        if self.max_chars > 0 {
            clean = take_graphemes(&clean, self.max_chars).to_owned();
        }
        self.filter.accepts(&clean).then_some(clean)
    }

    /// Selects `anchor..caret` (bytes, clamped and moved back to grapheme boundaries).
    pub fn select(&mut self, anchor: usize, caret: usize) {
        self.selection_anchor = snap_grapheme(&self.text, anchor);
        self.caret = snap_grapheme(&self.text, caret);
        self.goal_x = None;
        self.history.break_run();
    }

    /// Selects the whole text.
    pub fn select_all(&mut self) {
        self.select(0, self.text.len());
    }

    /// Whether an undo step is available.
    pub fn can_undo(&self) -> bool {
        self.history.undo_len() > 0
    }

    /// Whether a redo step is available.
    pub fn can_redo(&self) -> bool {
        self.history.redo_len() > 0
    }

    /// Undoes one step; returns whether anything changed. Does nothing when `read_only`.
    pub fn undo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        match self.history.undo(self.snapshot()) {
            Some(s) => {
                self.restore(s);
                true
            }
            None => false,
        }
    }

    /// Redoes one undone step; returns whether anything changed.
    pub fn redo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        match self.history.redo(self.snapshot()) {
            Some(s) => {
                self.restore(s);
                true
            }
            None => false,
        }
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            text: self.text.clone(),
            caret: self.caret,
            anchor: self.selection_anchor,
        }
    }

    fn restore(&mut self, s: EditSnapshot) {
        self.text = s.text;
        self.caret = s.caret.min(self.text.len());
        self.selection_anchor = s.anchor.min(self.text.len());
        self.goal_x = None;
    }

    /// Clamps the caret and anchor into the text on character boundaries (code may set them).
    pub(crate) fn clamp_positions(&mut self) {
        self.caret = char_floor(&self.text, self.caret);
        self.selection_anchor = char_floor(&self.text, self.selection_anchor);
    }

    /// Text entering the field: no carriage returns, no control characters, and in a single-line
    /// field no line breaks (they become spaces). U+2028/U+2029 (Zl/Zp, not control characters)
    /// sanitize exactly like `\r`/`\n`: left in, they would normalize to `\n` in the label
    /// pipeline and split a single-line field into two drawn paragraphs.
    fn sanitize(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' if chars.peek() == Some(&'\n') => {}
                '\r' | '\n' | '\u{2028}' | '\u{2029}' => out.push(if self.multiline { '\n' } else { ' ' }),
                '\t' => out.push(' '),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        out
    }

    /// Replaces the selection with `insert`, honoring `read_only`, `max_chars` and the filter.
    /// Returns whether the text changed.
    pub(crate) fn replace_selection(&mut self, insert: &str, kind: EditKind) -> bool {
        if self.read_only {
            return false;
        }
        self.clamp_positions();
        let mut insert = self.sanitize(insert);
        let (a, b) = self.selection();
        if self.max_chars > 0 {
            let kept = self.text.chars().count() - self.text[a..b].chars().count();
            let room = self.max_chars.saturating_sub(kept);
            // Whole grapheme clusters only, so a base letter never loses its marks.
            let fit = take_graphemes(&insert, room).len();
            insert.truncate(fit);
        }
        if insert.is_empty() && a == b {
            return false;
        }
        let mut candidate = String::with_capacity(self.text.len() + insert.len());
        candidate.push_str(&self.text[..a]);
        candidate.push_str(&insert);
        candidate.push_str(&self.text[b..]);
        if candidate == self.text || !self.filter.accepts(&candidate) {
            return false;
        }
        let new_word = kind == EditKind::Typing
            && insert.starts_with(char::is_whitespace)
            && !self.text[..a].ends_with(char::is_whitespace);
        // Typing over a selection is its own undo step.
        let kind = if kind == EditKind::Typing && a != b {
            EditKind::Other
        } else {
            kind
        };
        self.history.record(kind, self.snapshot(), new_word);
        self.text = candidate;
        self.caret = a + insert.len();
        self.selection_anchor = self.caret;
        self.goal_x = None;
        self.caret_hint = None;
        true
    }

    /// Deletes `a..b` (a selection or a computed range); the selection is kept if refused.
    fn delete_range(&mut self, a: usize, b: usize, kind: EditKind) -> bool {
        let before = (self.selection_anchor, self.caret);
        self.selection_anchor = a;
        self.caret = b;
        if self.replace_selection("", kind) {
            true
        } else {
            (self.selection_anchor, self.caret) = before;
            false
        }
    }

    /// Moves the caret; `extend` keeps the anchor (Shift).
    fn move_to(&mut self, pos: usize, extend: bool) {
        self.caret = pos.min(self.text.len());
        if !extend {
            self.selection_anchor = self.caret;
        }
        self.goal_x = None;
        self.caret_hint = None;
        self.history.break_run();
    }

    /// Byte offset in `text` to byte offset in the label's text: masked when `secret`, and
    /// offsets after the caret shifted past the composition while composing (the caret itself
    /// maps to the composition's start).
    pub(crate) fn to_display(&self, b: usize) -> usize {
        let raw = self.raw_display(b);
        if !self.preedit.is_empty() && b > self.caret {
            raw + self.preedit_display().len()
        } else {
            raw
        }
    }

    /// [`TextInput::to_display`] without the composition.
    fn raw_display(&self, b: usize) -> usize {
        match self.secret {
            None => b,
            Some(m) => self.text.get(..b).map_or(0, |s| s.chars().count()) * m.len_utf8(),
        }
    }

    /// The label byte the caret is drawn at: inside the composition (at the IME cursor's end,
    /// else the composition's end) while composing, else the caret's own offset.
    pub(crate) fn caret_display(&self) -> usize {
        let start = self.raw_display(self.caret);
        if self.preedit.is_empty() {
            return start;
        }
        let shown = self.preedit_display();
        let inner = match self.preedit_cursor {
            Some((_, end)) => {
                let end = char_floor(&self.preedit, end);
                match self.secret {
                    Some(m) => self.preedit[..end].chars().count() * m.len_utf8(),
                    None => end,
                }
            }
            None => shown.len(),
        };
        start + inner.min(shown.len())
    }

    /// Byte offset in the label's text back to `text` (an offset inside the composition maps to
    /// the caret).
    pub(crate) fn byte_from_display(&self, d: usize) -> usize {
        if !self.preedit.is_empty() {
            let start = self.raw_display(self.caret);
            let len = self.preedit_display().len();
            if d > start && d < start + len {
                return self.caret;
            }
            if d >= start + len {
                return self.raw_byte_from_display(d - len);
            }
        }
        self.raw_byte_from_display(d)
    }

    /// [`TextInput::byte_from_display`] without the composition.
    fn raw_byte_from_display(&self, d: usize) -> usize {
        match self.secret {
            None => char_floor(&self.text, d),
            Some(m) => {
                let index = d / m.len_utf8().max(1);
                self.text
                    .char_indices()
                    .nth(index)
                    .map_or(self.text.len(), |(i, _)| i)
            }
        }
    }

    /// The display-text caret map applies only while the label shows the text itself (not the
    /// placeholder).
    fn usable<'a>(&self, map: Option<&'a CaretMap>) -> Option<&'a CaretMap> {
        map.filter(|_| !self.text.is_empty() || !self.preedit.is_empty())
    }

    /// Where the caret is drawn ([`caret_block_at`]), copied out of the input so a caller can
    /// place the caret while it borrows the cached caret map.
    fn caret_spot(&self) -> CaretSpot {
        CaretSpot {
            at: self.to_display(self.caret),
            hint: self.caret_hint,
            composing_at: (!self.preedit.is_empty()).then(|| self.caret_display()),
        }
    }

    /// Left/Right with a layout: one visual stop (ADR-7208). At the line's edge the move goes on
    /// logically (forward when the step points the paragraph's way), reaching the next line.
    fn visual_move(&mut self, right: bool, extend: bool, map: &CaretMap) {
        let d = self.to_display(self.caret);
        let x = map.caret_near(d, self.caret_hint)[0];
        match map.visual_step(d, x, right) {
            Some((nx, nb)) => {
                self.move_to(self.byte_from_display(nb), extend);
                self.caret_hint = Some(nx);
            }
            None => {
                let forward = right != map.line_rtl(map.line_of(d));
                let to = if forward {
                    next_grapheme(&self.text, self.caret)
                } else {
                    prev_grapheme(&self.text, self.caret)
                };
                self.move_to(to, extend);
            }
        }
    }

    /// Ctrl+Left/Right with a layout: to the next word edge on screen (ADR-9340). At the line's
    /// edge the move goes on logically (forward when the step points the paragraph's way).
    fn visual_word_move(&mut self, right: bool, extend: bool, map: &CaretMap) {
        let d = self.to_display(self.caret);
        let x = map.caret_near(d, self.caret_hint)[0];
        match map.visual_word_step(d, x, right) {
            Some((nx, nb)) => {
                self.move_to(self.byte_from_display(nb), extend);
                self.caret_hint = Some(nx);
            }
            None => {
                let forward = right != map.line_rtl(map.line_of(d));
                let to = if forward {
                    next_word(&self.text, self.caret)
                } else {
                    prev_word(&self.text, self.caret)
                };
                self.move_to(to, extend);
            }
        }
    }

    /// After Home/End landed on `self.caret`: draw the caret at the line's visual start (`end`
    /// false) or end edge when that edge's stop is the caret's offset (ADR-9340).
    fn hint_line_edge(&mut self, end: bool, map: &CaretMap) {
        let d = self.to_display(self.caret);
        let line = map.line_of(d);
        let stops = map.visual_stops(line);
        let edge = if end != map.line_rtl(line) {
            stops.last()
        } else {
            stops.first()
        };
        if let Some(&(x, b)) = edge
            && b == d
        {
            self.caret_hint = Some(x);
        }
    }

    /// Without Shift, Left/Right collapse a selection to its visually left/right end.
    fn collapse_side(&self, a: usize, b: usize, right: bool, map: Option<&CaretMap>) -> usize {
        let Some(map) = self.usable(map) else {
            return if right { b } else { a };
        };
        let (xa, xb) = (map.caret(self.to_display(a))[0], map.caret(self.to_display(b))[0]);
        if (xb >= xa) == right { b } else { a }
    }

    /// Up/Down in a multi-line field: the same x on the previous/next visual line.
    fn vertical(&mut self, down: bool, extend: bool, map: Option<&CaretMap>) {
        let (to, goal) = match self.usable(map) {
            Some(map) => {
                let d = self.to_display(self.caret);
                let line = map.line_of(d);
                let goal = self.goal_x.unwrap_or_else(|| map.caret(d)[0]);
                let to = if !down && line == 0 {
                    0
                } else if down && line + 1 >= map.lines.len() {
                    self.text.len()
                } else {
                    let target = if down { line + 1 } else { line - 1 };
                    self.byte_from_display(map.hit_line(target, goal))
                };
                (to, Some(goal))
            }
            None => (self.vertical_logical(down), None),
        };
        self.move_to(to, extend);
        self.goal_x = goal;
    }

    /// Up/Down without a layout: same character column on the neighboring logical line.
    fn vertical_logical(&self, down: bool) -> usize {
        let (start, end) = line_bounds(&self.text, self.caret);
        let column = self.text[start..self.caret].chars().count();
        let target = if down {
            if end >= self.text.len() {
                return self.text.len();
            }
            line_bounds(&self.text, end + 1)
        } else {
            if start == 0 {
                return 0;
            }
            line_bounds(&self.text, start - 1)
        };
        let line = &self.text[target.0..target.1];
        target.0 + line.char_indices().nth(column).map_or(line.len(), |(i, _)| i)
    }

    /// Home/End: the visual line's ends (the logical line's without a layout).
    fn line_edge(&self, end: bool, map: Option<&CaretMap>) -> usize {
        if let Some(map) = self.usable(map)
            && let Some(line) = map.lines.get(map.line_of(self.to_display(self.caret)))
        {
            return self.byte_from_display(if end { line.end } else { line.start });
        }
        let (a, b) = line_bounds(&self.text, self.caret);
        if end { b } else { a }
    }

    fn copy(&self, clipboard: &mut UiClipboard) -> bool {
        let selected = self.selected_text();
        if self.secret.is_some() || selected.is_empty() {
            return false;
        }
        clipboard.set_text(selected);
        true
    }

    /// Applies one pressed key with `mods` held. `map` is the caret map of the displayed text
    /// (vertical moves and line edges use it; without one, logical lines are used).
    pub(crate) fn apply_key(
        &mut self,
        key: &str,
        mods: UiModifiers,
        clipboard: &mut UiClipboard,
        map: Option<&CaretMap>,
    ) -> KeyEffect {
        self.clamp_positions();
        let altgr = mods.ctrl && mods.alt;
        let primary = (mods.ctrl || mods.meta) && !altgr;
        let word = mods.ctrl || mods.alt;
        let shift = mods.shift;
        let (a, b) = self.selection();
        let has_selection = a != b;
        let lower = key.to_ascii_lowercase();
        match lower.as_str() {
            "arrowleft" | "left" | "arrowright" | "right" => {
                let right = lower.ends_with("right");
                // A masked field has no words on screen and a composition is not text yet: those
                // keep the logical word moves.
                let visual_words = self
                    .usable(map)
                    .filter(|_| self.secret.is_none() && self.preedit.is_empty());
                if word && let Some(map) = visual_words {
                    self.visual_word_move(right, shift, map);
                } else if word {
                    let to = if right {
                        next_word(&self.text, self.caret)
                    } else {
                        prev_word(&self.text, self.caret)
                    };
                    self.move_to(to, shift);
                } else if has_selection && !shift {
                    let to = self.collapse_side(a, b, right, map);
                    self.move_to(to, false);
                } else if let Some(map) = self.usable(map) {
                    self.visual_move(right, shift, map);
                } else {
                    let to = if right {
                        next_grapheme(&self.text, self.caret)
                    } else {
                        prev_grapheme(&self.text, self.caret)
                    };
                    self.move_to(to, shift);
                }
                KeyEffect::moved()
            }
            "arrowup" | "up" | "arrowdown" | "down" if self.multiline => {
                self.vertical(lower.ends_with("down"), shift, map);
                KeyEffect::moved()
            }
            "home" | "end" => {
                let end = lower == "end";
                let to = if primary || !self.multiline {
                    if end { self.text.len() } else { 0 }
                } else {
                    self.line_edge(end, map)
                };
                self.move_to(to, shift);
                if let Some(map) = self.usable(map) {
                    self.hint_line_edge(end, map);
                }
                KeyEffect::moved()
            }
            "backspace" | "delete" => {
                let forward = lower == "delete";
                let done = if has_selection {
                    self.delete_range(a, b, EditKind::Other)
                } else if forward {
                    let to = if word {
                        next_word(&self.text, self.caret)
                    } else {
                        next_grapheme(&self.text, self.caret)
                    };
                    let kind = if word { EditKind::Other } else { EditKind::Deleting };
                    self.delete_range(self.caret, to, kind)
                } else {
                    let from = if word {
                        prev_word(&self.text, self.caret)
                    } else {
                        prev_grapheme(&self.text, self.caret)
                    };
                    let kind = if word { EditKind::Other } else { EditKind::Deleting };
                    self.delete_range(from, self.caret, kind)
                };
                if done {
                    KeyEffect::edited()
                } else {
                    KeyEffect::default()
                }
            }
            "enter" | "numpadenter" | "return" => {
                if self.multiline && !primary {
                    if self.replace_selection("\n", EditKind::Typing) {
                        KeyEffect::edited()
                    } else {
                        KeyEffect::default()
                    }
                } else {
                    KeyEffect {
                        submitted: true,
                        ..KeyEffect::default()
                    }
                }
            }
            "copy" => {
                self.copy(clipboard);
                KeyEffect::default()
            }
            "cut" => self.cut(clipboard),
            "paste" => self.paste(clipboard),
            "undo" => self.undo_effect(),
            "redo" => self.redo_effect(),
            _ if primary => match lower.as_str() {
                "a" => {
                    self.select_all();
                    KeyEffect::moved()
                }
                "c" | "insert" => {
                    self.copy(clipboard);
                    KeyEffect::default()
                }
                "x" => self.cut(clipboard),
                "v" => self.paste(clipboard),
                "z" if shift => self.redo_effect(),
                "z" => self.undo_effect(),
                "y" => self.redo_effect(),
                _ => KeyEffect::default(),
            },
            "space" => self.type_text(" "),
            _ if key.graphemes(true).count() == 1 && !key.chars().any(char::is_control) => {
                self.type_text(key)
            }
            _ => KeyEffect::default(),
        }
    }

    /// Inserts typed (or IME-committed) text.
    pub(crate) fn type_text(&mut self, s: &str) -> KeyEffect {
        if self.replace_selection(s, EditKind::Typing) {
            KeyEffect::edited()
        } else {
            KeyEffect::default()
        }
    }

    fn cut(&mut self, clipboard: &mut UiClipboard) -> KeyEffect {
        if self.read_only || !self.copy(clipboard) {
            return KeyEffect::default();
        }
        let (a, b) = self.selection();
        if self.delete_range(a, b, EditKind::Other) {
            KeyEffect::edited()
        } else {
            KeyEffect::default()
        }
    }

    fn paste(&mut self, clipboard: &mut UiClipboard) -> KeyEffect {
        if self.read_only {
            return KeyEffect::default();
        }
        match clipboard.get_text() {
            Some(text) if self.replace_selection(&text, EditKind::Other) => KeyEffect::edited(),
            _ => KeyEffect::default(),
        }
    }

    fn undo_effect(&mut self) -> KeyEffect {
        if self.undo() {
            KeyEffect::edited()
        } else {
            KeyEffect::default()
        }
    }

    fn redo_effect(&mut self) -> KeyEffect {
        if self.redo() {
            KeyEffect::edited()
        } else {
            KeyEffect::default()
        }
    }

    /// A pointer press or drag at `p` (physical px relative to the text block): places the caret,
    /// keeping the anchor when `extend`.
    pub(crate) fn pointer_at(&mut self, p: [f32; 2], extend: bool, map: Option<&CaretMap>) -> bool {
        let to = self.usable(map).map_or(0, |m| self.byte_from_display(m.hit(p)));
        let anchor = if extend { self.selection_anchor } else { to };
        if (anchor, to) == (self.selection_anchor, self.caret) {
            return false;
        }
        self.move_to(to, extend);
        true
    }

    /// Double click: the word (or space run, or punctuation mark) at the caret. A masked field
    /// selects everything.
    pub(crate) fn select_word_at_caret(&mut self) {
        if self.secret.is_some() {
            self.select_all();
            return;
        }
        let (a, b) = segment_at(&self.text, self.caret);
        self.select(a, b);
    }

    /// Triple click: the logical line at the caret (the whole text in a single-line field).
    pub(crate) fn select_line_at_caret(&mut self) {
        if self.multiline {
            let (a, b) = line_bounds(&self.text, self.caret);
            self.select(a, b);
        } else {
            self.select_all();
        }
    }
}

/// The longest prefix of `s` made of whole grapheme clusters with at most `max_chars` scalars.
fn take_graphemes(s: &str, max_chars: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for g in s.graphemes(true) {
        let n = g.chars().count();
        if used + n > max_chars {
            break;
        }
        used += n;
        end += g.len();
    }
    &s[..end]
}

/// `i` clamped into `s` and moved back to a character boundary.
fn char_floor(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn text_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Marks the nodes a [`TextInput`] spawns under itself: the label showing the text, the caret and
/// the selection highlight rectangles (one per selected visual line).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputPart {
    /// The text (or placeholder) label.
    Label,
    /// The caret bar.
    Caret,
    /// A selection highlight rectangle.
    Selection,
}

/// The text inputs' view of the keyboard: modifiers tracked in key-event order (so a chord sent
/// within one frame, as `ui.key` does, applies its modifiers to the right keys) and the message
/// cursors. Cursors, not this frame's buffer, so keys the platform writes between frames are seen.
#[derive(Resource, Debug, Default)]
pub struct TextInputKeys {
    /// Modifiers held after the last key event processed.
    pub modifiers: UiModifiers,
    keys: MessageCursor<regia_platform::KeyboardInput>,
    ime: MessageCursor<regia_platform::ImeEvent>,
    events: MessageCursor<UiEvent>,
}

/// The cached caret map of an input's label.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct TextInputLayout {
    digest: u64,
    map: CaretMap,
}

/// Where an input's text block sits this frame (physical px).
#[derive(Debug, Clone, Copy)]
struct Placement {
    /// The padding box's top-left (inside the layout border).
    inner: [f32; 2],
    /// The content box: x, y, width, height.
    content: [f32; 4],
    /// UI scale.
    scale: f32,
    /// The label is absolutely placed and scrolls; otherwise it stays in flow.
    scrolls: bool,
}

/// The parts of a [`TextInput`] its layout depends on.
#[derive(Debug, Clone, Copy)]
struct View {
    multiline: bool,
    scroll: [f32; 2],
}

impl View {
    fn of(input: &TextInput) -> Self {
        Self {
            multiline: input.multiline,
            scroll: input.scroll,
        }
    }
}

fn dim_px(d: Dimension, scale: f32) -> f32 {
    match d {
        Dimension::Points(v) => v * scale,
        _ => 0.0,
    }
}

fn definite(d: Dimension) -> bool {
    !matches!(d, Dimension::Auto)
}

fn placement(world: &World, entity: Entity) -> Option<Placement> {
    let computed = world.get::<ComputedNode>(entity)?;
    let fallback = Style::default();
    let style = world.get::<NodeStyle>(entity).map_or(&fallback, |s| &s.0);
    let scale = computed.scale.max(0.001);
    let rect = computed.rect;
    let inner = [
        rect[0] + dim_px(style.border.left, scale),
        rect[1] + dim_px(style.border.top, scale),
    ];
    let pad = [
        dim_px(style.padding.left, scale),
        dim_px(style.padding.top, scale),
        dim_px(style.padding.right, scale),
        dim_px(style.padding.bottom, scale),
    ];
    let border_x = dim_px(style.border.left, scale) + dim_px(style.border.right, scale);
    let border_y = dim_px(style.border.top, scale) + dim_px(style.border.bottom, scale);
    let content = [
        inner[0] + pad[0],
        inner[1] + pad[1],
        (rect[2] - border_x - pad[0] - pad[2]).max(0.0),
        (rect[3] - border_y - pad[1] - pad[3]).max(0.0),
    ];
    let scrolls = (definite(style.size[0]) || definite(style.min_size[0]))
        && (definite(style.size[1]) || definite(style.min_size[1]));
    Some(Placement {
        inner,
        content,
        scale,
        scrolls,
    })
}

/// The input's label: the first child with a [`Text`].
fn label_of(world: &World, entity: Entity) -> Option<Entity> {
    world
        .get::<Children>(entity)?
        .iter()
        .find(|child| world.get::<Text>(*child).is_some())
}

fn parts_of(world: &World, entity: Entity, part: TextInputPart) -> Vec<Entity> {
    world
        .get::<Children>(entity)
        .map(|children| {
            children
                .iter()
                .filter(|child| world.get::<TextInputPart>(*child) == Some(&part))
                .collect()
        })
        .unwrap_or_default()
}

fn has_part(world: &World, entity: Entity, part: TextInputPart) -> bool {
    world.get::<Children>(entity).is_some_and(|children| {
        children
            .iter()
            .any(|child| world.get::<TextInputPart>(child) == Some(&part))
    })
}

/// Last label sync. An idle field skips `sync_parts` while this still matches.
#[derive(Component, Clone, Copy)]
struct TextInputStamp {
    digest: u64,
    /// Change tick of the label [`Text`] after that pass.
    label_tick: u32,
    /// The label wrap that pass chose. It depends on the field's size, not its text: a field
    /// synced before layout gave it a `ComputedNode`, or one whose size turned definite or auto,
    /// must sync again.
    wrap: WrapMode,
}

fn input_digest(input: &TextInput) -> u64 {
    let mut hasher = DefaultHasher::new();
    input.text.hash(&mut hasher);
    input.placeholder.hash(&mut hasher);
    input.secret.hash(&mut hasher);
    input.multiline.hash(&mut hasher);
    // A composition is part of the label (ADR-7208), split at the caret while it is shown.
    input.preedit.hash(&mut hasher);
    if !input.preedit.is_empty() {
        input.caret.hash(&mut hasher);
    }
    hasher.finish()
}

fn label_tick(world: &World, label: Entity) -> u32 {
    world
        .entity(label)
        .get_change_ticks::<Text>()
        .map(|ticks| ticks.changed.get())
        .unwrap_or(0)
}

/// On an input's label while it shows a composition: the label's own style to restore after the
/// commit.
#[derive(Component, Debug, Clone, PartialEq)]
pub(crate) struct PreeditSpan {
    base: TextStyle,
}

/// The label spans for `parts` ([`TextInput::label_parts`]) in `base` style: one span, or the
/// text before the caret, the underlined composition and the text after it (empty pieces left
/// out). Returns the spans and whether a composition is shown.
fn label_spans(parts: &(String, String, String), base: &TextStyle) -> (Vec<TextSpan>, bool) {
    let (before, preedit, after) = parts;
    if preedit.is_empty() {
        return (
            vec![TextSpan::lit(before.clone()).with_style(base.clone())],
            false,
        );
    }
    let mut spans = Vec::with_capacity(3);
    if !before.is_empty() {
        spans.push(TextSpan::lit(before.clone()).with_style(base.clone()));
    }
    let underlined = TextStyle {
        underline: true,
        ..base.clone()
    };
    spans.push(TextSpan::lit(preedit.clone()).with_style(underlined));
    if !after.is_empty() {
        spans.push(TextSpan::lit(after.clone()).with_style(base.clone()));
    }
    (spans, true)
}

/// Writes `parts` into the label `label` (only when they differ), keeping the label's style and
/// tracking the composition with [`PreeditSpan`].
fn write_label(world: &mut World, label: Entity, parts: &(String, String, String)) {
    let marker = world.get::<PreeditSpan>(label).cloned();
    let Some(text) = world.get::<Text>(label) else {
        return;
    };
    // The label's own style: the style saved when the composition started (correct even when the
    // composition is the label's only span or sits at index 0); else the first span's style.
    let base = marker
        .as_ref()
        .map(|m| m.base.clone())
        .or_else(|| text.spans.first().map(|s| s.style.clone()))
        .unwrap_or_default();
    let (spans, composing) = label_spans(parts, &base);
    if text.spans != spans
        && let Some(mut text) = world.get_mut::<Text>(label)
    {
        text.spans = spans;
    }
    match (composing, marker) {
        (true, Some(m)) if m.base == base => {}
        (true, _) => {
            world.entity_mut(label).insert(PreeditSpan { base });
        }
        (false, Some(_)) => {
            world.entity_mut(label).remove::<PreeditSpan>();
        }
        (false, None) => {}
    }
}

/// Ensures each input has a label (placeholder or content) and a caret node.
fn sync_parts(world: &mut World) {
    let inputs: Vec<Entity> = world
        .query_filtered::<Entity, With<TextInput>>()
        .iter(world)
        .collect();
    for entity in inputs {
        let Some((digest, multiline)) = world
            .get::<TextInput>(entity)
            .map(|input| (input_digest(input), input.multiline))
        else {
            continue;
        };
        // Multi-line fields wrap when they scroll (definite size); otherwise lines break only at
        // line breaks, as before.
        let scrolls = placement(world, entity).is_some_and(|p| p.scrolls);
        let wrap = if multiline && scrolls {
            WrapMode::Word
        } else {
            WrapMode::None
        };
        let label = label_of(world, entity);
        let tick = label.map(|id| label_tick(world, id)).unwrap_or(0);
        if label.is_some()
            && has_part(world, entity, TextInputPart::Caret)
            && world
                .get::<TextInputStamp>(entity)
                .is_some_and(|stamp| stamp.digest == digest && stamp.label_tick == tick && stamp.wrap == wrap)
        {
            continue;
        }
        if let Some(mut counts) = world.get_resource_mut::<crate::layout::UiHotPathCounts>() {
            counts.text_input_syncs += 1;
        }
        let Some(parts) = world.get::<TextInput>(entity).map(TextInput::label_parts) else {
            continue;
        };
        match label_of(world, entity) {
            None => {
                let child = world
                    .spawn((
                        UiNode,
                        TextInputPart::Label,
                        Text {
                            spans: Vec::new(),
                            wrap,
                            ..Default::default()
                        },
                    ))
                    .id();
                write_label(world, child, &parts);
                world.entity_mut(entity).add_child(child);
                world.resource_mut::<LayoutDirty>().mark();
            }
            Some(label) => {
                write_label(world, label, &parts);
                if let Some(mut text) = world.get_mut::<Text>(label)
                    && text.wrap != wrap
                {
                    text.wrap = wrap;
                }
            }
        }
        if parts_of(world, entity, TextInputPart::Caret).is_empty() {
            let caret = world
                .spawn((
                    UiNode,
                    TextInputPart::Caret,
                    NodeStyle::new(hidden_style()),
                    Background::default(),
                    Opacity(1.0),
                ))
                .id();
            world.entity_mut(entity).add_child(caret);
        }
        let label_tick = label_of(world, entity)
            .map(|id| label_tick(world, id))
            .unwrap_or(0);
        world.entity_mut(entity).insert(TextInputStamp {
            digest,
            label_tick,
            wrap,
        });
    }
}

fn hidden_style() -> Style {
    Style {
        display: Display::None,
        position: PositionType::Absolute,
        ..Default::default()
    }
}

/// An absolutely placed rectangle at `r` (physical px relative to the padding box).
fn rect_style(r: [f32; 4], scale: f32) -> Style {
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(r[0] / scale),
            top: Dimension::Points(r[1] / scale),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        },
        size: [Dimension::Points(r[2] / scale), Dimension::Points(r[3] / scale)],
        ..Default::default()
    }
}

/// Writes `style` to `entity` only when it differs (no spurious layout passes).
fn set_style(world: &mut World, entity: Entity, style: Style) {
    match world.get_mut::<NodeStyle>(entity) {
        Some(mut current) => {
            if current.0 != style {
                current.0 = style;
            }
        }
        None => {
            world.entity_mut(entity).insert(NodeStyle::new(style));
        }
    }
}

fn set_background(world: &mut World, entity: Entity, color: UiColor) {
    let kind = BackgroundKind::Color(color);
    match world.get_mut::<Background>(entity) {
        Some(mut bg) => {
            if bg.kind != kind {
                bg.kind = kind;
            }
        }
        None => {
            world.entity_mut(entity).insert(Background { kind });
        }
    }
}

/// Digest of the label as it will be shaped, and the width passed to the shaper.
fn caret_key(world: &World, label: Entity, place: &Placement) -> Option<(u64, Option<f32>)> {
    let text = world.get::<Text>(label)?;
    // The width the label is shaped at, as `text::shape_text` picks it (the laid-out content
    // width): right-to-left, centered and end-aligned lines are placed inside it. A scrolling
    // multi-line label has the content width before its first layout.
    let laid_out = world.get::<ComputedNode>(label).map(|computed| {
        let [pad_l, pad_r] = crate::text::pad_x_px(world.get::<NodeStyle>(label), computed.scale);
        computed.rect[2] - pad_l - pad_r
    });
    let max_w = laid_out
        .filter(|w| *w > 1.0)
        .or_else(|| (place.scrolls && text.wrap != WrapMode::None).then_some(place.content[2]));
    let generation = world
        .get_resource::<Localization>()
        .map_or(0, |loc| loc.generation.0);
    let min_px = world
        .get_resource::<TextSystem>()
        .map_or(0.0, |fonts| fonts.min_font_px);
    let mut hasher = DefaultHasher::new();
    crate::text::text_digest(text, generation).hash(&mut hasher);
    place.scale.to_bits().hash(&mut hasher);
    max_w.map(f32::to_bits).hash(&mut hasher);
    min_px.to_bits().hash(&mut hasher);
    Some((hasher.finish(), max_w))
}

/// The caret position inputs of [`caret_block_at`].
struct CaretSpot {
    /// The caret in display bytes.
    at: usize,
    /// The visual-move x the caret keeps (ADR-7208).
    hint: Option<f32>,
    /// The caret inside the composition while composing.
    composing_at: Option<usize>,
}

/// The caret as drawn in block space, `[x, top, height]`: inside the composition while
/// composing, at the visual-move position when one applies, else the logical default.
fn caret_block_at(map: &CaretMap, spot: &CaretSpot) -> [f32; 3] {
    match spot.composing_at {
        None => map.caret_near(spot.at, spot.hint),
        Some(at) => map.caret(at),
    }
}

/// Runs `f` on the cached caret map. A digest hit borrows the map. A miss shapes once, stores
/// the map, and does not clone it for the caller.
fn with_caret_map<R>(
    world: &mut World,
    entity: Entity,
    place: &Placement,
    f: impl FnOnce(&CaretMap) -> R,
) -> Option<R> {
    let label = label_of(world, entity)?;
    let (digest, max_w) = caret_key(world, label, place)?;
    if let Some(cached) = world.get::<TextInputLayout>(entity)
        && cached.digest == digest
    {
        return Some(f(&cached.map));
    }
    let text = world.get::<Text>(label)?.clone();
    if !world.contains_resource::<TextSystem>() {
        return None;
    }
    let map = world.resource_scope(|world, mut fonts: Mut<TextSystem>| {
        match world.get_resource::<Localization>() {
            Some(loc) => fonts.caret_map(&text, place.scale, max_w, loc),
            None => fonts.caret_map(&text, place.scale, max_w, &Localization::default()),
        }
    });
    let result = f(&map);
    if let Some(mut counts) = world.get_resource_mut::<crate::layout::UiHotPathCounts>() {
        counts.text_input_layout_writes += 1;
    }
    world.entity_mut(entity).insert(TextInputLayout { digest, map });
    Some(result)
}

/// Owned caret map for a frame that edits. A cache hit clones the map and counts that clone.
/// Idle frames do not call this.
fn owned_caret_map(world: &mut World, entity: Entity, place: &Placement) -> Option<CaretMap> {
    let label = label_of(world, entity)?;
    let (digest, _) = caret_key(world, label, place)?;
    if world
        .get::<TextInputLayout>(entity)
        .is_some_and(|cached| cached.digest == digest)
    {
        if let Some(mut counts) = world.get_resource_mut::<crate::layout::UiHotPathCounts>() {
            counts.caret_map_clones += 1;
        }
        return world
            .get::<TextInputLayout>(entity)
            .map(|cached| cached.map.clone());
    }
    with_caret_map(world, entity, place, CaretMap::clone)
}

/// The text block's top-left (physical px) for the current scroll, matching extract's placement.
fn block_origin(world: &World, entity: Entity, view: View, place: &Placement, map_h: f32) -> [f32; 2] {
    if place.scrolls {
        let x = place.content[0] - view.scroll[0];
        let y = if view.multiline {
            place.content[1] - view.scroll[1]
        } else {
            place.content[1] + ((place.content[3] - map_h) * 0.5).max(0.0)
        };
        return [x.round(), y.round()];
    }
    match label_of(world, entity).and_then(|l| world.get::<ComputedNode>(l)) {
        Some(label) => [
            label.rect[0].round(),
            (label.rect[1] + ((label.rect[3] - map_h) * 0.5).max(0.0)).round(),
        ],
        None => [place.content[0], place.content[1]],
    }
}

/// The scroll that keeps the caret rect (`[x, top, height]`, block space) inside the view.
fn keep_caret_visible(field: View, caret: [f32; 3], place: &Placement, map: &CaretMap) -> [f32; 2] {
    let view = [place.content[2], place.content[3]];
    let mut scroll = field.scroll;
    let s = &mut scroll;
    if field.multiline {
        s[0] = 0.0;
        if caret[1] + caret[2] - s[1] > view[1] {
            s[1] = caret[1] + caret[2] - view[1];
        }
        if caret[1] - s[1] < 0.0 {
            s[1] = caret[1];
        }
        s[1] = s[1].clamp(0.0, (map.height() - view[1]).max(0.0));
    } else {
        s[1] = 0.0;
        if caret[0] + CARET_WIDTH - s[0] > view[0] {
            s[0] = caret[0] + CARET_WIDTH - view[0];
        }
        if caret[0] - s[0] < 0.0 {
            s[0] = caret[0];
        }
        s[0] = s[0].clamp(0.0, (map.width() + CARET_WIDTH - view[0]).max(0.0));
    }
    scroll
}

/// Selection highlight rectangles in block space, one per visual line.
fn selection_rects(map: &CaretMap, a: usize, b: usize) -> Vec<[f32; 4]> {
    let mut rects = Vec::new();
    if a == b {
        return rects;
    }
    for line in &map.lines {
        let mut x0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        for c in &line.clusters {
            if c.start < b && a < c.end {
                x0 = x0.min(c.x0);
                x1 = x1.max(c.x1);
            }
        }
        // A selected line break shows as a short block after the line.
        let breaks = line.end >= a && line.end < b && map.text.as_bytes().get(line.end) == Some(&b'\n');
        if breaks {
            let end = if x1.is_finite() { x1 } else { line.origin_x };
            x0 = x0.min(end);
            x1 = end + line.height * 0.3;
        }
        if x1 > x0 {
            rects.push([x0, line.top, x1 - x0, line.height]);
        }
    }
    rects
}

/// Moves keyboard focus to `next` from exclusive code, the way `focus::set_focus` does: the
/// `Focused` marker moves, `FocusChanged` events are sent and [`FocusState`] is updated. A press on
/// a text field and `ui.key { node }` use it.
pub fn focus_node(world: &mut World, next: Entity) {
    let Some(prev) = world.get_resource::<FocusState>().map(|f| f.focused) else {
        return;
    };
    if prev == Some(next) {
        return;
    }
    if let Some(p) = prev
        && let Ok(mut e) = world.get_entity_mut(p)
    {
        e.remove::<Focused>();
    }
    if let Ok(mut e) = world.get_entity_mut(next) {
        e.insert(Focused);
    }
    if let Some(p) = prev {
        world.write_message(UiEvent::new(p, UiEventKind::FocusChanged(false)));
    }
    world.write_message(UiEvent::new(next, UiEventKind::FocusChanged(true)));
    world.resource_mut::<FocusState>().focused = Some(next);
}

/// Runs `f` with the input taken out of the world, writing it back only if it changed.
fn with_input<R>(
    world: &mut World,
    entity: Entity,
    f: impl FnOnce(&mut World, &mut TextInput) -> R,
) -> Option<R> {
    let original = world.get::<TextInput>(entity)?.clone();
    let mut input = original.clone();
    let out = f(world, &mut input);
    if input != original
        && let Some(mut slot) = world.get_mut::<TextInput>(entity)
    {
        *slot = input;
    }
    Some(out)
}

/// `Update` (inside `widgets::ui_widgets`): text input editing, pointer selection, IME commits and
/// the caret, selection and scroll visuals.
pub fn ui_text_inputs(world: &mut World) {
    let _span = tracing::trace_span!("ui_text_inputs").entered();
    if !world.contains_resource::<UiClipboard>() {
        world.init_resource::<UiClipboard>();
    }
    if !world.contains_resource::<TextInputKeys>() {
        world.init_resource::<TextInputKeys>();
    }
    sync_parts(world);
    let entities: Vec<Entity> = world
        .query_filtered::<Entity, With<TextInput>>()
        .iter(world)
        .collect();
    // Text replaced from outside the widget (code, binding, agent) drops the undo history.
    for &entity in &entities {
        if let Some(mut input) = world.get_mut::<TextInput>(entity) {
            let hash = text_hash(&input.text);
            if hash != input.seen {
                input.history.clear();
                input.clamp_positions();
                input.seen = hash;
            }
        }
    }
    let (keys, ime, clicks) = world.resource_scope(|world, mut state: Mut<TextInputKeys>| {
        let state = &mut *state;
        let keys: Vec<regia_platform::KeyboardInput> = world
            .get_resource::<Messages<regia_platform::KeyboardInput>>()
            .map(|m| state.keys.read(m).cloned().collect())
            .unwrap_or_default();
        let ime: Vec<regia_platform::ImeEvent> = world
            .get_resource::<Messages<regia_platform::ImeEvent>>()
            .map(|m| state.ime.read(m).cloned().collect())
            .unwrap_or_default();
        let clicks: Vec<(Entity, u8)> = world
            .get_resource::<Messages<UiEvent>>()
            .map(|m| {
                state
                    .events
                    .read(m)
                    .filter(|e| e.kind == UiEventKind::Click)
                    .map(|e| (e.target, e.click_count))
                    .collect()
            })
            .unwrap_or_default();
        (keys, ime, clicks)
    });
    let dt = world.get_resource::<Time>().map_or(0.0, |t| t.delta_secs());

    let mut submitted: Vec<(Entity, String)> = Vec::new();
    let mut changed: Vec<(Entity, String)> = Vec::new();

    // Pointer: press places the caret (and focuses), drag extends, multi-clicks select.
    let (pressed, cursor, shift_held) = {
        let pointer = world.resource::<UiPointer>();
        (pointer.pressed, pointer.position(), pointer.modifiers.shift)
    };
    for &entity in &entities {
        let is_pressed = pressed == Some(entity) && !crate::node::is_disabled(world, entity);
        let was_dragging = world.get::<TextInput>(entity).is_some_and(|t| t.dragging);
        if !is_pressed {
            if was_dragging && let Some(mut input) = world.get_mut::<TextInput>(entity) {
                input.dragging = false;
            }
            continue;
        }
        if !was_dragging {
            focus_node(world, entity);
        }
        let (Some(place), Some(p)) = (placement(world, entity), cursor) else {
            continue;
        };
        let map = owned_caret_map(world, entity, &place);
        with_input(world, entity, |world, input| {
            let origin = block_origin(
                world,
                entity,
                View::of(input),
                &place,
                map.as_ref().map_or(0.0, CaretMap::height),
            );
            let local = [p[0] - origin[0], p[1] - origin[1]];
            let extend = was_dragging || shift_held;
            if input.pointer_at(local, extend, map.as_ref()) {
                input.blink = 0.0;
            }
            input.dragging = true;
        });
    }
    for (entity, count) in clicks {
        if count < 2 {
            continue;
        }
        with_input(world, entity, |_, input| {
            if count == 2 {
                input.select_word_at_caret();
            } else {
                input.select_line_at_caret();
            }
        });
    }

    // Keys and IME go to the focused, enabled input.
    let focused = world
        .resource::<FocusState>()
        .focused
        .filter(|e| world.get::<TextInput>(*e).is_some() && !crate::node::is_disabled(world, *e));
    let mut mods = world.resource::<TextInputKeys>().modifiers;
    if let Some(entity) = focused {
        let place = placement(world, entity);
        // An idle focused field has nothing to edit. Building the map here cloned it every frame.
        let mut map = if keys.is_empty() && ime.is_empty() {
            None
        } else {
            place.and_then(|region| owned_caret_map(world, entity, &region))
        };
        world.resource_scope(|world, mut clipboard: Mut<UiClipboard>| {
            with_input(world, entity, |world, input| {
                let mut edited = false;
                for e in &keys {
                    let down = e.state == regia_platform::ButtonState::Pressed;
                    if mods.apply_key(&e.logical_key, down) || !down {
                        continue;
                    }
                    if e.repeat && matches!(e.logical_key.as_str(), "Enter" | "NumpadEnter") {
                        continue;
                    }
                    let effect = input.apply_key(&e.logical_key, mods, &mut clipboard, map.as_ref());
                    if effect.submitted {
                        submitted.push((entity, input.text.clone()));
                    }
                    if effect.edited {
                        edited = true;
                        // Later keys this frame need the new layout (Up/Down, Home/End).
                        if let Some(p) = place {
                            sync_label_text(world, entity, input);
                            map = owned_caret_map(world, entity, &p);
                        }
                    }
                    if effect.moved || effect.edited {
                        input.blink = 0.0;
                    }
                }
                let composing = (input.preedit.clone(), input.preedit_cursor);
                for e in &ime {
                    match e {
                        regia_platform::ImeEvent::Commit(_, s) => {
                            input.preedit.clear();
                            input.preedit_cursor = None;
                            if input.type_text(s).edited {
                                edited = true;
                                input.blink = 0.0;
                            }
                        }
                        regia_platform::ImeEvent::Preedit(_, s) => {
                            input.preedit.clone_from(s);
                            input.preedit_cursor = None;
                            input.blink = 0.0;
                        }
                        regia_platform::ImeEvent::PreeditCursor(_, cursor) => {
                            input.preedit_cursor = *cursor;
                        }
                        regia_platform::ImeEvent::Disabled(_) => {
                            input.preedit.clear();
                            input.preedit_cursor = None;
                        }
                        regia_platform::ImeEvent::Enabled(_) => {}
                    }
                }
                if (input.preedit.clone(), input.preedit_cursor) != composing && !edited {
                    // The label shows the composition this frame (the caret map follows it).
                    sync_label_text(world, entity, input);
                    world.resource_mut::<LayoutDirty>().mark();
                }
                if edited {
                    input.seen = text_hash(&input.text);
                    changed.push((entity, input.text.clone()));
                }
            });
        });
    } else {
        for e in &keys {
            mods.apply_key(&e.logical_key, e.state == regia_platform::ButtonState::Pressed);
        }
    }
    world.resource_mut::<TextInputKeys>().modifiers = mods;

    if !changed.is_empty() {
        world.resource_mut::<LayoutDirty>().mark();
    }
    for (entity, text) in changed {
        sync_label(world, entity);
        world.write_message(UiEvent::new(
            entity,
            UiEventKind::ValueChanged(UiValue::Text(text)),
        ));
    }
    for (entity, text) in submitted {
        world.write_message(UiEvent::new(entity, UiEventKind::Submitted(text)));
    }
    for entity in entities {
        update_visuals(world, entity, focused == Some(entity), dt);
    }
    // The focused field's caret rect goes to the platform layer for the IME candidate window.
    let rect = focused
        .and_then(|e| world.get::<TextInput>(e))
        .and_then(TextInput::ime_rect);
    match world.get_resource_mut::<regia_platform::ImeCursorArea>() {
        Some(mut area) => {
            if area.rect != rect {
                area.rect = rect;
            }
        }
        None => world.insert_resource(regia_platform::ImeCursorArea { window: None, rect }),
    }
}

/// Copies an input's shown text (and composition) into its label now (so this frame's caret map
/// sees it).
fn sync_label_text(world: &mut World, entity: Entity, input: &TextInput) {
    if let Some(label) = label_of(world, entity) {
        write_label(world, label, &input.label_parts());
    }
}

fn sync_label(world: &mut World, entity: Entity) {
    if let Some(input) = world.get::<TextInput>(entity).cloned() {
        sync_label_text(world, entity, &input);
    }
}

/// Places the label, caret and selection rectangles and keeps the caret in view.
fn update_visuals(world: &mut World, entity: Entity, focused: bool, dt: f32) {
    let carets = parts_of(world, entity, TextInputPart::Caret);
    let selections = parts_of(world, entity, TextInputPart::Selection);
    let Some(place) = placement(world, entity) else {
        return;
    };
    let Some(label) = label_of(world, entity) else {
        return;
    };
    let Some(view) = world.get::<TextInput>(entity).map(View::of) else {
        return;
    };
    if place.scrolls && world.get::<ClipChildren>(entity).is_none() {
        world.entity_mut(entity).insert(ClipChildren);
    }
    if !focused {
        for part in carets.iter().chain(&selections) {
            set_style(world, *part, hidden_style());
        }
        // An abandoned composition (focus moved away mid-IME) must not keep its underlined ghost
        // in the label or count as content: drop it with the other focus-only state.
        let had_preedit = world
            .get::<TextInput>(entity)
            .is_some_and(|i| !i.preedit.is_empty() || i.preedit_cursor.is_some());
        if let Some(mut input) = world.get_mut::<TextInput>(entity)
            && (had_preedit || input.ime_rect.is_some() || input.visual_caret.is_some())
        {
            let input = input.bypass_change_detection();
            input.preedit.clear();
            input.preedit_cursor = None;
            input.ime_rect = None;
            input.visual_caret = None;
        }
        if had_preedit {
            sync_label(world, entity);
            world.resource_mut::<LayoutDirty>().mark();
        }
        if place.scrolls {
            set_style(world, label, label_style(view, &place));
        }
        return;
    }
    let _span = tracing::trace_span!("ui_text_input_visuals").entered();
    let Some((spot, preedit_end, display_a, display_b, text_empty, old_scroll, blink)) =
        world.get::<TextInput>(entity).map(|input| {
            let (a, b) = input.selection();
            let spot = input.caret_spot();
            let preedit_end = (!input.preedit.is_empty()).then(|| spot.at + input.preedit_display().len());
            (
                spot,
                preedit_end,
                input.to_display(a),
                input.to_display(b),
                // The map shows the placeholder, not the text, while both text and composition
                // are empty.
                input.text.is_empty() && input.preedit.is_empty(),
                input.scroll,
                (input.blink + dt) % (BLINK_HALF_PERIOD * 2.0),
            )
        })
    else {
        return;
    };
    // Borrow the cached map. Copy out the caret, selection and scroll; do not clone the map.
    let Some((map_h, caret_block, preedit_x, scroll, rects)) = with_caret_map(world, entity, &place, |map| {
        let map_h = map.height();
        let line_h = map.lines.first().map_or(place.content[3], |line| line.height);
        let caret_block = if text_empty {
            [0.0, 0.0, line_h]
        } else {
            caret_block_at(map, &spot)
        };
        // While composing, the IME rect spans the composition. A composition that wraps a
        // visual line reports the caret's (last) line only — x coordinates from two lines
        // would give a bogus width and misplace the candidate window.
        let preedit_x = preedit_end.map(|end| {
            let mut start = spot.at;
            if map.line_of(start) != map.line_of(end)
                && let Some(line) = map.lines.get(map.line_of(end))
            {
                start = start.max(line.start);
            }
            let (a, b) = (map.caret(start)[0], map.caret(end)[0]);
            [a.min(b), a.max(b)]
        });
        let scroll = if place.scrolls {
            keep_caret_visible(view, caret_block, &place, map)
        } else {
            old_scroll
        };
        // The composition stands in for the selection until it is committed.
        let rects = if text_empty || preedit_end.is_some() {
            Vec::new()
        } else {
            selection_rects(map, display_a, display_b)
        };
        (map_h, caret_block, preedit_x, scroll, rects)
    }) else {
        return;
    };
    if let Some(mut slot) = world.get_mut::<TextInput>(entity) {
        // Scroll is layout. Blink is the caret clock: a tracked write made every focused frame
        // look like an edit, so a search box re-ranked its whole list while the caret flashed.
        if slot.scroll != scroll {
            slot.scroll = scroll;
        }
        if slot.blink != blink {
            slot.bypass_change_detection().blink = blink;
        }
    }
    let view = View {
        multiline: view.multiline,
        scroll,
    };
    if place.scrolls {
        set_style(world, label, label_style(view, &place));
    }
    let origin = block_origin(world, entity, view, &place, map_h);
    let to_inner = |x: f32, y: f32| [x - place.inner[0], y - place.inner[1]];
    // The window-space caret and IME rect (derived output: no change mark, like blink).
    let caret_px = [
        origin[0] + caret_block[0],
        origin[1] + caret_block[1],
        caret_block[2],
    ];
    let ime_rect = match preedit_x {
        Some([a, b]) => [
            origin[0] + a,
            caret_px[1],
            (b - a).max(CARET_WIDTH),
            caret_block[2],
        ],
        None => [caret_px[0], caret_px[1], CARET_WIDTH, caret_block[2]],
    };
    if let Some(mut slot) = world.get_mut::<TextInput>(entity)
        && (slot.ime_rect != Some(ime_rect) || slot.visual_caret != Some(caret_px))
    {
        let slot = slot.bypass_change_detection();
        slot.ime_rect = Some(ime_rect);
        slot.visual_caret = Some(caret_px);
    }
    let color = world
        .get::<Themed>(entity)
        .and_then(|t| t.text_color)
        .or_else(|| {
            world
                .get::<Text>(label)
                .and_then(|t| t.spans.first())
                .map(|s| s.style.color)
        })
        .unwrap_or(UiColor::WHITE);
    let highlight = world
        .get::<Themed>(entity)
        .and_then(|t| t.focus_ring)
        .unwrap_or(color)
        .with_alpha(0.35);
    for (i, caret) in carets.iter().enumerate() {
        if i > 0 {
            set_style(world, *caret, hidden_style());
            continue;
        }
        let at = to_inner(origin[0] + caret_block[0], origin[1] + caret_block[1]);
        set_style(
            world,
            *caret,
            rect_style([at[0], at[1], CARET_WIDTH, caret_block[2]], place.scale),
        );
        set_background(world, *caret, color);
        let opacity = if blink < BLINK_HALF_PERIOD { 1.0 } else { 0.0 };
        if world.get::<Opacity>(*caret).is_none_or(|o| o.0 != opacity) {
            world.entity_mut(*caret).insert(Opacity(opacity));
        }
    }
    let mut pool = selections;
    while pool.len() < rects.len() {
        let node = world
            .spawn((
                UiNode,
                TextInputPart::Selection,
                NodeStyle::new(hidden_style()),
                Background::default(),
            ))
            .id();
        // Under the label, so the highlight never covers the glyphs.
        world.entity_mut(entity).insert_child(0, node);
        pool.push(node);
    }
    for (i, node) in pool.iter().enumerate() {
        match rects.get(i) {
            Some(r) => {
                let at = to_inner(origin[0] + r[0], origin[1] + r[1]);
                set_style(world, *node, rect_style([at[0], at[1], r[2], r[3]], place.scale));
                set_background(world, *node, highlight);
            }
            None => set_style(world, *node, hidden_style()),
        }
    }
}

/// The label's style while the field scrolls: absolutely placed in the content box, shifted by
/// the scroll; multi-line labels span the content width so they wrap.
fn label_style(view: View, place: &Placement) -> Style {
    let s = place.scale;
    let pad_l = (place.content[0] - place.inner[0]) / s;
    let pad_t = (place.content[1] - place.inner[1]) / s;
    let mut style = Style {
        position: PositionType::Absolute,
        ..Default::default()
    };
    if view.multiline {
        style.inset = Edges {
            left: Dimension::Points(pad_l),
            top: Dimension::Points(pad_t - view.scroll[1] / s),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        };
        style.size = [Dimension::Points(place.content[2] / s), Dimension::Auto];
    } else {
        style.inset = Edges {
            left: Dimension::Points(pad_l - view.scroll[0] / s),
            top: Dimension::Points(pad_t),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        };
        style.size = [Dimension::Auto, Dimension::Points(place.content[3] / s)];
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::MemoryClipboard;

    const NONE: UiModifiers = UiModifiers {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
    const SHIFT: UiModifiers = UiModifiers { shift: true, ..NONE };
    const CTRL: UiModifiers = UiModifiers { ctrl: true, ..NONE };
    const CTRL_SHIFT: UiModifiers = UiModifiers {
        ctrl: true,
        shift: true,
        ..NONE
    };

    /// Presses `keys` (`"Ctrl+Shift+Left"` style chords) in order on `input`.
    fn press(input: &mut TextInput, clipboard: &mut UiClipboard, keys: &[&str]) -> KeyEffect {
        let mut last = KeyEffect::default();
        for chord in keys {
            let mut mods = NONE;
            let mut parts: Vec<&str> = chord.split('+').collect();
            // A trailing "+" is the plus key itself.
            if chord.ends_with('+') && chord.len() > 1 {
                parts.pop();
                parts.pop();
                parts.push("+");
            }
            let key = parts.pop().unwrap_or_default();
            for m in parts {
                match m {
                    "Shift" => mods.shift = true,
                    "Ctrl" => mods.ctrl = true,
                    "Alt" => mods.alt = true,
                    "Meta" => mods.meta = true,
                    other => panic!("unknown modifier {other}"),
                }
            }
            last = input.apply_key(key, mods, clipboard, None);
        }
        last
    }

    /// `text` with `|` marking the caret and `[`/`]` the selection (anchor and caret in order).
    fn render(input: &TextInput) -> String {
        let (a, b) = input.selection();
        if a == b {
            let mut s = input.text.clone();
            s.insert(input.caret, '|');
            return s;
        }
        let mut s = input.text.clone();
        s.insert(b, ']');
        s.insert(a, '[');
        s
    }

    fn input(text: &str) -> TextInput {
        let mut t = TextInput::default();
        t.set_text(text);
        t
    }

    #[test]
    fn key_sequences_edit_and_select() {
        // (start text, multiline, keys, expected render)
        let table: &[(&str, bool, &[&str], &str)] = &[
            ("hello", false, &["Left", "Left", "x"], "helx|lo"),
            ("hello", false, &["Home", "Shift+Right", "Shift+Right"], "[he]llo"),
            ("hello", false, &["Shift+Home", "Backspace"], "|"),
            ("hello world", false, &["Ctrl+Left"], "hello |world"),
            ("hello world", false, &["Ctrl+Left", "Ctrl+Left"], "|hello world"),
            (
                "hello world",
                false,
                &["Home", "Ctrl+Right", "Ctrl+Right"],
                "hello world|",
            ),
            ("hello world", false, &["Ctrl+Shift+Left"], "hello [world]"),
            ("hello world", false, &["Ctrl+Backspace"], "hello |"),
            ("hello world", false, &["Home", "Ctrl+Delete"], "| world"),
            ("hello", false, &["Ctrl+A"], "[hello]"),
            ("hello", false, &["Ctrl+A", "x"], "x|"),
            ("hello", false, &["Shift+Left", "Shift+Left", "Left"], "hel|lo"),
            ("hello", false, &["Shift+Left", "Shift+Left", "Right"], "hello|"),
            ("hello", false, &["Home", "Delete", "Delete"], "|llo"),
            // Combining marks and emoji move and delete as one character.
            ("cafe\u{301}", false, &["Left", "x"], "cafx|e\u{301}"),
            ("cafe\u{301}", false, &["Backspace"], "caf|"),
            ("a👍🏽b", false, &["Left", "Left", "Delete"], "a|b"),
            ("日本語", false, &["Left", "Shift+Left"], "日[本]語"),
            // Multi-line: Enter inserts a break, Home/End/Up/Down follow logical lines without a layout.
            ("ab\ncd", true, &["Up", "x"], "abx|\ncd"),
            ("abc\nd", true, &["Home", "Up", "Right", "Down"], "abc\nd|"),
            ("ab\ncd", true, &["Home", "Shift+End"], "ab\n[cd]"),
            ("ab\ncd", true, &["Ctrl+Home"], "|ab\ncd"),
            ("ab", true, &["Enter", "c"], "ab\nc|"),
            // Single-line: Up/Down are left to focus navigation.
            ("ab", false, &["Up"], "ab|"),
        ];
        for (start, multiline, keys, want) in table {
            let mut t = TextInput {
                multiline: *multiline,
                ..TextInput::default()
            };
            t.set_text(start);
            let mut clip = UiClipboard::new(MemoryClipboard::default());
            press(&mut t, &mut clip, keys);
            assert_eq!(render(&t), *want, "{start:?} + {keys:?}");
        }
    }

    #[test]
    fn enter_submits_single_line_and_ctrl_enter_submits_multiline() {
        let mut clip = UiClipboard::new(MemoryClipboard::default());
        let mut t = input("go");
        assert!(press(&mut t, &mut clip, &["Enter"]).submitted);
        assert_eq!(t.text, "go");
        let mut m = TextInput {
            multiline: true,
            ..TextInput::default()
        };
        assert!(!press(&mut m, &mut clip, &["Enter"]).submitted);
        assert!(press(&mut m, &mut clip, &["Ctrl+Enter"]).submitted);
    }

    #[test]
    fn clipboard_round_trip_through_the_provider() {
        let probe = MemoryClipboard::default();
        let mut clip = UiClipboard::new(probe.clone());
        let mut t = input("copy me");
        press(&mut t, &mut clip, &["Ctrl+Shift+Left", "Ctrl+C"]);
        assert_eq!(probe.contents().as_deref(), Some("me"));
        press(&mut t, &mut clip, &["Home", "Ctrl+V"]);
        assert_eq!(render(&t), "me|copy me");
        press(&mut t, &mut clip, &["Ctrl+A", "Ctrl+X"]);
        assert_eq!(probe.contents().as_deref(), Some("mecopy me"));
        assert_eq!(t.text, "");
        // Pasting multi-line text into a single-line field turns breaks into spaces.
        clip.set_text("a\r\nb\nc");
        press(&mut t, &mut clip, &["Ctrl+V"]);
        assert_eq!(t.text, "a b c");
    }

    #[test]
    fn secret_and_read_only_fields_guard_their_content() {
        let probe = MemoryClipboard::default();
        let mut clip = UiClipboard::new(probe.clone());
        let mut secret = TextInput {
            secret: Some('*'),
            ..TextInput::default()
        };
        secret.set_text("hunter2");
        press(&mut secret, &mut clip, &["Ctrl+A", "Ctrl+C", "Ctrl+X"]);
        assert_eq!(probe.contents(), None, "masked text is never copied");
        assert_eq!(secret.text, "hunter2");
        assert_eq!(secret.display_text(), "*******");

        let mut ro = input("fixed");
        ro.read_only = true;
        press(
            &mut ro,
            &mut clip,
            &["Backspace", "x", "Ctrl+A", "Ctrl+C", "Ctrl+V", "Ctrl+X"],
        );
        assert_eq!(ro.text, "fixed");
        assert_eq!(
            probe.contents().as_deref(),
            Some("fixed"),
            "read-only text can be copied"
        );
        assert_eq!(render(&ro), "[fixed]");
    }

    #[test]
    fn filters_and_max_chars_reject_whole_edits() {
        let mut clip = UiClipboard::new(MemoryClipboard::default());
        let mut n = TextInput {
            filter: InputFilter::Integer,
            ..TextInput::default()
        };
        press(&mut n, &mut clip, &["-", "4", "x", "2", "."]);
        assert_eq!(n.text, "-42");
        clip.set_text("7a");
        press(&mut n, &mut clip, &["Ctrl+V"]);
        assert_eq!(n.text, "-42", "a paste the filter refuses is dropped whole");

        let mut short = TextInput {
            max_chars: 4,
            ..TextInput::default()
        };
        clip.set_text("abce\u{301}f");
        press(&mut short, &mut clip, &["Ctrl+V"]);
        assert_eq!(short.text, "abc", "a grapheme that does not fit whole is dropped");
    }

    /// U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR are category Zl/Zp, not control
    /// characters, so they used to survive `sanitize` — and the label pipeline normalizes them
    /// to `\n`, drawing a single-line field as two paragraphs and pushing caret offsets past the
    /// end of the text. They must sanitize exactly like `\r`/`\n` (rc1 finding, text+text_input).
    #[test]
    fn unicode_separators_sanitize_like_line_breaks() {
        let mut single = TextInput::default();
        single.set_text("a\u{2028}b");
        assert_eq!(single.text, "a b");
        let mut single9 = TextInput::default();
        single9.set_text("a\u{2029}b");
        assert_eq!(single9.text, "a b");

        let mut multi = TextInput {
            multiline: true,
            ..TextInput::default()
        };
        multi.set_text("a\u{2028}b\u{2029}c");
        assert_eq!(multi.text, "a\nb\nc");

        // The paste path funnels through the same sanitize.
        let mut clip = UiClipboard::new(MemoryClipboard::default());
        clip.set_text("a\u{2028}b\nc");
        let mut pasted = TextInput::default();
        press(&mut pasted, &mut clip, &["Ctrl+V"]);
        assert_eq!(pasted.text, "a b c");
    }

    #[test]
    fn undo_coalesces_typing_and_redo_restores() {
        let mut clip = UiClipboard::new(MemoryClipboard::default());
        let mut t = TextInput::default();
        press(&mut t, &mut clip, &["h", "i", "Space", "y", "o", "u"]);
        assert_eq!(t.text, "hi you");
        assert!(t.can_undo());
        press(&mut t, &mut clip, &["Ctrl+Z"]);
        assert_eq!(render(&t), "hi|");
        press(&mut t, &mut clip, &["Ctrl+Z"]);
        assert_eq!(render(&t), "|");
        assert!(!t.can_undo());
        press(&mut t, &mut clip, &["Ctrl+Shift+Z"]);
        assert_eq!(t.text, "hi");
        press(&mut t, &mut clip, &["Ctrl+Y"]);
        assert_eq!(t.text, "hi you");
        assert!(!t.can_redo());
        // A caret move ends the typing run.
        press(&mut t, &mut clip, &["a", "Left", "Right", "b", "Ctrl+Z"]);
        assert_eq!(t.text, "hi youa");
        // Consecutive Backspaces are one step.
        press(
            &mut t,
            &mut clip,
            &["Backspace", "Backspace", "Backspace", "Ctrl+Z"],
        );
        assert_eq!(t.text, "hi youa");
    }

    #[test]
    fn altgr_types_text_instead_of_a_shortcut() {
        let mut clip = UiClipboard::new(MemoryClipboard::default());
        let mut t = TextInput::default();
        let altgr = UiModifiers {
            ctrl: true,
            alt: true,
            ..NONE
        };
        t.apply_key("@", altgr, &mut clip, None);
        t.apply_key("a", CTRL, &mut clip, None);
        assert_eq!(render(&t), "[@]", "Ctrl+A selected, AltGr typed");
        t.apply_key("Left", SHIFT, &mut clip, None);
        t.apply_key("z", CTRL_SHIFT, &mut clip, None);
        assert_eq!(t.text, "@");
    }

    #[test]
    fn multi_clicks_select_word_and_line() {
        let mut t = TextInput {
            multiline: true,
            ..TextInput::default()
        };
        t.set_text("one two\nthree");
        t.caret = 5;
        t.select_word_at_caret();
        assert_eq!(render(&t), "one [two]\nthree");
        t.caret = 10;
        t.selection_anchor = 10;
        t.select_line_at_caret();
        assert_eq!(render(&t), "one two\n[three]");
    }

    #[test]
    fn outside_positions_are_clamped_before_editing() {
        let mut clip = UiClipboard::new(MemoryClipboard::default());
        let mut t = input("é");
        t.caret = 1; // inside the two-byte é
        t.selection_anchor = 1;
        press(&mut t, &mut clip, &["x"]);
        assert_eq!(t.text, "xé");
        t.caret = 99;
        t.selection_anchor = 99;
        press(&mut t, &mut clip, &["y"]);
        assert_eq!(t.text, "xéy");
    }

    // Ported from the widgets.rs tests when TextInput moved here (same assertions).
    #[test]
    fn text_input_editing() {
        let mut input = TextInput::default();
        input.type_text("héllo");
        assert_eq!(input.text, "héllo");
        assert_eq!(input.caret, 6); // byte position after 'o'
        input.caret = prev_grapheme(&input.text, input.caret);
        input.selection_anchor = input.caret; // collapse selection after caret move
        input.type_text("!");
        assert_eq!(input.text, "héll!o");
        // Selection replace: bytes 0..6 are "héll!", leaving trailing "o".
        input.selection_anchor = 0;
        input.caret = 6;
        input.type_text("x");
        assert_eq!(input.text, "xo");
    }

    #[test]
    fn text_input_max_chars() {
        let mut input = TextInput {
            max_chars: 3,
            ..Default::default()
        };
        input.type_text("abcdef");
        assert_eq!(input.text, "abc");
    }

    #[test]
    fn text_input_secret_mask() {
        let mut input = TextInput {
            secret: Some('*'),
            ..Default::default()
        };
        input.type_text("hunter2");
        assert_eq!(input.display_text(), "*******");
        assert_eq!(input.to_display(input.text.len()), 7);
        assert_eq!(input.byte_from_display(3), 3);
    }

    /// ADR-7208: while composing, label offsets after the caret shift past the composition, an
    /// offset inside it maps back to the caret, and the drawn caret follows the IME cursor.
    #[test]
    fn preedit_shifts_display_offsets_after_the_caret() {
        let mut t = input("abcd");
        t.caret = 2;
        t.selection_anchor = 2;
        t.preedit = "xy".into();
        assert_eq!(
            t.label_parts(),
            ("ab".to_owned(), "xy".to_owned(), "cd".to_owned())
        );
        assert_eq!([t.to_display(1), t.to_display(2), t.to_display(3)], [1, 2, 5]);
        assert_eq!(
            [
                t.byte_from_display(2),
                t.byte_from_display(3),
                t.byte_from_display(4),
                t.byte_from_display(5)
            ],
            [2, 2, 2, 3]
        );
        assert_eq!(t.caret_display(), 4, "no IME cursor: the composition's end");
        t.preedit_cursor = Some((1, 1));
        assert_eq!(t.caret_display(), 3);
        t.preedit_cursor = Some((99, 99));
        assert_eq!(t.caret_display(), 4, "an out-of-range cursor is clamped");
        // Nothing composing: the placeholder shows for an empty field.
        let mut empty = TextInput {
            placeholder: "name".into(),
            ..TextInput::default()
        };
        assert_eq!(empty.label_parts().0, "name");
        empty.preedit = "k".into();
        assert_eq!(
            empty.label_parts(),
            (String::new(), "k".to_owned(), String::new()),
            "a composition replaces the placeholder"
        );
    }

    /// A masked field masks the composition too, and the IME cursor counts characters.
    #[test]
    fn a_secret_field_masks_the_composition() {
        let mut t = TextInput {
            secret: Some('*'),
            ..TextInput::default()
        };
        t.set_text("pw");
        t.preedit = "日本".into();
        assert_eq!(t.label_parts(), ("**".to_owned(), "**".to_owned(), String::new()));
        t.preedit_cursor = Some((3, 3));
        assert_eq!(t.caret_display(), 3, "after the first masked character");
    }

    #[test]
    fn char_boundaries_respect_utf8() {
        let s = "日本語";
        assert_eq!(prev_grapheme(s, 3), 0);
        assert_eq!(next_grapheme(s, 0), 3);
        assert_eq!(prev_grapheme(s, 6), 3);
    }
}
