// SPDX-License-Identifier: MPL-2.0
//! The caption board: a bounded, deterministic presenter state machine
//! (`docs/specs/56-dialogue-and-subtitles.md` §§ 2.6, 2.7, 2.8, 7).
//!
//! The board is a pure function of `(entries, now_us, options, layout)`: it never reads a clock,
//! a random number or a hash order. Entries are kept sorted by id; every ranking has a total order
//! ending in the id.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::caption::{CaptionId, CaptionKind, CaptionStart, CaptionText, CaptionTime};
use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::options::{CaptionOptions, CaptionPolicy, OverlapPolicy, Rgb, SpeakerLabels, effective_colors};
use super::wrap::{TextMeasure, grapheme_count, is_cjk_locale, is_rtl_locale, paginate, wrap};

/// A rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    fn to_json(self) -> Value {
        json!({"x": self.x, "y": self.y, "w": self.w, "h": self.h})
    }
}

/// Platform insets (display cutouts, gesture bars), in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    /// Left.
    pub left: f32,
    /// Top.
    pub top: f32,
    /// Right.
    pub right: f32,
    /// Bottom.
    pub bottom: f32,
}

/// The viewport and its insets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockMetrics {
    /// Viewport width and height in logical pixels.
    pub viewport: (f32, f32),
    /// Platform insets.
    pub inset: Insets,
}

impl Default for BlockMetrics {
    fn default() -> Self {
        Self {
            viewport: (1920.0, 1080.0),
            inset: Insets::default(),
        }
    }
}

/// Text measurement plus the viewport the block sits in.
pub trait BlockLayout: TextMeasure {
    /// The current viewport and insets.
    fn metrics(&self) -> BlockMetrics;
}

/// Title-safe margin as a fraction of each side.
pub const TITLE_SAFE_MARGIN: f32 = 0.05;

/// The safe rectangle: the viewport minus the larger of the platform inset and a 5% margin on each
/// side, minus `bottom_offset` at the bottom.
pub fn safe_rect(metrics: &BlockMetrics, bottom_offset: f32) -> Rect {
    let (vw, vh) = metrics.viewport;
    let left = metrics.inset.left.max(vw * TITLE_SAFE_MARGIN);
    let right = metrics.inset.right.max(vw * TITLE_SAFE_MARGIN);
    let top = metrics.inset.top.max(vh * TITLE_SAFE_MARGIN);
    let bottom = metrics.inset.bottom.max(vh * TITLE_SAFE_MARGIN) + bottom_offset.max(0.0);
    Rect {
        x: left,
        y: top,
        w: (vw - left - right).max(0.0),
        h: (vh - top - bottom).max(0.0),
    }
}

/// The state of one entry (spec § 2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    /// Accepted with a start in the future.
    Pending,
    /// Started, but no row is free.
    Waiting,
    /// On screen.
    Visible,
    /// Ended normally; removed next frame.
    Done,
    /// Evicted, expired or filtered out; removed next frame.
    Dropped,
}

impl EntryState {
    /// The name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Waiting => "Waiting",
            Self::Visible => "Visible",
            Self::Done => "Done",
            Self::Dropped => "Dropped",
        }
    }
}

/// Which side of the row text the speaker label sits on (spec 56 § 2.1, `Speaker.side`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LabelSide {
    /// The reading-start side (left for left-to-right scripts, right for right-to-left ones):
    /// the label comes before the text.
    #[default]
    Start,
    /// The reading-end side: the label comes after the text.
    End,
}

impl LabelSide {
    /// `Start` or `End` (the name `captions.state` reports).
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::End => "End",
        }
    }
}

/// How a speaker is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerStyle {
    /// The display name (already localized).
    pub name: String,
    /// The label color, if the speaker has one.
    pub color: Option<Rgb>,
    /// The side of the row text the label sits on.
    pub side: LabelSide,
}

#[derive(Debug, Clone)]
struct Segment {
    text: String,
    lines: Vec<String>,
    from_us: u64,
    to_us: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    id: CaptionId,
    source: String,
    kind: CaptionKind,
    speaker: Option<String>,
    locale: String,
    rtl: bool,
    priority: u8,
    pieces: Vec<(u64, String)>,
    keep_breaks: bool,
    planned_us: u64,
    start_us: u64,
    end_us: u64,
    hold_until_us: u64,
    state: EntryState,
    segments: Vec<Segment>,
    page: usize,
    layout_key: Option<(u32, u32, usize)>,
    lines: Vec<String>,
    rect: Rect,
    label: Option<String>,
    underrun: bool,
    /// One line per page (`CaptionStyleHint::one_at_a_time`).
    one_at_a_time: bool,
    /// It replaced a visible entry with the same id: it takes that slot before other waiting ones.
    keeps_slot: bool,
    /// It was taken off the screen by an option turned off (spec 36 § 8.4 (4), ADR-5993): shown
    /// again, it keeps its start and end instead of starting over at the show.
    hidden: bool,
    /// Bumped when `show` replaces the page timeline.
    revision: u64,
}

impl Entry {
    fn kind_rank(&self) -> u8 {
        u8::from(self.kind != CaptionKind::Sound)
    }

    fn met_min_display(&self, now: u64, policy: &CaptionPolicy) -> bool {
        now >= self.start_us.saturating_add(policy.min_display_us)
    }
}

/// What `captions.state` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptionSnapshot {
    /// The board clock in microseconds.
    pub clock_us: u64,
    /// The entries in id order.
    pub entries: Vec<Value>,
    /// Entries pending or waiting.
    pub queue_len: usize,
    /// Captions dropped since the board was created.
    pub dropped_total: u64,
    /// Captions that could not satisfy the reading speed within the extension cap.
    pub underrun_total: u64,
    /// Speakers whose color was replaced to meet 4.5:1.
    pub contrast_overrides: Vec<String>,
    /// The block rectangle.
    pub block_rect: Rect,
    /// The safe rectangle.
    pub safe_rect: Rect,
}

impl CaptionSnapshot {
    /// The JSON object the agent returns; `options` is added by the caller.
    pub fn to_json(&self, include_layout: bool) -> Value {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                let mut entry = entry.clone();
                if !include_layout && let Some(object) = entry.as_object_mut() {
                    object.remove("rect");
                    object.remove("lines");
                }
                entry
            })
            .collect();
        let mut out = json!({
            "clock_us": self.clock_us,
            "entries": entries,
            "queue_len": self.queue_len,
            "dropped_total": self.dropped_total,
            "underrun_total": self.underrun_total,
            "contrast_overrides": self.contrast_overrides,
        });
        if include_layout && let Some(object) = out.as_object_mut() {
            object.insert("block_rect".into(), self.block_rect.to_json());
            object.insert("safe_rect".into(), self.safe_rect.to_json());
        }
        out
    }
}

/// What the presenter draws for one visible row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    /// The caption id.
    pub id: CaptionId,
    /// The wrapped lines of the current page.
    pub lines: Vec<String>,
    /// The row rectangle in UI pixels.
    pub rect: Rect,
    /// The speaker label, when it shows.
    pub label: Option<String>,
    /// The label color (already checked against the contrast rule).
    pub label_color: Option<Rgb>,
    /// The paragraph direction.
    pub rtl: bool,
    /// The text size in UI pixels.
    pub size_px: f32,
    /// The caption kind.
    pub kind: CaptionKind,
    /// The side of the text the label sits on (the speaker's `side`).
    pub label_side: LabelSide,
}

/// The presenter state machine.
#[derive(Resource, Debug, Clone, Default)]
pub struct CaptionBoard {
    /// Overlap, limits and reading speed.
    pub policy: CaptionPolicy,
    entries: Vec<Entry>,
    speakers: BTreeMap<String, SpeakerStyle>,
    clock_us: u64,
    dropped_total: u64,
    underrun_total: u64,
    contrast_overrides: BTreeSet<String>,
    block_rect: Rect,
    safe: Rect,
    size_px: f32,
    /// Layouts that measured rows. A held caption whose page did not change does not increase this.
    layout_runs: u64,
    /// Bumped when a layout pass changes rows. Row sync skips work while it stays put.
    content_gen: u64,
    /// Inputs of the last layout. `None` until the first pass, or after a speaker style change.
    layout_stamp: Option<LayoutStamp>,
    /// The theme's `subtitle.size` multiplier and `subtitle.font` (ADR-9340).
    theme_size: Option<f32>,
    theme_font: Option<String>,
}

/// What `layout_rows` compared so a held page can skip measurement.
#[derive(Debug, Clone)]
struct LayoutStamp {
    viewport: (u32, u32),
    inset: [u32; 4],
    bottom_bits: u32,
    frac_bits: u32,
    max_w_bits: u32,
    max_lines: usize,
    scale_bits: u32,
    from_bottom: bool,
    labels: SpeakerLabels,
    speaker_colors: bool,
    text_color: String,
    /// `(id, page, revision)` of the visible entries, oldest first. More than eight forces a layout.
    pages: Vec<(CaptionId, usize, u64)>,
}

/// Whether the options drop a caption of this kind: `enabled` off drops speech and narration,
/// `caption_mode` off drops sound captions (spec 56 § 2.8).
fn filtered_by(kind: CaptionKind, opts: &CaptionOptions) -> bool {
    match kind {
        CaptionKind::Speech | CaptionKind::Narration => !opts.enabled,
        CaptionKind::Sound => !opts.caption_mode,
    }
}

fn ceil_div(a: u128, b: u128) -> u128 {
    a.div_ceil(b.max(1))
}

impl CaptionBoard {
    /// A board with the given policy.
    pub fn new(policy: CaptionPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// The theme's caption style (spec 36 § 8.3, ADR-9340): `size` multiplies the text size on
    /// top of the player's `text_scale` (`None` = 1; clamped to `0.25..=4`), and `font` is the
    /// family rows are measured in. A change re-lays the visible pages.
    pub fn set_theme_style(&mut self, size: Option<f32>, font: Option<&str>) {
        let size = size.filter(|s| s.is_finite()).map(|s| s.clamp(0.25, 4.0));
        if self.theme_size.map(f32::to_bits) == size.map(f32::to_bits) && self.theme_font.as_deref() == font {
            return;
        }
        self.theme_size = size;
        self.theme_font = font.map(str::to_owned);
        self.layout_stamp = None;
    }

    /// The theme's caption size multiplier in force (1 when the theme sets none).
    pub fn theme_size(&self) -> f32 {
        self.theme_size.unwrap_or(1.0)
    }

    /// Registers how a speaker id is shown (name and label color).
    pub fn set_speaker(&mut self, id: impl Into<String>, style: SpeakerStyle) {
        self.speakers.insert(id.into(), style);
        self.layout_stamp = None;
    }

    /// The board clock as of the last `push`/`advance`.
    pub fn clock_us(&self) -> u64 {
        self.clock_us
    }

    /// Entries held (any state).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entry is held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Accepts a caption (pieces must already hold literal text; a `Key` piece shows its key).
    ///
    /// A caption whose id the board already holds replaces that entry (a re-based cue set, spec 36
    /// § 8.2 `sync`); when the old entry was visible the new one takes its place ahead of other
    /// waiting entries, so a re-base never hands the row to someone else.
    pub fn push(&mut self, c: CaptionStart, now_us: u64, opts: &CaptionOptions) {
        self.clock_us = self.clock_us.max(now_us);
        let mut keeps_slot = false;
        // The layout stamp names a visible row as (id, page, revision). A re-base removes the
        // entry and `show` adds one to the revision it is given. Starting again at 0 makes the
        // new cue look like the page just replaced (both revision 1, page 0), so layout skips
        // and the row is left with no lines.
        let mut revision = 0;
        if let Some(at) = self
            .entries
            .iter()
            .position(|e| e.id == c.id && !matches!(e.state, EntryState::Done | EntryState::Dropped))
        {
            keeps_slot = self.entries[at].state == EntryState::Visible;
            revision = self.entries[at].revision;
            self.entries.remove(at);
        }
        let start_us = match c.start {
            CaptionTime::Now => now_us,
            CaptionTime::AtMicros(t) => t,
        };
        let pieces: Vec<(u64, String)> = c
            .pieces
            .iter()
            .map(|p| {
                let raw = match &p.text {
                    CaptionText::Literal(text) => text.clone(),
                    CaptionText::Key { key, .. } => key.clone(),
                };
                let text = if c.keep_breaks {
                    raw
                } else {
                    raw.replace('\n', " ")
                };
                (p.offset_us.min(c.planned_us), text)
            })
            .collect();
        let filtered = filtered_by(c.kind, opts);
        let state = if filtered || self.entries.len() >= self.policy.max_entries {
            self.dropped_total += 1;
            EntryState::Dropped
        } else if start_us > now_us {
            EntryState::Pending
        } else {
            EntryState::Waiting
        };
        let entry = Entry {
            id: c.id,
            source: c.source.describe(),
            kind: c.kind,
            speaker: c.speaker.clone().or(c.hint.speaker.clone()),
            rtl: is_rtl_locale(&c.locale),
            locale: c.locale,
            priority: c.priority,
            pieces,
            keep_breaks: c.keep_breaks,
            planned_us: c.planned_us,
            start_us,
            end_us: start_us.saturating_add(c.planned_us),
            hold_until_us: start_us,
            state,
            segments: Vec::new(),
            page: 0,
            layout_key: None,
            lines: Vec::new(),
            rect: Rect::default(),
            label: None,
            underrun: false,
            one_at_a_time: c.hint.one_at_a_time,
            keeps_slot,
            hidden: false,
            revision,
        };
        let at = self.entries.partition_point(|e| e.id < entry.id);
        self.entries.insert(at, entry);
    }

    /// Ends a caption (its audio stopped): the row still holds to the minimum display time.
    pub fn stop(&mut self, id: CaptionId, now_us: u64) {
        self.clock_us = self.clock_us.max(now_us);
        let min_display = self.policy.min_display_us;
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        match entry.state {
            EntryState::Pending | EntryState::Waiting => {
                entry.state = EntryState::Done;
            }
            EntryState::Visible => {
                entry.end_us = entry.end_us.min(now_us);
                let floor = entry.start_us.saturating_add(min_display);
                entry.hold_until_us = entry.hold_until_us.min(now_us.max(floor));
            }
            EntryState::Done | EntryState::Dropped => {}
        }
    }

    fn reading_cps(&self, locale: &str, opts: &CaptionOptions) -> u32 {
        opts.reading_cps
            .unwrap_or(if is_cjk_locale(locale) {
                self.policy.cps_cjk
            } else {
                self.policy.cps_alphabetic
            })
            .max(1)
    }

    fn max_lines(&self, opts: &CaptionOptions) -> usize {
        if opts.text_scale >= 1.5 {
            self.policy.max_lines.max(3)
        } else {
            self.policy.max_lines
        }
    }

    /// Advances to `now_us`: removes last frame's finished entries, starts, ends, evicts and
    /// promotes entries, then lays out the visible rows.
    pub fn advance(&mut self, now_us: u64, opts: &CaptionOptions, layout: &mut dyn BlockLayout) {
        let _span = tracing::trace_span!("caption_board").entered();
        self.clock_us = self.clock_us.max(now_us);
        let now = self.clock_us;
        self.entries
            .retain(|e| !matches!(e.state, EntryState::Done | EntryState::Dropped));

        for entry in &mut self.entries {
            if entry.state == EntryState::Pending && entry.start_us <= now {
                entry.state = EntryState::Waiting;
            }
        }
        self.cap_at_next_piece();
        for entry in &mut self.entries {
            if entry.state == EntryState::Visible && now >= entry.hold_until_us {
                entry.state = EntryState::Done;
            }
        }
        let max_wait = self.policy.max_wait_us;
        let mut dropped = 0;
        for entry in &mut self.entries {
            if entry.state == EntryState::Waiting
                && !entry.hidden
                && now > entry.end_us.saturating_add(max_wait)
            {
                entry.state = EntryState::Dropped;
                dropped += 1;
            }
        }
        // An option turned off since the push hides the rows it filters without ending them
        // (spec 36 § 8.4 (4): "nothing shown, sets still advance and finish"; ADR-5993): a row on
        // screen leaves it and waits, keeps its timing, finishes at its end like a shown row, and
        // shows again at its current text if the option comes back on before then. A row pushed
        // while the option is off is still dropped at the push (spec 56 § 2.8).
        for entry in &mut self.entries {
            if matches!(entry.state, EntryState::Done | EntryState::Dropped) {
                continue;
            }
            if filtered_by(entry.kind, opts) {
                if entry.state == EntryState::Visible {
                    entry.state = EntryState::Waiting;
                    entry.keeps_slot = true;
                }
                entry.hidden = true;
            }
            if entry.hidden && entry.state != EntryState::Visible && now >= entry.end_us {
                entry.state = EntryState::Done;
            }
        }
        self.dropped_total += dropped;

        self.promote(now, opts, layout);
        self.layout_rows(now, opts, layout);
    }

    /// Spec 56 § 2.7: a visible row is not held past the start of the next piece of the same
    /// speaker (the earliest later-starting entry of that speaker that is still pending, waiting
    /// or visible). The cap only shortens the hold: never below the planned end or the minimum
    /// display time, like a stop (ADR-7237).
    fn cap_at_next_piece(&mut self) {
        let min_display = self.policy.min_display_us;
        for i in 0..self.entries.len() {
            let cap = {
                let e = &self.entries[i];
                let Some(speaker) = e.speaker.as_deref().filter(|_| e.state == EntryState::Visible) else {
                    continue;
                };
                let next = self
                    .entries
                    .iter()
                    .filter(|o| {
                        o.id != e.id
                            && !matches!(o.state, EntryState::Done | EntryState::Dropped)
                            && o.speaker.as_deref() == Some(speaker)
                            && o.start_us > e.start_us
                    })
                    .map(|o| o.start_us)
                    .min();
                let Some(next) = next else {
                    continue;
                };
                let floor = e.end_us.max(e.start_us.saturating_add(min_display));
                next.max(floor)
            };
            let entry = &mut self.entries[i];
            entry.hold_until_us = entry.hold_until_us.min(cap);
        }
    }

    fn slots(&self) -> usize {
        match self.policy.overlap {
            OverlapPolicy::Queue => 1,
            _ => self.policy.max_visible.max(1),
        }
    }

    fn promote(&mut self, now: u64, opts: &CaptionOptions, layout: &mut dyn BlockLayout) {
        // A row hidden by an option that is off waits for it (spec 36 § 8.4 (4), ADR-5993).
        let mut candidates: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                self.entries[i].state == EntryState::Waiting && !filtered_by(self.entries[i].kind, opts)
            })
            .collect();
        candidates.sort_by_key(|&i| {
            let e = &self.entries[i];
            (
                std::cmp::Reverse(e.keeps_slot),
                std::cmp::Reverse(e.priority),
                std::cmp::Reverse(e.kind_rank()),
                e.start_us,
                e.id,
            )
        });
        let slots = self.slots();
        for index in candidates {
            if self.policy.overlap == OverlapPolicy::ReplaceSameSpeaker
                && self.entries[index].kind == CaptionKind::Speech
                && let Some(speaker) = self.entries[index].speaker.clone()
            {
                for other in &mut self.entries {
                    if other.state == EntryState::Visible
                        && other.kind == CaptionKind::Speech
                        && other.speaker.as_deref() == Some(speaker.as_str())
                    {
                        other.state = EntryState::Done;
                    }
                }
            }
            let visible = self
                .entries
                .iter()
                .filter(|e| e.state == EntryState::Visible)
                .count();
            if visible >= slots {
                if self.policy.overlap == OverlapPolicy::Queue {
                    continue;
                }
                let candidate = &self.entries[index];
                let key = (candidate.priority, candidate.kind_rank());
                let victim = self
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.state == EntryState::Visible)
                    .min_by_key(|(_, e)| {
                        (
                            e.priority,
                            e.kind_rank(),
                            std::cmp::Reverse(e.met_min_display(now, &self.policy)),
                            e.start_us,
                            e.id,
                        )
                    })
                    .map(|(i, e)| (i, (e.priority, e.kind_rank()), e.kind));
                match victim {
                    Some((v, v_key, v_kind))
                        if key > v_key
                            && (candidate.kind != CaptionKind::Sound || v_kind == CaptionKind::Sound) =>
                    {
                        self.entries[v].state = EntryState::Dropped;
                        self.dropped_total += 1;
                    }
                    _ => continue,
                }
            }
            self.show(index, now, opts, layout);
        }
    }

    /// Turns a waiting entry into a visible one: shifts its start to now if it waited, builds the
    /// page timeline and the hold time.
    fn show(&mut self, index: usize, now: u64, opts: &CaptionOptions, layout: &mut dyn BlockLayout) {
        let metrics = layout.metrics();
        let cps = u128::from(self.reading_cps(&self.entries[index].locale, opts));
        let max_lines = self.max_lines(opts);
        let (size_px, width) = self.text_params(&metrics, opts);
        let policy = self.policy.clone();
        let entry = &mut self.entries[index];
        let max_lines = if entry.one_at_a_time { 1 } else { max_lines };
        entry.keeps_slot = false;
        // A row the options hid keeps its timeline: it shows the text its time has reached.
        if now > entry.start_us && !std::mem::take(&mut entry.hidden) {
            entry.start_us = now;
            entry.end_us = now.saturating_add(entry.planned_us);
        }
        // Spec 56 § 10: wrap + paginate of one entry, run once when the entry is shown.
        let _span = tracing::trace_span!("caption_layout").entered();
        let mut segments: Vec<Segment> = Vec::new();
        let pieces = if entry.pieces.is_empty() {
            vec![(0, String::new())]
        } else {
            entry.pieces.clone()
        };
        for (k, (offset, text)) in pieces.iter().enumerate() {
            let window_end = pieces.get(k + 1).map_or(entry.planned_us, |next| next.0);
            let window = window_end.saturating_sub(*offset);
            // An empty piece is a gap between cues: nothing shows until the next one.
            if text.is_empty() {
                segments.push(Segment {
                    text: String::new(),
                    lines: vec![String::new()],
                    from_us: *offset,
                    to_us: window_end,
                });
                continue;
            }
            for page in paginate(text, window, width, max_lines, entry.rtl, size_px, layout) {
                segments.push(Segment {
                    text: page.text,
                    lines: page.lines,
                    from_us: offset.saturating_add(page.from_us),
                    to_us: offset.saturating_add(page.to_us),
                });
            }
        }
        // Reading speed: only the last page may be held past its window, by at most max_extend.
        let mut underrun = false;
        let last = segments.len().saturating_sub(1);
        for (i, segment) in segments.iter_mut().enumerate() {
            let floor = ceil_div(grapheme_count(&segment.text) as u128 * 1_000_000, cps) as u64;
            if i == last {
                let wanted = segment.from_us.saturating_add(floor);
                let cap = segment.to_us.saturating_add(policy.max_extend_us);
                segment.to_us = segment.to_us.max(wanted.min(cap));
            }
            if segment.to_us.saturating_sub(segment.from_us) < floor {
                underrun = true;
            }
        }
        let hold_off = segments.last().map_or(0, |s| s.to_us).max(entry.planned_us);
        entry.hold_until_us = entry.start_us.saturating_add(hold_off.max(policy.min_display_us));
        entry.segments = segments;
        entry.page = 0;
        entry.layout_key = None;
        entry.underrun = underrun;
        entry.state = EntryState::Visible;
        entry.revision = entry.revision.wrapping_add(1);
        if underrun {
            self.underrun_total += 1;
        }
    }

    fn text_params(&self, metrics: &BlockMetrics, opts: &CaptionOptions) -> (f32, f32) {
        let safe = safe_rect(metrics, self.policy.bottom_offset);
        let size_px = (safe.h / 24.0 * opts.text_scale * self.theme_size()).max(1.0);
        let block_w = (self.policy.max_width_frac * safe.w).min(self.policy.max_width_px);
        (size_px, block_w.max(1.0))
    }

    /// True when `now` is still on the pages last laid out, for these options and metrics.
    fn layout_is_current(&self, now: u64, opts: &CaptionOptions, metrics: BlockMetrics) -> bool {
        let max_lines = self.max_lines(opts);
        let bottom_bits = self.policy.bottom_offset.to_bits();
        let frac_bits = self.policy.max_width_frac.to_bits();
        let max_w_bits = self.policy.max_width_px.to_bits();
        let labels = opts.effective_labels();
        let from_bottom = opts.position == super::options::CaptionPosition::Bottom;
        let copied = {
            let Some(stamp) = &self.layout_stamp else {
                return false;
            };
            let scalars = stamp.viewport == (metrics.viewport.0.to_bits(), metrics.viewport.1.to_bits())
                && stamp.inset
                    == [
                        metrics.inset.left.to_bits(),
                        metrics.inset.top.to_bits(),
                        metrics.inset.right.to_bits(),
                        metrics.inset.bottom.to_bits(),
                    ]
                && stamp.bottom_bits == bottom_bits
                && stamp.frac_bits == frac_bits
                && stamp.max_w_bits == max_w_bits
                && stamp.max_lines == max_lines
                && stamp.scale_bits == opts.text_scale.to_bits()
                && stamp.from_bottom == from_bottom
                && stamp.labels == labels
                && stamp.speaker_colors == opts.speaker_colors
                && stamp.text_color == opts.text_color;
            if !scalars || stamp.pages.len() > 8 {
                return false;
            }
            let mut pages = [(CaptionId(0), 0usize, 0u64); 8];
            let n = stamp.pages.len();
            for (i, page) in stamp.pages.iter().enumerate() {
                pages[i] = *page;
            }
            (pages, n)
        };
        self.pages_match(now, &copied.0[..copied.1])
    }

    /// Whether the visible entries are still `pages` (oldest first). More than eight never matches.
    fn pages_match(&self, now: u64, pages: &[(CaptionId, usize, u64)]) -> bool {
        let mut idx = [0usize; 8];
        let mut n = 0usize;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.state != EntryState::Visible {
                continue;
            }
            if n == idx.len() {
                return false;
            }
            idx[n] = i;
            n += 1;
        }
        if n != pages.len() {
            return false;
        }
        idx[..n].sort_by_key(|&i| (self.entries[i].start_us, self.entries[i].id));
        idx[..n].iter().zip(pages).all(|(&i, page)| {
            let entry = &self.entries[i];
            let rel = now.saturating_sub(entry.start_us);
            // The same fallback as `layout_rows`: before the first segment, past the last page.
            let at = entry
                .segments
                .iter()
                .rposition(|s| s.from_us <= rel)
                .unwrap_or(entry.segments.len());
            page.0 == entry.id && page.1 == at && page.2 == entry.revision
        })
    }

    /// The stamp of the rows `layout_rows` just wrote.
    fn capture_stamp(&self, _now: u64, opts: &CaptionOptions, metrics: BlockMetrics) -> LayoutStamp {
        let mut order: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].state == EntryState::Visible)
            .collect();
        order.sort_by_key(|&i| (self.entries[i].start_us, self.entries[i].id));
        LayoutStamp {
            viewport: (metrics.viewport.0.to_bits(), metrics.viewport.1.to_bits()),
            inset: [
                metrics.inset.left.to_bits(),
                metrics.inset.top.to_bits(),
                metrics.inset.right.to_bits(),
                metrics.inset.bottom.to_bits(),
            ],
            bottom_bits: self.policy.bottom_offset.to_bits(),
            frac_bits: self.policy.max_width_frac.to_bits(),
            max_w_bits: self.policy.max_width_px.to_bits(),
            max_lines: self.max_lines(opts),
            scale_bits: opts.text_scale.to_bits(),
            from_bottom: opts.position == super::options::CaptionPosition::Bottom,
            labels: opts.effective_labels(),
            speaker_colors: opts.speaker_colors,
            text_color: opts.text_color.clone(),
            pages: order
                .into_iter()
                .map(|i| {
                    let entry = &self.entries[i];
                    (entry.id, entry.page, entry.revision)
                })
                .collect(),
        }
    }

    fn layout_rows(&mut self, now: u64, opts: &CaptionOptions, layout: &mut dyn BlockLayout) {
        let metrics = layout.metrics();
        if self.layout_is_current(now, opts, metrics) {
            return;
        }
        // Spec 56 § 10: the re-wrap pass (runs only when text, options or the block change).
        let _span = tracing::trace_span!("caption_layout").entered();
        self.layout_runs = self.layout_runs.wrapping_add(1);
        let safe = safe_rect(&metrics, self.policy.bottom_offset);
        let (size_px, block_w) = self.text_params(&metrics, opts);
        let max_lines = self.max_lines(opts);
        let line_h = size_px * 1.25;
        let pad = size_px * 0.5;
        let block_x = safe.x + (safe.w - block_w) / 2.0;
        self.safe = safe;
        self.size_px = size_px;
        self.block_rect = Rect {
            x: block_x,
            y: safe.y,
            w: block_w,
            h: safe.h,
        };
        let key = (size_px.to_bits(), block_w.to_bits(), max_lines);
        let mut rewraps = 0;
        // A row that needed a rewrap past this pass's cap: the layout is not current yet.
        let mut deferred = false;
        let mut overrides: Vec<String> = Vec::new();

        let mut order: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].state == EntryState::Visible)
            .collect();
        order.sort_by_key(|&i| (self.entries[i].start_us, self.entries[i].id));

        let labels = opts.effective_labels();
        let mut previous_speaker: Option<Option<String>> = None;
        let mut heights: Vec<(usize, f32, f32)> = Vec::new();
        for &i in &order {
            let rel = now.saturating_sub(self.entries[i].start_us);
            let rtl = self.entries[i].rtl;
            let entry = &mut self.entries[i];
            let entry_lines = if entry.one_at_a_time { 1 } else { max_lines };
            let key = (key.0, key.1, entry_lines);
            // No segment started yet (the first cue lies in the future): show nothing rather
            // than fall back to page 0, which would leak the first cue's text early. The
            // out-of-range index lands in the empty-lines branch below, and the row hides.
            let page = entry
                .segments
                .iter()
                .rposition(|s| s.from_us <= rel)
                .unwrap_or(entry.segments.len());
            let page_changed = page != entry.page;
            entry.page = page;
            let stale = page_changed || entry.layout_key != Some(key) || entry.lines.is_empty();
            if stale && rewraps >= 2 {
                deferred = true;
            }
            if stale && rewraps < 2 {
                if let Some(segment) = entry.segments.get(page) {
                    let relaid = if entry.layout_key.is_some() && entry.layout_key != Some(key) {
                        // A resize reflows within the current page only.
                        wrap(&segment.text, block_w, entry_lines, rtl, size_px, layout).lines
                    } else {
                        segment.lines.clone()
                    };
                    entry.lines = relaid;
                } else {
                    entry.lines = vec![String::new()];
                }
                entry.layout_key = Some(key);
                rewraps += 1;
            }
            let text_w = entry
                .lines
                .iter()
                .map(|l| layout.width(l, size_px, rtl))
                .fold(0.0_f32, f32::max);
            let row_w = (text_w + 2.0 * pad).min(block_w);
            let row_h = entry.lines.len().max(1) as f32 * line_h + pad;
            // Speaker label.
            let speaker = self.entries[i].speaker.clone();
            let show_label = match labels {
                SpeakerLabels::Off => false,
                SpeakerLabels::Always => speaker.is_some(),
                SpeakerLabels::OnChange => speaker.is_some() && previous_speaker.as_ref() != Some(&speaker),
            };
            let label = if show_label {
                speaker.as_ref().map(|id| {
                    let style = self.speakers.get(id);
                    if let Some(color) = style.and_then(|s| s.color)
                        && opts.speaker_colors
                    {
                        let (_, _, replaced) = effective_colors(opts, color);
                        if replaced {
                            overrides.push(id.clone());
                        }
                    }
                    style.map_or_else(|| id.clone(), |s| s.name.clone())
                })
            } else {
                None
            };
            previous_speaker = Some(speaker);
            if self.entries[i].label.as_deref() != label.as_deref() {
                self.entries[i].label = label;
            }
            heights.push((i, row_w, row_h));
        }
        for id in overrides {
            self.contrast_overrides.insert(id);
        }
        // Stack newest at the bottom (or hang from the top).
        let from_bottom = opts.position == super::options::CaptionPosition::Bottom;
        let mut cursor = if from_bottom { safe.y + safe.h } else { safe.y };
        if from_bottom {
            for &(i, row_w, row_h) in heights.iter().rev() {
                let y = cursor - row_h;
                self.entries[i].rect = Rect {
                    x: block_x + (block_w - row_w) / 2.0,
                    y,
                    w: row_w,
                    h: row_h,
                };
                cursor = y;
            }
        } else {
            for &(i, row_w, row_h) in &heights {
                let y = cursor;
                self.entries[i].rect = Rect {
                    x: block_x + (block_w - row_w) / 2.0,
                    y,
                    w: row_w,
                    h: row_h,
                };
                cursor = y + row_h;
            }
        }
        // A deferred row is laid out on the next pass; stamping now would skip that pass.
        self.layout_stamp = if deferred {
            None
        } else {
            Some(self.capture_stamp(now, opts, metrics))
        };
        self.content_gen = self.content_gen.wrapping_add(1);
    }

    /// What `captions.state` returns.
    pub fn snapshot(&self) -> CaptionSnapshot {
        let entries = self
            .entries
            .iter()
            .map(|e| {
                let lines: Vec<&str> = e.lines.iter().map(String::as_str).collect();
                json!({
                    "id": e.id.0,
                    "source": e.source,
                    "kind": e.kind.name(),
                    "speaker": e.speaker,
                    "state": e.state.name(),
                    "priority": e.priority,
                    "start_us": e.start_us,
                    "end_us": e.end_us,
                    "hold_until_us": e.hold_until_us,
                    "page": e.page,
                    "pages": e.segments.len(),
                    "text": e.segments.get(e.page).map_or_else(
                        || e.pieces.first().map(|p| p.1.clone()).unwrap_or_default(),
                        |s| s.text.clone(),
                    ),
                    "lines": lines,
                    "rect": e.rect.to_json(),
                    "label": e.label,
                    "label_side": self.label_side(e).name(),
                    "rtl": e.rtl,
                    "keep_breaks": e.keep_breaks,
                    "one_at_a_time": e.one_at_a_time,
                })
            })
            .collect();
        CaptionSnapshot {
            clock_us: self.clock_us,
            entries,
            queue_len: self
                .entries
                .iter()
                .filter(|e| matches!(e.state, EntryState::Pending | EntryState::Waiting))
                .count(),
            dropped_total: self.dropped_total,
            underrun_total: self.underrun_total,
            contrast_overrides: self.contrast_overrides.iter().cloned().collect(),
            block_rect: self.block_rect,
            safe_rect: self.safe,
        }
    }

    /// The visible rows in draw order (oldest first), for the UI nodes.
    pub fn visible_rows(&self, opts: &CaptionOptions) -> Vec<RowView> {
        // A row in a cue gap has no text: no node, no plate.
        let mut rows: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.state == EntryState::Visible && !e.lines.iter().all(String::is_empty))
            .collect();
        rows.sort_by_key(|e| (e.start_us, e.id));
        rows.into_iter()
            .map(|e| {
                let label_color = e
                    .speaker
                    .as_ref()
                    .and_then(|id| self.speakers.get(id))
                    .and_then(|s| s.color)
                    .filter(|_| opts.speaker_colors)
                    .map(|c| effective_colors(opts, c).0);
                RowView {
                    id: e.id,
                    lines: e.lines.clone(),
                    rect: e.rect,
                    label: e.label.clone(),
                    label_color,
                    rtl: e.rtl,
                    size_px: self.size_px,
                    kind: e.kind,
                    label_side: self.label_side(e),
                }
            })
            .collect()
    }

    fn label_side(&self, entry: &Entry) -> LabelSide {
        entry
            .speaker
            .as_ref()
            .and_then(|id| self.speakers.get(id))
            .map_or(LabelSide::Start, |s| s.side)
    }

    /// The safe rectangle of the last layout.
    pub fn safe_area(&self) -> Rect {
        self.safe
    }

    /// The window of the page a row shows now, in absolute board microseconds (`from`, `to`).
    /// Used by karaoke highlighting; `None` when the board does not hold the entry.
    pub fn page_window(&self, id: CaptionId) -> Option<(u64, u64)> {
        let e = self.entries.iter().find(|e| e.id == id)?;
        match e.segments.get(e.page) {
            Some(s) => Some((
                e.start_us.saturating_add(s.from_us),
                e.start_us.saturating_add(s.to_us),
            )),
            None => Some((e.start_us, e.end_us)),
        }
    }

    /// The state of one entry, if the board holds it.
    pub fn state_of(&self, id: CaptionId) -> Option<EntryState> {
        self.entries.iter().find(|e| e.id == id).map(|e| e.state)
    }

    /// Layouts that measured rows (tests). A held caption does not increase this.
    #[cfg(test)]
    pub(super) fn layout_runs(&self) -> u64 {
        self.layout_runs
    }

    /// Generation of the last layout that measured rows. Unchanged while a held page stays put.
    pub(super) fn content_gen(&self) -> u64 {
        self.content_gen
    }

    /// Visible rows that have text. The same filter the row sync uses.
    pub(super) fn visible_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.state == EntryState::Visible && !entry.lines.iter().all(String::is_empty))
            .count()
    }

    /// The lines currently shown for an entry.
    pub fn lines_of(&self, id: CaptionId) -> Option<&[String]> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.lines.as_slice())
    }
}
