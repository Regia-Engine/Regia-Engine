// SPDX-License-Identifier: MPL-2.0
//! Captions and subtitles presenter (`docs/specs/56-dialogue-and-subtitles.md`, ADR-1697).
//!
//! The pieces:
//! - [`CaptionBoard`]: a bounded, deterministic state machine. It takes `CaptionStart` and
//!   `CaptionStop` messages from `regia_app::caption`, decides which rows are visible, paginates
//!   and wraps their text, holds each row for its reading time and lays the rows out in the safe
//!   area. It reads no clock: callers pass `now_us`.
//! - [`wrap`] and [`paginate`]: pure line breaking over a [`TextMeasure`].
//! - [`CaptionOptions`]: the player's options (scale, background, labels, color, position, speed).
//! - [`CaptionPlugin`]: feeds the board from messages on the simulation clock, resolving Fluent
//!   keys, and exposes the board to the agent protocol (`captions.state`, `captions.set_options`).
//!
//! The older [`SubtitleQueue`](crate::subtitles::SubtitleQueue) is unchanged;
//! [`caption_from_subtitle`] routes one of its lines into the board.

pub mod anchor;
mod board;
pub mod karaoke;
mod options;
mod rows;
pub mod sets;
mod wrap;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::ops::Range;

use regia_app::caption::{
    CaptionCueEvent, CaptionId, CaptionIds, CaptionKind, CaptionPiece, CaptionSource, CaptionStart,
    CaptionStop, CaptionStyleHint, CaptionText, CaptionTime,
};
use regia_app::prelude::*;

pub use anchor::{
    ANCHOR_GAP, AnchorInput, AnchorMode, AnchorPlacement, AnchorTarget, CaptionAnchorState,
    CaptionAnchorView, CaptionAnchors, CaptionPlacement, DialogueCaptionSettings, EdgeArrow, Projected,
    arrow_glyph, caption_anchor_system, place_anchored, project_point,
};
pub use board::{
    BlockLayout, BlockMetrics, CaptionBoard, CaptionSnapshot, EntryState, Insets, LabelSide, Rect, RowView,
    SpeakerStyle, TITLE_SAFE_MARGIN, safe_rect,
};
pub use options::{
    CaptionBackground, CaptionOptions, CaptionPolicy, CaptionPosition, MIN_CONTRAST, OptionError,
    OverlapPolicy, Rgb, SpeakerLabels, effective_colors, parse_text_color,
};
pub use rows::{
    CAPTION_OUTLINE_WIDTH, CAPTION_SORT_ORDER, CaptionBlock, CaptionRow, CaptionThemeStyle, sync_caption_rows,
};
pub use sets::{
    SYNC_TOLERANCE_US, SetRequest, SubtitleKind, SubtitleSets, SyncOutcome, cue_pieces, pieces_from,
    queue_set, stop_all_sets, stop_owner, stop_set, sync_set,
};
pub use wrap::{Page, TextMeasure, Wrapped, grapheme_count, is_cjk_locale, is_rtl_locale, paginate, wrap};

use crate::localization::Localization;
use crate::scale::UiScales;
use crate::subtitles::SubtitleLine;
use crate::text::{CaretCluster, CaretMap, Text, TextSpan, TextStyle, TextSystem, WrapMode};

/// The caption clock in microseconds: fixed-step simulation time, never wall time.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CaptionClock(pub u64);

/// Keeps the caption clock on the audio output's sample clock (spec 56 § 2, ADR-1882), so the text
/// and the sound of a voiced line run on one clock. `regia_framework` writes
/// [`Self::device_us`] every frame from `regia_audio::Audio::sample_clock`; without a reading
/// (headless, no device, suspended, a render-out mixdown) the caption clock is fixed-step time.
///
/// The clock holds while the game is paused or suspended and picks the device up again where it
/// stopped, so a paused line neither runs out nor jumps.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CaptionDeviceClock {
    /// The output's sample clock now, microseconds; `None` when there is none.
    pub device_us: Option<u64>,
    /// `caption clock - device clock` while following.
    offset_us: Option<i64>,
    /// The caption clock has followed the device at least once.
    followed: bool,
    /// Simulation time at the last [`Self::step`].
    last_sim_us: u64,
}

impl CaptionDeviceClock {
    /// Whether the caption clock is on the device clock now.
    #[must_use]
    pub fn following(&self) -> bool {
        self.offset_us.is_some()
    }

    /// A device reading on the caption clock; `None` while not following.
    #[must_use]
    pub fn to_caption(&self, device_us: u64) -> Option<u64> {
        Some(device_us.saturating_add_signed(self.offset_us?))
    }

    /// One frame: the caption clock after `clock_us`, given the simulation time (`None` in an app
    /// without [`Time`]) and whether the game is paused or suspended. Never goes backwards.
    pub fn step(&mut self, clock_us: u64, sim_us: Option<u64>, paused: bool) -> u64 {
        let sim = sim_us.unwrap_or(self.last_sim_us);
        let sim_step = sim.saturating_sub(self.last_sim_us);
        self.last_sim_us = sim;
        let Some(device) = self.device_us.filter(|_| !paused) else {
            self.offset_us = None;
            return if self.followed {
                clock_us.saturating_add(sim_step)
            } else {
                clock_us.max(sim)
            };
        };
        // The first reading takes the device's own time when it is ahead, so a caption clock
        // that has not run yet equals the frame counter; later ones continue where the clock is.
        let first = !self.followed && device >= clock_us;
        let offset = *self.offset_us.get_or_insert_with(|| {
            if first {
                0
            } else {
                clock_us.wrapping_sub(device).cast_signed()
            }
        });
        self.followed = true;
        clock_us.max(device.saturating_add_signed(offset))
    }
}

/// Platform insets for the caption block (the platform glue sets them from the display cutout
/// and gesture bars).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq)]
pub struct CaptionInsets(pub Insets);

/// One mapped cue event (`cue_events.ron`): the caption shown for a `CaptionCueEvent` name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CueEventDef {
    /// The Fluent key to show.
    pub key: String,
    /// The kind of caption (usually `Sound`).
    pub kind: CaptionKindDef,
    /// Planned length in milliseconds.
    pub planned_ms: u64,
}

/// Serializable form of [`CaptionKind`] for `cue_events.ron`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaptionKindDef {
    /// [`CaptionKind::Speech`].
    Speech,
    /// [`CaptionKind::Narration`].
    Narration,
    /// [`CaptionKind::Sound`].
    Sound,
}

impl From<CaptionKindDef> for CaptionKind {
    fn from(kind: CaptionKindDef) -> Self {
        match kind {
            CaptionKindDef::Speech => Self::Speech,
            CaptionKindDef::Narration => Self::Narration,
            CaptionKindDef::Sound => Self::Sound,
        }
    }
}

/// The project's mapping from cue event names to captions (`assets/conversations/cue_events.ron`).
/// An unmapped name is ignored.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct CueEventMap(pub BTreeMap<String, CueEventDef>);

/// A measure that needs no font: 0.5 em per narrow grapheme and 1 em per wide one. Games use it
/// until the shaper-backed measure is installed, and tests use it for exact layout.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmMeasure;

impl TextMeasure for EmMeasure {
    fn width(&mut self, text: &str, size_px: f32, _rtl: bool) -> f32 {
        use unicode_segmentation::UnicodeSegmentation;
        text.graphemes(true)
            .map(|g| {
                let wide = g.chars().next().is_some_and(|c| {
                    matches!(c, '\u{1100}'..='\u{115F}' | '\u{2E80}'..='\u{A4CF}' | '\u{AC00}'..='\u{D7A3}'
                        | '\u{F900}'..='\u{FAFF}' | '\u{FE30}'..='\u{FE6F}' | '\u{FF00}'..='\u{FF60}'
                        | '\u{20000}'..='\u{3FFFD}')
                });
                if wide { size_px } else { size_px * 0.5 }
            })
            .sum()
    }
}

/// An [`EmMeasure`] sitting in a fixed viewport.
#[derive(Debug, Clone, Copy)]
pub struct FixedLayout {
    /// The viewport and insets.
    pub metrics: BlockMetrics,
}

impl TextMeasure for FixedLayout {
    fn width(&mut self, text: &str, size_px: f32, rtl: bool) -> f32 {
        EmMeasure.width(text, size_px, rtl)
    }
}

impl BlockLayout for FixedLayout {
    fn metrics(&self) -> BlockMetrics {
        self.metrics
    }
}

/// One text's glyph clusters, line by line, for [`ShapedLayout`]'s range widths.
#[derive(Debug, Default)]
struct RangeClusters {
    text: String,
    size_bits: u32,
    /// Empty when the text holds right-to-left runs: their ranges are shaped one by one.
    lines: Vec<(Range<usize>, Vec<CaretCluster>)>,
}

impl RangeClusters {
    fn new(text: &str, size_bits: u32, map: CaretMap) -> Self {
        let any_rtl = map.lines.iter().flat_map(|l| &l.clusters).any(|c| c.rtl);
        let lines = if any_rtl {
            Vec::new()
        } else {
            map.lines
                .into_iter()
                .map(|l| (l.start..l.end, l.clusters))
                .collect()
        };
        Self {
            text: text.to_owned(),
            size_bits,
            lines,
        }
    }

    /// The width of `range` when it starts and ends on cluster edges of one line.
    fn width(&self, range: &Range<usize>) -> Option<f32> {
        let (_, clusters) = self
            .lines
            .iter()
            .find(|(line, _)| line.start <= range.start && range.end <= line.end)?;
        let first = clusters.get(clusters.partition_point(|c| c.start < range.start))?;
        let last = clusters.get(clusters.partition_point(|c| c.end < range.end))?;
        (first.start == range.start && last.end == range.end).then_some(last.x1 - first.x0)
    }
}

/// How many widths [`MeasureCache`] keeps. One more distinct line drops the least recently used.
const WIDTH_CACHE_CAP: usize = 2048;

/// One remembered width. The owned text rejects a hash collision.
#[derive(Debug)]
struct CachedWidth {
    text: String,
    width: f32,
    /// Last use. Eviction drops the lowest stamp, which is unique.
    used: u64,
}

fn width_key(text: &str, size_px: f32) -> (u64, u32) {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    (hasher.finish(), size_px.to_bits())
}

/// What [`ShapedLayout`] remembers between frames.
#[derive(Debug, Default)]
pub struct MeasureCache {
    /// Widths keyed by a hash of the text and the size bits.
    widths: HashMap<(u64, u32), CachedWidth>,
    /// The text last measured by range.
    clusters: RangeClusters,
    /// Times [`ShapedLayout::width`] ran the shaper.
    shapes: u64,
    /// Owned strings stored as cache keys. A hit must not increase this.
    lookup_allocs: u64,
    /// Monotonic use stamp. Hits and inserts advance it.
    stamp: u64,
    /// The font family captions are measured in (the theme's `subtitle.font`; `None` = the UI
    /// font, ADR-9340).
    family: Option<String>,
}

impl MeasureCache {
    /// Forgets everything; the next measures shape again.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Measures in `family` from now on (the theme's `subtitle.font`, ADR-9340). A change
    /// forgets every remembered width; returns whether the family changed.
    pub fn set_family(&mut self, family: Option<&str>) -> bool {
        if self.family.as_deref() == family {
            return false;
        }
        self.clear();
        self.family = family.map(str::to_owned);
        true
    }

    /// Widths remembered. Tests use this; a held line must not grow it.
    #[cfg(test)]
    fn cached_widths(&self) -> usize {
        self.widths.len()
    }

    /// Remembers `width` for `(text, size_px)` without shaping. Tests fill the cache to its cap.
    #[cfg(test)]
    fn insert_test_width(&mut self, text: &str, size_px: f32, width: f32) {
        self.stamp = self.stamp.wrapping_add(1);
        let used = self.stamp;
        self.widths.insert(
            width_key(text, size_px),
            CachedWidth {
                text: text.to_string(),
                width,
                used,
            },
        );
    }

    /// Drops the least recently used width so a new one fits.
    fn evict_oldest(&mut self) {
        let Some(victim) = self
            .widths
            .iter()
            .min_by_key(|(_, row)| row.used)
            .map(|(key, _)| *key)
        else {
            return;
        };
        self.widths.remove(&victim);
    }
}

/// Measures with the UI's text shaper ([`TextSystem`]), so wrapping and row rectangles follow the real
/// font. A [`MeasureCache`] remembers widths, and every range of a wrapped text comes from one shaping
/// run of the whole text: wrapping asks for many overlapping ranges of it.
pub struct ShapedLayout<'a> {
    /// The shaper.
    pub text: &'a mut TextSystem,
    /// The localization the shaper reads (a default one when the app has none).
    pub loc: &'a Localization,
    /// Remembered widths.
    pub cache: &'a mut MeasureCache,
    /// The viewport and insets.
    pub metrics: BlockMetrics,
}

fn measured_text(text: &str, size_px: f32, family: Option<&str>) -> Text {
    Text {
        spans: vec![TextSpan::lit(text).with_style(TextStyle {
            font_px: size_px,
            family: family.map(str::to_owned),
            ..TextStyle::default()
        })],
        wrap: WrapMode::None,
        ..Text::default()
    }
}

impl TextMeasure for ShapedLayout<'_> {
    fn width(&mut self, text: &str, size_px: f32, _rtl: bool) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let key = width_key(text, size_px);
        let remembered = self
            .cache
            .widths
            .get(&key)
            .and_then(|hit| (hit.text == text).then_some(hit.width));
        if let Some(width) = remembered {
            self.cache.stamp = self.cache.stamp.wrapping_add(1);
            let used = self.cache.stamp;
            if let Some(hit) = self.cache.widths.get_mut(&key) {
                hit.used = used;
            }
            return width;
        }
        let collided = self.cache.widths.contains_key(&key);
        let width = self
            .text
            .shape(
                &measured_text(text, size_px, self.cache.family.as_deref()),
                1.0,
                None,
                self.loc,
            )
            .size[0];
        self.cache.shapes = self.cache.shapes.wrapping_add(1);
        // A colliding hash keeps the stored line. The shaped width is still correct for this call.
        if collided {
            return width;
        }
        self.cache.lookup_allocs = self.cache.lookup_allocs.wrapping_add(1);
        if self.cache.widths.len() >= WIDTH_CACHE_CAP {
            self.cache.evict_oldest();
        }
        self.cache.stamp = self.cache.stamp.wrapping_add(1);
        let used = self.cache.stamp;
        self.cache.widths.insert(
            key,
            CachedWidth {
                text: text.to_string(),
                width,
                used,
            },
        );
        width
    }

    fn range_width(&mut self, text: &str, range: Range<usize>, size_px: f32, rtl: bool) -> f32 {
        if rtl {
            return self.width(&text[range], size_px, rtl);
        }
        let size_bits = size_px.to_bits();
        let held = &self.cache.clusters;
        if held.size_bits != size_bits || held.text != text {
            let family = self.cache.family.as_deref();
            let map = self
                .text
                .caret_map(&measured_text(text, size_px, family), 1.0, None, self.loc);
            self.cache.clusters = RangeClusters::new(text, size_bits, map);
        }
        match self.cache.clusters.width(&range) {
            Some(width) => width,
            None => self.width(&text[range], size_px, rtl),
        }
    }
}

impl BlockLayout for ShapedLayout<'_> {
    fn metrics(&self) -> BlockMetrics {
        self.metrics
    }
}

/// Routes a [`SubtitleLine`] of the older queue into the board: kind `Speech`, times converted to
/// microseconds, one literal piece. The caller allocates the id.
pub fn caption_from_subtitle(line: &SubtitleLine, id: CaptionId, locale: &str) -> CaptionStart {
    let micros = |seconds: f64| (seconds.max(0.0) * 1_000_000.0).round() as u64;
    CaptionStart {
        id,
        source: CaptionSource::Game("subtitle_queue".to_string()),
        kind: CaptionKind::Speech,
        locale: locale.to_string(),
        pieces: vec![CaptionPiece {
            offset_us: 0,
            text: match &line.key {
                Some(key) => CaptionText::Key {
                    key: key.clone(),
                    args: line.args.clone(),
                },
                None => CaptionText::Literal(line.text.clone()),
            },
        }],
        speaker: (!line.speaker.is_empty()).then(|| line.speaker.clone()),
        priority: line.priority.clamp(0, 255) as u8,
        start: CaptionTime::AtMicros(micros(line.start)),
        planned_us: micros(line.duration),
        keep_breaks: false,
        hint: CaptionStyleHint::default(),
    }
}

/// Resolves `Key` pieces through the localization (in the caption's own locale, then the active
/// chain) so the board only sees literal text.
pub fn resolve_pieces(start: &mut CaptionStart, loc: &Localization) {
    resolve_pieces_honoring_newlines(start, loc, false);
}

/// [`resolve_pieces`], and with `honor_newlines` on (the board policy, spec 56 § 2.8) a literal
/// newline in a translator-supplied message is kept as an author break: when a resolved `Key`
/// piece holds a `'\n'`, the caption keeps its breaks, and the newlines of its game-supplied
/// `Literal` pieces are flattened here so only the translated text breaks (ADR-7237). A caption
/// that already has `keep_breaks` is left as it is.
pub fn resolve_pieces_honoring_newlines(start: &mut CaptionStart, loc: &Localization, honor_newlines: bool) {
    // Flattening a game literal here is what the board does anyway when the caption does not keep
    // its breaks, so it changes nothing unless a translated break turns `keep_breaks` on below.
    let honor = honor_newlines && !start.keep_breaks;
    let mut translated_break = false;
    for piece in &mut start.pieces {
        match &mut piece.text {
            CaptionText::Key { key, args } => {
                let text = loc
                    .resolve_in(&start.locale, key, args)
                    .unwrap_or_else(|| loc.resolve(key, args));
                translated_break |= text.contains('\n');
                piece.text = CaptionText::Literal(text);
            }
            CaptionText::Literal(text) => {
                if honor && text.contains('\n') {
                    *text = text.replace('\n', " ");
                }
            }
        }
    }
    if honor && translated_break {
        start.keep_breaks = true;
    }
}

/// The viewport in UI pixels: the window size (the caption block's root uses
/// `ScaleMode::ConstantPhysicalSize`, so UI pixels are physical pixels).
fn viewport(scales: Option<&UiScales>) -> (f32, f32) {
    match scales {
        Some(s) if s.window_size[0] > 0.0 && s.window_size[1] > 0.0 => (s.window_size[0], s.window_size[1]),
        _ => (1920.0, 1080.0),
    }
}

/// `PostUpdate` system: updates the clock, feeds messages to the board and advances it.
pub fn caption_system(
    mut board: ResMut<CaptionBoard>,
    mut clock: ResMut<CaptionClock>,
    mut device: ResMut<CaptionDeviceClock>,
    mut ids: ResMut<CaptionIds>,
    opts: Res<CaptionOptions>,
    insets: Res<CaptionInsets>,
    cue_events: Res<CueEventMap>,
    time: Option<Res<Time>>,
    loc: Option<Res<Localization>>,
    scales: Option<Res<UiScales>>,
    text_system: Option<ResMut<TextSystem>>,
    theme: Option<Res<crate::theme_resolve::ThemeResolver>>,
    mut cache: Local<MeasureCache>,
    mut starts: MessageReader<CaptionStart>,
    mut stops: MessageReader<CaptionStop>,
    mut events: MessageReader<CaptionCueEvent>,
) {
    let _span = tracing::debug_span!("caption_system").entered();
    let sim_us = time
        .as_deref()
        .map(|t| u64::try_from(t.sim_elapsed().as_micros()).unwrap_or(u64::MAX));
    let paused = time.as_deref().is_some_and(|t| t.paused || t.is_suspended());
    clock.0 = device.step(clock.0, sim_us, paused);
    let now = clock.0;
    for mut start in starts.read().cloned() {
        if let Some(loc) = loc.as_deref() {
            resolve_pieces_honoring_newlines(&mut start, loc, board.policy.honor_newlines);
        }
        board.push(start, now, &opts);
    }
    for event in events.read() {
        let Some(def) = cue_events.0.get(&event.name) else {
            continue;
        };
        let locale = loc
            .as_deref()
            .map_or_else(|| "en".to_string(), |l| l.locale().to_string());
        let mut start = CaptionStart {
            id: ids.next_id(),
            source: CaptionSource::Game(format!("cue_event:{}", event.name)),
            kind: def.kind.into(),
            locale,
            pieces: vec![CaptionPiece {
                offset_us: 0,
                text: CaptionText::Key {
                    key: def.key.clone(),
                    args: Vec::new(),
                },
            }],
            speaker: None,
            priority: 0,
            start: CaptionTime::AtMicros(event.at_us),
            planned_us: def.planned_ms.saturating_mul(1000),
            keep_breaks: false,
            hint: CaptionStyleHint::default(),
        };
        if let Some(loc) = loc.as_deref() {
            resolve_pieces(&mut start, loc);
        }
        board.push(start, now, &opts);
    }
    for stop in stops.read() {
        board.stop(stop.id, now);
    }
    // The theme's caption font and size (spec 36 § 8.3, ADR-9340): the rows are measured and
    // laid out in them, so a change re-lays the visible pages.
    let style = theme
        .as_deref()
        .and_then(|r| r.resolved_theme())
        .map(CaptionThemeStyle::of_theme)
        .unwrap_or_default();
    cache.set_family(style.font.as_deref());
    board.set_theme_style(style.size, style.font.as_deref());
    let metrics = BlockMetrics {
        viewport: viewport(scales.as_deref()),
        inset: insets.0,
    };
    match text_system {
        Some(mut text) => {
            let fallback = Localization::default();
            let mut layout = ShapedLayout {
                text: &mut text,
                loc: loc.as_deref().unwrap_or(&fallback),
                cache: &mut cache,
                metrics,
            };
            board.advance(now, &opts, &mut layout);
        }
        None => board.advance(now, &opts, &mut FixedLayout { metrics }),
    }
}

/// Adds the caption board, its options, clock and messages to an app. Inert until a caption
/// arrives.
#[derive(Debug, Default, Clone, Copy)]
pub struct CaptionPlugin;

impl Plugin for CaptionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CaptionBoard>()
            .init_resource::<CaptionOptions>()
            .init_resource::<CaptionClock>()
            .init_resource::<CaptionDeviceClock>()
            .init_resource::<CaptionInsets>()
            .init_resource::<CueEventMap>()
            .init_resource::<CaptionIds>()
            .init_resource::<SubtitleSets>()
            .init_resource::<DialogueCaptionSettings>()
            .init_resource::<CaptionAnchors>()
            .init_resource::<CaptionAnchorState>()
            .register_type::<CaptionOptions>()
            .add_message::<CaptionStart>()
            .add_message::<CaptionStop>()
            .add_message::<CaptionCueEvent>()
            .add_systems(
                PostUpdate,
                (
                    caption_system,
                    sets::prune_subtitle_sets,
                    caption_anchor_system,
                    sync_caption_rows,
                )
                    .chain(),
            );
    }
}

#[cfg(test)]
mod tests;
