// SPDX-License-Identifier: MPL-2.0
//! Text: span model, rich-text markup, cosmic-text shaping and per-node layout
//! (`specs/18` § 2). Shaping happens in `PostUpdate` after layout so each text is shaped at its
//! final pixel size; results are cached in [`ShapedText`] and only recomputed when the text,
//! locale, font stack or node size changes.
//!
//! Font stack: the embedded default set (Inter plus per-script Noto subsets — Arabic,
//! Simplified Chinese with the full kana blocks, Korean, and color emoji; all OFL-1.1, see
//! `assets/fonts/README.md`) always loads on every platform, the system font database is
//! available on desktop unless [`TextSystem::embedded_only`] was used, and project fonts are
//! added with [`TextSystem::load_font`]. Missing glyphs fall back through cosmic-text's
//! per-script font fallback, which a bundled-first `Fallback` seeds with the embedded
//! families so
//! unpinned text covers Latin, Arabic, CJK (incl. kana) and emoji everywhere — Android
//! included, where no system fonts are loaded at all.

use std::sync::Arc;

use cosmic_text as ct;
use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use unicode_script::Script;
use unicode_segmentation::UnicodeSegmentation;

use crate::color::UiColor;
use crate::localization::Localization;
use crate::node::ComputedNode;
use crate::node::NodeStyle;

/// Where a span's string comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TextContent {
    /// Literal text.
    Lit(String),
    /// Localization key with optional Fluent arguments (`specs/18` § 6).
    Loc {
        /// The message id, e.g. `"menu-start"`.
        key: String,
        /// Fluent arguments (name → value).
        args: Vec<(String, String)>,
    },
}

impl TextContent {
    /// Resolves to the final string using `loc` (missing keys show `⟦key⟧` in dev builds).
    pub fn resolve(&self, loc: &Localization) -> String {
        match self {
            Self::Lit(s) => s.clone(),
            Self::Loc { key, args } => loc.resolve(key, args),
        }
    }
}

impl From<&str> for TextContent {
    fn from(s: &str) -> Self {
        Self::Lit(s.to_owned())
    }
}

/// The style of one text span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextStyle {
    /// Font size in UI px.
    pub font_px: f32,
    /// Text color.
    pub color: UiColor,
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Explicit font family (falls back through the font stack when glyphs are missing).
    pub family: Option<String>,
    /// Extra space after every character in em (tracking; ADR-2045). 0 = the font's own spacing.
    /// This and the other ADR-2045 fields are `.ui.ron` v4; they are left out of written text
    /// when unset, so a style without them writes the bytes v3 wrote.
    #[serde(skip_serializing_if = "is_zero_f32")]
    pub letter_spacing: f32,
    /// Explicit font weight, 1 to 1000 (400 regular, 700 bold); overrides [`Self::bold`]. Faces
    /// without that weight fall back to the nearest one the font stack has (ADR-2045).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u16>,
    /// Draw an underline (font metrics; ADR-2045).
    #[serde(skip_serializing_if = "is_false")]
    pub underline: bool,
    /// Draw a strikethrough (font metrics; ADR-2045).
    #[serde(skip_serializing_if = "is_false")]
    pub strikethrough: bool,
    /// OpenType feature settings: `"smcp"`, `"tnum"`, `"liga=0"`
    /// ([`crate::text_effects::parse_features`]; invalid entries are ignored).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Outline and shadow layers under the glyphs, front to back
    /// ([`crate::text_effects::TextEffect`]; at most
    /// [`crate::text_effects::MAX_TEXT_EFFECTS`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<crate::text_effects::TextEffect>,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font_px: 18.0,
            color: UiColor::WHITE,
            bold: false,
            italic: false,
            family: None,
            letter_spacing: 0.0,
            weight: None,
            underline: false,
            strikethrough: false,
            features: Vec::new(),
            effects: Vec::new(),
        }
    }
}

impl TextStyle {
    /// Converts to cosmic-text attributes at `scale` (UI px → physical px), never smaller than
    /// `min_px` physical px ([`TextSystem::min_font_px`]).
    ///
    /// `run` is the font run of this piece of the span ([`font_runs`]): an Arabic run with no
    /// explicit family prefers `"Noto Naskh Arabic"` (a joining Naskh face instead of a system
    /// `"Noto Sans Arabic"` fallback that often renders as disconnected isolates), and an emoji
    /// run (a character with U+FE0F) always asks for the color emoji face. `span` is stored as
    /// the glyph metadata, so a later color change can recolor glyphs without reshaping.
    ///
    /// `line_height` is the [`Text::line_height`] multiplier. cosmic-text takes each line's height from
    /// its spans' metrics, not from the buffer's, so this is what spaces the lines (ADR-5615).
    fn to_attrs(
        &self,
        scale: f32,
        min_px: f32,
        line_height: f32,
        run: FontRun,
        span: usize,
    ) -> ct::Attrs<'static> {
        let size = (self.font_px * scale).max(min_px).max(1.0);
        let metrics = ct::Metrics::new(size, size * line_height.max(0.1));
        let mut attrs = ct::Attrs::new()
            .metrics(metrics)
            .color(cosmic_color(self.color))
            .metadata(span);
        if let Some(weight) = self.weight {
            attrs = attrs.weight(ct::Weight(weight.clamp(1, 1000)));
        } else if self.bold {
            attrs = attrs.weight(ct::Weight::BOLD);
        }
        if self.italic {
            attrs = attrs.style(ct::Style::Italic);
        }
        // The ADR-2045 attributes are only set when used, so default text shapes as before.
        if self.letter_spacing != 0.0 && self.letter_spacing.is_finite() {
            attrs = attrs.letter_spacing(self.letter_spacing.clamp(-1.0, 4.0));
        }
        if !self.features.is_empty() {
            attrs = attrs.font_features(crate::text_effects::parse_features(&self.features));
        }
        if self.underline {
            attrs = attrs.underline(ct::UnderlineStyle::Single);
        }
        if self.strikethrough {
            attrs = attrs.strikethrough();
        }
        let family: Option<&'static str> = match run {
            FontRun::Emoji => Some(EMOJI_FAMILY),
            FontRun::Arabic if self.family.is_none() => Some(ARABIC_FAMILY),
            _ => self.family.as_deref().map(intern_family),
        };
        if let Some(family) = family {
            attrs = attrs.family(ct::Family::Name(family));
        }
        attrs
    }
}

/// The bundled color emoji face (also the name of the full system font on Linux and Android).
const EMOJI_FAMILY: &str = "Noto Color Emoji";
/// The bundled joining Naskh face.
const ARABIC_FAMILY: &str = "Noto Naskh Arabic";

/// cosmic-text carries color as 8-bit; store it sRGB-encoded (decoded again by
/// [`decoded_color`]) so dark text colors keep their precision.
fn cosmic_color(color: UiColor) -> ct::Color {
    let c = color.0;
    let srgb = crate::color::linear_to_srgb8;
    ct::Color::rgba(
        srgb(c[0]),
        srgb(c[1]),
        srgb(c[2]),
        (c[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
    )
}

/// The linear color a glyph draws with: the 8-bit cosmic color decoded again.
fn decoded_color(c: ct::Color) -> UiColor {
    let [r, g, b, a] = c.as_rgba();
    UiColor([
        crate::color::srgb_to_linear(r),
        crate::color::srgb_to_linear(g),
        crate::color::srgb_to_linear(b),
        f32::from(a) / 255.0,
    ])
}

/// The `'static` copy of a font family name. cosmic-text's `Attrs<'static>` holds family names by
/// reference; each distinct name is leaked once for the process (the old code leaked a fresh copy
/// on every shape call, so animated or re-laid-out text grew the heap without bound).
fn intern_family(name: &str) -> &'static str {
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let names = NAMES.get_or_init(|| Mutex::new(Vec::new()));
    // A poisoned lock only means another thread panicked mid-push; the list is still valid.
    let mut names = names.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = names.iter().copied().find(|n| *n == name) {
        return found;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.push(leaked);
    leaked
}

/// Which face a run of a span asks for (see [`font_runs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRun {
    /// The span's own family (or the default stack).
    Default,
    /// Arabic letters (and the neutrals between them): the joining Naskh face unless the span
    /// names a family.
    Arabic,
    /// One emoji presentation sequence (a character followed by U+FE0F, with its keycap, skin
    /// tone and ZWJ continuations): always the color emoji face.
    Emoji,
}

/// True for characters of the Arabic blocks (letters, Arabic-Indic digits, marks, punctuation).
fn is_arabic(c: char) -> bool {
    matches!(
        c,
        '\u{0600}'..='\u{06FF}' // Arabic
            | '\u{0750}'..='\u{077F}' // Arabic Supplement
            | '\u{08A0}'..='\u{08FF}' // Arabic Extended-A
            | '\u{FB50}'..='\u{FDFF}' // Arabic Presentation Forms-A
            | '\u{FE70}'..='\u{FEFC}' // Arabic Presentation Forms-B (U+FEFF is the BOM)
    )
}

/// Emoji modifiers (skin tones), U+1F3FB..=U+1F3FF.
fn is_emoji_modifier(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// Splits one span's text into font runs: the byte ranges that ask for the emoji face (emoji
/// presentation sequences, UTS #51: a character followed by U+FE0F, plus its skin tone,
/// keycap U+20E3, tag and ZWJ continuations), the Arabic face (Arabic-block characters) and
/// the span's own family. Neutral characters (spaces, digits, punctuation, marks, format
/// controls) stay in the run they follow, so a space between two Arabic words never splits the
/// Arabic run and the Latin words of a mixed label keep the default face. Leading neutrals join
/// the first strong run. Adjacent runs of one kind are merged; the ranges cover `s` exactly.
///
/// Why: cosmic-text picks a face per character by coverage alone, so "❤️" (U+2764 U+FE0F)
/// rendered in Inter's monochrome heart, and the old per-span rule put the Latin words of
/// "Score مرحبا" in the Arabic face too.
pub fn font_runs(s: &str) -> Vec<(std::ops::Range<usize>, FontRun)> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut kinds: Vec<Option<FontRun>> = vec![None; chars.len()];
    // Pass 1: emoji presentation sequences (always explicit).
    let mut i = 0;
    while i < chars.len() {
        let presented = chars
            .get(i + 1)
            .is_some_and(|&(_, c)| c == '\u{FE0F}' || is_emoji_modifier(c));
        if !presented || chars[i].1.is_whitespace() || chars[i].1 == '\u{200D}' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        loop {
            match chars.get(i).map(|&(_, c)| c) {
                Some('\u{FE0F}' | '\u{20E3}') => i += 1,
                Some(c) if is_emoji_modifier(c) || ('\u{E0020}'..='\u{E007F}').contains(&c) => i += 1,
                // ZWJ continues the sequence only when another character follows it.
                Some('\u{200D}') if i + 1 < chars.len() => i += 2,
                _ => break,
            }
        }
        for k in &mut kinds[start..i] {
            *k = Some(FontRun::Emoji);
        }
    }
    // Pass 2: strong characters; neutrals follow the run before them.
    let strong = |c: char| {
        if is_arabic(c) && c.is_alphabetic() {
            Some(FontRun::Arabic)
        } else if c.is_alphabetic() {
            Some(FontRun::Default)
        } else {
            None
        }
    };
    let mut current = chars
        .iter()
        .zip(&kinds)
        .find_map(|(&(_, c), k)| if k.is_none() { strong(c) } else { None })
        .unwrap_or(FontRun::Default);
    for (&(_, c), kind) in chars.iter().zip(kinds.iter_mut()) {
        if kind.is_some() {
            continue; // emoji: does not change the surrounding run
        }
        if let Some(k) = strong(c) {
            current = k;
        }
        *kind = Some(current);
    }
    let mut runs: Vec<(std::ops::Range<usize>, FontRun)> = Vec::new();
    for (n, &(at, c)) in chars.iter().enumerate() {
        let kind = kinds[n].unwrap_or(FontRun::Default);
        let end = at + c.len_utf8();
        match runs.last_mut() {
            Some((range, k)) if *k == kind => range.end = end,
            _ => runs.push((at..end, kind)),
        }
    }
    runs
}

/// Normalizes line breaks to `\n`: CR LF, a lone CR, NEL (U+0085), LINE SEPARATOR (U+2028) and
/// PARAGRAPH SEPARATOR (U+2029). cosmic-text's ASCII fast path only splits paragraphs at `\n`,
/// so a lone CR stayed in the line and drew as `.notdef`, CR LF left a stray CR before every
/// break, and U+2028 (which no bundled face carries) drew as a box instead of breaking the line.
/// `after_cr` says the previous span ended in a CR, so a leading LF here completes that CR LF.
pub fn normalize_line_breaks(s: &str, after_cr: bool) -> std::borrow::Cow<'_, str> {
    let needs = s
        .chars()
        .any(|c| matches!(c, '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
        || (after_cr && s.starts_with('\n'));
    if !needs {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    if after_cr && chars.peek() == Some(&'\n') {
        chars.next();
    }
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{0085}' | '\u{2028}' | '\u{2029}' => out.push('\n'),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Raw byte offsets of the first character of every paragraph of `s`, as the shaped buffer sees
/// it. `normalize_line_breaks` turns CR, CRLF, NEL (U+0085), U+2028 and U+2029 into `\n`, and the
/// buffer breaks paragraphs at the normalized breaks, while caret offsets index the raw string —
/// so scanning the raw string for `\n` alone misses those breaks and clusters fell back to
/// `text.len()`, past the end of the text. The rules must stay in lockstep with
/// [`normalize_line_breaks`].
fn raw_paragraph_starts(s: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut iter = s.char_indices().peekable();
    while let Some((_, c)) = iter.next() {
        let broke = match c {
            '\r' => {
                if iter.peek().is_some_and(|&(_, next)| next == '\n') {
                    iter.next();
                }
                true
            }
            '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}' => true,
            _ => false,
        };
        if broke {
            starts.push(iter.peek().map_or(s.len(), |&(at, _)| at));
        }
    }
    starts
}

/// Characters that never draw ink: C0/C1 controls and the Unicode default-ignorable code points
/// (soft hyphen, zero-width space / joiners, bidi marks, embeddings and isolates, word joiner,
/// variation selectors, BOM, tags). When no face maps one, the shaper would otherwise leave a
/// `.notdef` box where nothing should be drawn.
pub fn is_invisible_control(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF0}'..='\u{FFF8}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

/// The exact string a [`Text`] shapes: its spans resolved through `loc`, concatenated, with line
/// breaks normalized ([`normalize_line_breaks`]). [`ShapedLine::range`] indexes its paragraphs
/// ([`text_paragraphs`]).
pub fn shaped_string(text: &Text, loc: &Localization) -> String {
    let mut out = String::new();
    let mut after_cr = false;
    for span in &text.spans {
        let raw = span.content.resolve(loc);
        out.push_str(&normalize_line_breaks(&raw, after_cr));
        after_cr = raw.ends_with('\r');
    }
    out
}

/// The paragraphs of a [`shaped_string`], in order: [`ShapedLine::paragraph`] indexes this list.
/// They split where cosmic-text splits them (bidi paragraph separators, which after
/// normalization are `\n` and U+001C..=U+001E).
pub fn text_paragraphs(shaped: &str) -> Vec<&str> {
    shaped.split(['\n', '\u{1C}', '\u{1D}', '\u{1E}']).collect()
}

/// One run of equally-styled text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextSpan {
    /// The text (or localization key).
    pub content: TextContent,
    /// Span style.
    pub style: TextStyle,
}

impl TextSpan {
    /// A literal span with the default style.
    pub fn lit(text: impl Into<String>) -> Self {
        Self {
            content: TextContent::Lit(text.into()),
            style: TextStyle::default(),
        }
    }

    /// A localization span with the default style.
    pub fn loc(key: impl Into<String>) -> Self {
        Self {
            content: TextContent::Loc {
                key: key.into(),
                args: Vec::new(),
            },
            style: TextStyle::default(),
        }
    }

    /// Overrides the style.
    pub fn with_style(mut self, style: TextStyle) -> Self {
        self.style = style;
        self
    }
}

/// Line wrapping mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WrapMode {
    /// Wrap at word boundaries (glyph fallback for over-long words).
    #[default]
    Word,
    /// Wrap at any glyph.
    Glyph,
    /// No wrapping; lines extend past the box.
    None,
}

/// Behaviour when text overflows its box (`specs/18` § 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TextOverflow {
    /// Draw past the box (clip only if an ancestor clips).
    #[default]
    Visible,
    /// Hard clip at the box.
    Clip,
    /// On unwrapped text ([`WrapMode::None`]) wider than the box, replace the end of the
    /// line with `…`. Wrapped text ([`WrapMode::Word`] or [`WrapMode::Glyph`]) keeps every line.
    Ellipsis,
    /// Scale the font down until the text fits the width (a few passes; height is not considered).
    ScaleToFit,
}

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TextAlign {
    /// Leading edge in the paragraph's direction: left for LTR lines, right for RTL lines.
    #[default]
    Start,
    /// Centred.
    Center,
    /// Trailing edge in the paragraph's direction: right for LTR lines, left for RTL lines.
    End,
}

/// A text component: styled spans, alignment and wrapping (`specs/18` § 2).
///
/// # Example
///
/// ```
/// use regia_ui::{Text, TextOverflow};
///
/// let label = Text::new("Hello")
///     .with_overflow(TextOverflow::Ellipsis);
/// assert_eq!(label.spans.len(), 1);
/// assert_eq!(label.line_height, 1.2); // × font size
/// ```
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Text {
    /// Spans, in order.
    pub spans: Vec<TextSpan>,
    /// Horizontal alignment inside the node's content box.
    pub align: TextAlign,
    /// Wrapping.
    pub wrap: WrapMode,
    /// Overflow behaviour.
    pub overflow: TextOverflow,
    /// Line height multiplier (`1.0` ≈ font size; typical body text uses `1.2`).
    pub line_height: f32,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            spans: Vec::new(),
            align: TextAlign::default(),
            wrap: WrapMode::default(),
            overflow: TextOverflow::default(),
            line_height: 1.2,
        }
    }
}

impl Text {
    /// Single-span literal text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            spans: vec![TextSpan::lit(text)],
            ..Default::default()
        }
    }

    /// Overrides the style of every span.
    pub fn with_style(mut self, style: TextStyle) -> Self {
        for span in &mut self.spans {
            span.style = style.clone();
        }
        self
    }

    /// Overrides the overflow behaviour.
    pub fn with_overflow(mut self, overflow: TextOverflow) -> Self {
        self.overflow = overflow;
        self
    }

    /// Overrides wrapping.
    pub fn with_wrap(mut self, wrap: WrapMode) -> Self {
        self.wrap = wrap;
        self
    }
}

/// A shaped glyph: the swash cache key plus the integer pen position (physical px, relative to
/// the node's content-box top-left). The atlas adds the glyph bitmap's own placement offset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    /// Swash cache key (font, glyph, size, subpixel bin).
    pub cache_key: ct::CacheKey,
    /// Integer pen position where the bitmap's origin sits.
    pub pen: [i32; 2],
    /// The color this glyph was authored with (tints monochrome glyphs; color emoji keep their
    /// bitmap colors).
    pub color: UiColor,
}

/// The shaped result cached per text node.
#[derive(Component, Debug, Clone, Default)]
pub struct ShapedText {
    /// Glyphs in draw order.
    pub glyphs: Vec<PlacedGlyph>,
    /// Final text block size (physical px).
    pub size: [f32; 2],
    /// Baseline of the first line (physical px from the block top).
    pub first_baseline: f32,
    /// Content digest when shaped (spans + locale generation).
    pub digest: u64,
    /// Node width used when shaped.
    pub shaped_width: f32,
    /// UI scale when shaped.
    pub shaped_scale: f32,
    /// Whether the text was ellipsised.
    pub truncated: bool,
    /// The laid-out lines, top to bottom.
    pub lines: Vec<ShapedLine>,
    /// Index of the span each glyph of [`Self::glyphs`] came from (same length); the
    /// ellipsis glyph counts as span 0.
    pub glyph_spans: Vec<u16>,
    /// Glyphs that drew `.notdef` (a box) because no loaded face covers their character.
    /// Invisible controls never count: they are dropped instead ([`is_invisible_control`]).
    pub missing_glyphs: u32,
    /// Digest of the span colors the glyph colors were taken from; when only colors change,
    /// [`shape_text`] recolors the glyphs instead of reshaping.
    pub colors_digest: u64,
    /// Underline and strikethrough rects (ADR-2045); empty unless a span asks for them.
    pub decorations: Vec<crate::text_effects::DecorationRect>,
}

/// One laid-out line of a [`ShapedText`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ShapedLine {
    /// The paragraph it belongs to (an index into [`text_paragraphs`] of the [`shaped_string`]).
    pub paragraph: u32,
    /// Byte range of the line inside its paragraph (smallest cluster start to largest cluster
    /// end, so a bidi line still covers one logical range). Empty for an empty line.
    pub range: [u32; 2],
    /// Index of the line's first glyph in [`ShapedText::glyphs`].
    pub first_glyph: u32,
    /// Glyphs on the line.
    pub glyph_count: u32,
    /// Line width (physical px).
    pub width: f32,
    /// Baseline (physical px from the block top).
    pub baseline: f32,
    /// The paragraph runs right to left.
    pub rtl: bool,
    /// The line ends in an ellipsis.
    pub ellipsized: bool,
}

/// A font to load into the [`TextSystem`].
#[derive(Clone)]
pub struct FontSource {
    /// Font family name (informational; matching uses the font's internal name).
    pub family: String,
    /// Raw font bytes (TTF/OTF).
    pub data: Arc<[u8]>,
}

/// The shaping engine resource: one cosmic-text `FontSystem` plus the swash rasterizer cache
/// (`specs/18` § 2). Creation scans the system font database once (hundreds of ms); tests use
/// [`TextSystem::embedded_only`] to skip that.
#[derive(Resource)]
pub struct TextSystem {
    /// cosmic-text font database + shaping.
    pub font_system: ct::FontSystem,
    /// Swash glyph rasterization cache.
    pub swash: ct::SwashCache,
    /// UAX #14 class SA word segmentation (Thai, Lao, Khmer, Myanmar), cached per string
    /// (ADR-3323).
    sa: crate::line_break_sa::SaSegmenter,
    locale: String,
    default_family: String,
    /// Set by a font load or default-family switch; `sync_font_locale` clears it after bumping
    /// the shared layout/shaping generation. Kept here (not in a system `Local`) so one-shot
    /// render-out runs see the same state as the frame loop.
    fonts_changed: bool,
    /// Smallest rendered font size, physical px (0 = no floor). Set from
    /// [`crate::scale::UiDevicePolicy::min_text_px`]; 12 on handhelds so text stays legible at 7".
    pub min_font_px: f32,
    /// Multiplier on every span's `font_px` (the player's text size, see
    /// [`crate::scale::UiAccessibility::text_scale`]); `1.0` = as authored. The floor
    /// [`Self::min_font_px`] still applies after it.
    pub text_scale: f32,
}

impl Default for TextSystem {
    fn default() -> Self {
        Self::with_system_fonts()
    }
}

impl TextSystem {
    /// A `TextSystem` with only the embedded default set (Inter + per-script Noto subsets;
    /// no system font scan). Fast, and identical on every platform: the Android default.
    pub fn embedded_only() -> Self {
        let mut db = ct::fontdb::Database::new();
        load_embedded(&mut db);
        Self::with_db(db)
    }

    /// A `TextSystem` over the desktop system font database plus the embedded default set.
    ///
    /// System font files the font database cannot parse are skipped. Its per-file warnings
    /// (which name a user's font paths) are counted instead of logged: one `debug` line per
    /// process names how many were skipped (v0.5.1, [`count_font_scan_warnings`]).
    pub fn with_system_fonts() -> Self {
        let mut db = ct::fontdb::Database::new();
        let ((), skipped) = count_font_scan_warnings(|| db.load_system_fonts());
        note_skipped_system_fonts(skipped);
        load_embedded(&mut db);
        Self::with_db(db)
    }

    fn with_db(mut db: ct::fontdb::Database) -> Self {
        // The sans-serif default is the bundled Inter: fontdb's implicit default is "Arial",
        // which most machines lack, so without this the fallback walk would settle on a
        // machine-dependent face (DejaVu here, "any font" elsewhere).
        db.set_sans_serif_family("Inter");
        let locale = regia_platform::system_locale().unwrap_or_else(|| "en".to_owned());
        Self {
            font_system: ct::FontSystem::new_with_locale_and_db_and_fallback(
                locale.clone(),
                db,
                BundledFallback,
            ),
            swash: ct::SwashCache::new(),
            sa: crate::line_break_sa::SaSegmenter::new(),
            locale,
            default_family: "Inter".to_owned(),
            fonts_changed: false,
            min_font_px: 0.0,
            text_scale: 1.0,
        }
    }

    /// Loads a font from bytes (project fonts / `Font` assets).
    pub fn load_font(&mut self, data: Arc<[u8]>) {
        self.font_system.db_mut().load_font_data(data.to_vec());
        self.fonts_changed = true;
    }

    /// The family used for spans without an explicit family (initially `Inter`). Script
    /// fallback remains active, including the bundled Arabic joining face.
    #[must_use]
    pub fn default_family(&self) -> &str {
        &self.default_family
    }

    /// Selects a loaded family as the default, or restores embedded `Inter` with `None`.
    /// Returns whether it changed. An unknown or empty name is rejected without mutation;
    /// load project font bytes with [`Self::load_font`] first. Explicit span families keep
    /// their meaning. `UiPlugin` invalidates layout and shaping before its next layout pass.
    pub fn set_default_family(&mut self, family: Option<&str>) -> Result<bool, DefaultFontError> {
        let requested = family.unwrap_or("Inter");
        let canonical = self
            .font_system
            .db()
            .faces()
            .flat_map(|face| &face.families)
            .find(|(name, _)| name.eq_ignore_ascii_case(requested))
            .map(|(name, _)| name.clone())
            .ok_or_else(|| DefaultFontError::Unavailable(requested.to_owned()))?;
        if self.default_family == canonical {
            return Ok(false);
        }
        let placeholder = ct::FontSystem::new_with_locale_and_db_and_fallback(
            String::new(),
            ct::fontdb::Database::new(),
            BundledFallback,
        );
        let (_, mut db) = std::mem::replace(&mut self.font_system, placeholder).into_locale_and_db();
        db.set_sans_serif_family(&canonical);
        self.font_system =
            ct::FontSystem::new_with_locale_and_db_and_fallback(self.locale.clone(), db, BundledFallback);
        self.default_family = canonical;
        self.fonts_changed = true;
        Ok(true)
    }

    /// The locale used for font fallback (e.g. Han unification).
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// Switches the font-fallback locale (the game's locale, [`sync_font_locale`]). Returns `true`
    /// when it changed; an empty tag or the current one changes nothing. The font database and
    /// every loaded font are kept; the shaping caches and fallback lists are rebuilt, so text
    /// shaped before the change must be shaped again (a locale switch bumps the localization
    /// generation, which does that).
    pub fn set_locale(&mut self, locale: &str) -> bool {
        if locale.is_empty() || self.locale == locale {
            return false;
        }
        let placeholder = ct::FontSystem::new_with_locale_and_db_and_fallback(
            String::new(),
            ct::fontdb::Database::new(),
            BundledFallback,
        );
        let (_, db) = std::mem::replace(&mut self.font_system, placeholder).into_locale_and_db();
        self.font_system =
            ct::FontSystem::new_with_locale_and_db_and_fallback(locale.to_owned(), db, BundledFallback);
        self.locale = locale.to_owned();
        true
    }

    /// Rasterizes one glyph bitmap (monochrome mask or color emoji) on demand.
    pub fn glyph_image(&mut self, cache_key: ct::CacheKey) -> Option<ct::SwashImage> {
        self.swash.get_image(&mut self.font_system, cache_key).clone()
    }

    /// Shapes `text` into placed glyphs at `scale` (UI px → physical px), wrapped to at most
    /// `max_width` physical px. Re-shaping the same inputs is deterministic.
    pub fn shape(
        &mut self,
        text: &Text,
        scale: f32,
        max_width: Option<f32>,
        loc: &Localization,
    ) -> ShapedText {
        // The text size setting multiplies font sizes only; `shaped_scale` stays the layout scale
        // so the reuse checks keep comparing like with like.
        let layout_scale = scale;
        let scale = scale * self.text_scale;
        // ScaleToFit: shape once naturally, then retry at a reduced size.
        if text.overflow == TextOverflow::ScaleToFit
            && let Some(max_w) = max_width
        {
            let mut natural = self.shape_once(text, scale, None, loc, false);
            // Width only. Font-size rounding can leave a retry slightly wide, so retry a few
            // times and always keep the narrowest result seen.
            for _ in 0..3 {
                if natural.size[0] <= max_w || natural.size[0] <= 0.0 {
                    break;
                }
                let fit = scale * (max_w / natural.size[0]) * 0.99;
                let reshaped = self.shape_once(text, fit, None, loc, false);
                if reshaped.size[0] < natural.size[0] {
                    natural = reshaped;
                } else {
                    break;
                }
            }
            natural.shaped_width = max_w;
            natural.shaped_scale = layout_scale;
            return natural;
        }
        let ellipsize = text.overflow == TextOverflow::Ellipsis;
        let mut shaped = self.shape_once(text, scale, max_width, loc, ellipsize);
        // Layout rounds a node sized to its text to whole pixels, which can leave the box up to
        // `WRAP_ROUNDING_SLACK_PX` narrower than the text's natural width. Wrapping there broke a
        // one-line label ("Tiny" at 8 px: 16.2 px wide in a 16 px box became "Tin" over "y").
        // A wrapped result whose natural width fits the box plus that rounding is shaped on one
        // line instead (ADR-359). The check costs one more shape, for multi-line results only.
        if !ellipsize
            && text.wrap != WrapMode::None
            && let Some(max_w) = max_width
            && is_multi_line(&shaped)
        {
            let natural = self.shape_once(text, scale, None, loc, false);
            if natural.size[0] > max_w && natural.size[0] <= max_w + WRAP_ROUNDING_SLACK_PX {
                // A hair over the natural width, so float rounding cannot wrap the line again.
                shaped = self.shape_once(text, scale, Some(natural.size[0] + 0.01), loc, false);
                shaped.shaped_width = max_w;
            }
        }
        shaped.shaped_scale = layout_scale;
        shaped
    }

    /// Lays `text` out in a cosmic-text buffer exactly as [`TextSystem::shape`] does (same metrics,
    /// wrapping, attributes, line-break normalization, font runs and alignment), so caret maps and
    /// drawn glyphs agree. Returns the buffer, the first span's font size in physical px, the
    /// [`LayoutMap`] between the buffer's marked coordinates and the source string, and the
    /// marked-concat offset at each paragraph start (SA spans get U+200B break markers;
    /// [`LayoutMap`] and the paragraph starts are trivial for texts without SA).
    fn layout_buffer(
        &mut self,
        text: &Text,
        scale: f32,
        max_width: Option<f32>,
        loc: &Localization,
        ellipsize: bool,
    ) -> (ct::Buffer, f32, LayoutMap, Vec<u32>) {
        let first_px = (text.spans.first().map_or(18.0, |s| s.style.font_px) * scale).max(self.min_font_px);
        let line_h = first_px * text.line_height.max(0.1);
        let mut buffer = ct::Buffer::new_empty(ct::Metrics::new(first_px, line_h));
        buffer.set_size(max_width, None);
        buffer.set_wrap(match text.wrap {
            WrapMode::Word => ct::Wrap::WordOrGlyph,
            WrapMode::Glyph => ct::Wrap::Glyph,
            WrapMode::None => ct::Wrap::None,
        });
        // cosmic-text's own ellipsis works on clusters in logical order and places "…" on the
        // visual end of the line, so RTL and mixed-direction lines truncate correctly and no
        // ligature, ZWJ emoji or base + mark cluster is ever split (the old pass dropped single
        // glyphs by pen x and left RTL lines with no ellipsis at all). Wrapped text keeps every
        // line, as before: the ellipsis applies to unwrapped text wider than the box.
        if ellipsize && text.wrap == WrapMode::None {
            buffer.set_ellipsize(ct::Ellipsize::End(ct::EllipsizeHeightLimit::Lines(1)));
        }
        let default_attrs = ct::Attrs::new().metrics(ct::Metrics::new(first_px, line_h));
        let mut pieces: Vec<(String, ct::Attrs<'static>)> = Vec::new();
        let mut map = LayoutMap {
            marked_starts: vec![0],
            norm_starts: vec![0],
            raw_starts: vec![0],
            spans: Vec::with_capacity(text.spans.len()),
        };
        let mut par_starts: Vec<u32> = vec![0];
        let mut after_cr = false;
        for (index, span) in text.spans.iter().enumerate() {
            let raw = span.content.resolve(loc);
            // SA path: segment the normalized span and insert break markers, keeping a map back
            // to the raw string. Other spans go through unchanged.
            let (marked, span_map, norm_len) = if crate::line_break_sa::has_sa(&raw) {
                let (norm, norm_to_raw) = normalize_line_breaks_mapped(&raw, after_cr);
                let (marked, marked_to_norm) = {
                    let breaks = self.sa.breaks(&norm);
                    mark_sa_breaks(&norm, breaks)
                };
                let marked_len = u32::try_from(marked.len()).unwrap_or(u32::MAX);
                (
                    marked,
                    Some(SpanMap {
                        marked_len,
                        marked_to_norm,
                        norm_to_raw,
                    }),
                    u32::try_from(norm.len()).unwrap_or(u32::MAX),
                )
            } else {
                let normalized = normalize_line_breaks(&raw, after_cr);
                let len = u32::try_from(normalized.len()).unwrap_or(u32::MAX);
                (normalized.into_owned(), None, len)
            };
            after_cr = raw.ends_with('\r');
            map.raw_starts
                .push(map.raw_starts[index] + u32::try_from(raw.len()).unwrap_or(u32::MAX));
            map.norm_starts.push(map.norm_starts[index] + norm_len);
            map.marked_starts
                .push(map.marked_starts[index] + span_map.as_ref().map_or(norm_len, |m| m.marked_len));
            map.spans.push(span_map);
            for (i, _) in marked.match_indices('\n') {
                par_starts.push(map.marked_starts[index] + u32::try_from(i + 1).unwrap_or(u32::MAX));
            }
            for (range, run) in font_runs(&marked) {
                let attrs = span
                    .style
                    .to_attrs(scale, self.min_font_px, text.line_height, run, index);
                pieces.push((marked[range].to_owned(), attrs));
            }
        }
        let cosmic_spans: Vec<(&str, ct::Attrs<'_>)> =
            pieces.iter().map(|(s, a)| (s.as_str(), a.clone())).collect();
        // Advanced shaping (harfrust) is required for Arabic contextual joins. Alignment: `Start`
        // is direction-relative, so it passes no explicit align and lets cosmic-text pick
        // `Align::Right` for RTL lines and `Align::Left` for LTR lines, per line; `End` maps to
        // cosmic's own trailing-edge `Align::End` (right for LTR, left for RTL).
        buffer.set_rich_text(
            cosmic_spans,
            &default_attrs,
            ct::Shaping::Advanced,
            match text.align {
                TextAlign::Start => None,
                TextAlign::End => Some(ct::Align::End),
                TextAlign::Center => Some(ct::Align::Center),
            },
        );
        buffer.shape_until_scroll(&mut self.font_system, false);
        if text.wrap == WrapMode::None && !ellipsize && no_wrap_drops_a_leading_run(&buffer) {
            // cosmic-text 0.19's no-wrap layout drops the first word of a paragraph whose first
            // bidi run goes against the paragraph (a number opening a right-to-left paragraph:
            // `123 مرحبا` lays out as ` مرحبا`, ADR-9340). Such a text takes the wrapping layout
            // at a width it cannot wrap at: its natural width, or the box when that is wider, so
            // a text that fits is aligned in the box exactly as the no-wrap layout aligns it.
            buffer.set_wrap(ct::Wrap::WordOrGlyph);
            buffer.set_size(None, None);
            buffer.shape_until_scroll(&mut self.font_system, false);
            if let Some(box_w) = max_width {
                let natural = buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
                buffer.set_size(Some(box_w.max(natural)), None);
                buffer.shape_until_scroll(&mut self.font_system, false);
            }
        }
        (buffer, first_px, map, par_starts)
    }

    fn shape_once(
        &mut self,
        text: &Text,
        scale: f32,
        max_width: Option<f32>,
        loc: &Localization,
        ellipsize: bool,
    ) -> ShapedText {
        let (buffer, _, map, par_starts) = self.layout_buffer(text, scale, max_width, loc, ellipsize);
        // Line ranges index the normalized (shaped) string; glyphs come back in marked buffer
        // coordinates when an SA span carried break markers (ADR-3323).
        let translate = |rel: usize, line_i: usize| -> u32 {
            if !map.has_sa() {
                return u32::try_from(rel).unwrap_or(u32::MAX);
            }
            let par = par_starts.get(line_i).copied().unwrap_or(0);
            map.to_norm_rel(par, u32::try_from(rel).unwrap_or(u32::MAX))
        };

        let mut glyphs: Vec<PlacedGlyph> = Vec::new();
        let mut glyph_spans: Vec<u16> = Vec::new();
        let mut lines: Vec<ShapedLine> = Vec::new();
        let mut size = [0.0f32; 2];
        let mut first_baseline = 0.0f32;
        let mut missing_glyphs = 0u32;
        let mut truncated = false;
        let mut decorations = Vec::new();
        let snap = cfg!(target_os = "android"); // sub-pixel positioning off on mobile
        let fallback_color = text.spans.first().map(|s| s.style.color).unwrap_or_default();
        for run in buffer.layout_runs() {
            if !run.decorations.is_empty() {
                let line_y = if snap { run.line_y.round() } else { run.line_y };
                crate::text_effects::decoration_rects(
                    &run,
                    line_y,
                    |span| text.spans.get(span).map_or(fallback_color, |s| s.style.color),
                    decoded_color,
                    &mut decorations,
                );
            }
            size[0] = size[0].max(run.line_w);
            size[1] = run.line_top + run.line_height;
            if lines.is_empty() {
                first_baseline = run.line_y;
            }
            let mut line = ShapedLine {
                paragraph: u32::try_from(run.line_i).unwrap_or(u32::MAX),
                range: [u32::MAX, 0],
                first_glyph: u32::try_from(glyphs.len()).unwrap_or(u32::MAX),
                glyph_count: 0,
                width: run.line_w,
                baseline: run.line_y,
                rtl: run.rtl,
                ellipsized: false,
            };
            for glyph in run.glyphs {
                // cosmic-text points the ellipsis glyph's cluster at the elision boundary
                // (start == end); every other cluster covers at least one character.
                let ellipsis = ellipsize && glyph.start == glyph.end;
                let cluster = run.text.get(glyph.start..glyph.end).unwrap_or("");
                if glyph.glyph_id == 0 && !cluster.is_empty() {
                    if cluster.chars().all(is_invisible_control) {
                        continue; // a format control no face maps: nothing to draw
                    }
                    missing_glyphs += 1;
                }
                if ellipsis {
                    truncated = true;
                    line.ellipsized = true;
                } else {
                    line.range[0] = line.range[0].min(translate(glyph.start, run.line_i));
                    line.range[1] = line.range[1].max(translate(glyph.end, run.line_i));
                }
                // Baseline is `run.line_y` (cosmic-text Buffer::draw); omitting it places ink at y≈0
                // and clips glyphs against the top of the layout box.
                let offset = if snap {
                    (0.0_f32, run.line_y.round())
                } else {
                    (0.0, run.line_y)
                };
                let physical = glyph.physical(offset, 1.0);
                let color = glyph.color_opt.map_or(fallback_color, decoded_color);
                glyphs.push(PlacedGlyph {
                    cache_key: physical.cache_key,
                    pen: [physical.x, physical.y],
                    color,
                });
                glyph_spans.push(u16::try_from(glyph.metadata).unwrap_or(u16::MAX));
                line.glyph_count += 1;
            }
            if line.range[0] > line.range[1] {
                line.range = [0, 0];
            }
            lines.push(line);
        }
        if truncated && let Some(max_w) = max_width {
            size[0] = size[0].min(max_w);
        }
        ShapedText {
            glyphs,
            size,
            first_baseline,
            digest: 0,
            shaped_width: max_width.unwrap_or(0.0),
            shaped_scale: scale,
            truncated,
            lines,
            glyph_spans,
            missing_glyphs,
            colors_digest: colors_digest(text),
            decorations,
        }
    }
}

impl ShapedText {
    /// Recolors the glyphs to `text`'s current span colors without reshaping: each glyph takes
    /// the color of the span it came from ([`Self::glyph_spans`]). `self` must be the shape of
    /// `text`. Returns whether anything changed; [`shape_text`] calls it when only colors changed
    /// since the last shape (before, a color edit on a `Text` never reached the screen until
    /// something else forced a reshape).
    pub fn recolor(&mut self, text: &Text) -> bool {
        let digest = colors_digest(text);
        if self.colors_digest == digest {
            return false;
        }
        self.colors_digest = digest;
        let first = text.spans.first().map(|s| s.style.color).unwrap_or_default();
        for (glyph, span) in self.glyphs.iter_mut().zip(&self.glyph_spans) {
            let color = text
                .spans
                .get(usize::from(*span))
                .map_or(first, |s| s.style.color);
            glyph.color = decoded_color(cosmic_color(color));
        }
        for deco in &mut self.decorations {
            let color = text
                .spans
                .get(usize::from(deco.span))
                .map_or(first, |s| s.style.color);
            deco.color = decoded_color(cosmic_color(color));
        }
        true
    }
}

/// How far (physical px) layout's pixel rounding can leave a node narrower than the text it was
/// sized to: taffy rounds both edges of a box, so its width can lose up to one pixel.
const WRAP_ROUNDING_SLACK_PX: f32 = 1.0;

/// Whether shaped glyphs span more than one line (lines are laid out top to bottom, so the first
/// and last glyph share a baseline only on a single line).
fn is_multi_line(shaped: &ShapedText) -> bool {
    shaped
        .glyphs
        .first()
        .zip(shaped.glyphs.last())
        .is_some_and(|(first, last)| first.pen[1] != last.pen[1])
}

/// Digest of a text's span colors (glyph colors follow it; see [`ShapedText::recolor`]).
fn colors_digest(text: &Text) -> u64 {
    let mut h = fnv_offset();
    for span in &text.spans {
        for channel in span.style.color.0 {
            h = fnv_write_u32(h, channel.to_bits());
        }
    }
    h
}

/// One shaped glyph cluster: the bytes it covers and its horizontal extent (physical px).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CaretCluster {
    /// First byte of the cluster in [`CaretMap::text`].
    pub start: usize,
    /// One past the cluster's last byte.
    pub end: usize,
    /// Left edge.
    pub x0: f32,
    /// Right edge.
    pub x1: f32,
    /// Whether the cluster runs right to left (its leading edge is `x1`).
    pub rtl: bool,
}

impl CaretCluster {
    /// The edge where the cluster's text begins.
    fn leading(&self) -> f32 {
        if self.rtl { self.x1 } else { self.x0 }
    }

    /// The edge where the cluster's text ends.
    fn trailing(&self) -> f32 {
        if self.rtl { self.x0 } else { self.x1 }
    }
}

/// One visual line of a [`CaretMap`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaretLine {
    /// Top of the line box (physical px from the text block's top).
    pub top: f32,
    /// Height of the line box.
    pub height: f32,
    /// First byte on the line.
    pub start: usize,
    /// One past the last byte on the line (a line break is not included).
    pub end: usize,
    /// Caret x on an empty line.
    pub origin_x: f32,
    /// Whether the line's paragraph runs right to left: the paragraph's base level, which the
    /// shaper resolves by UAX #9 rules P2 and P3 over the whole paragraph (the first strong
    /// character decides; digits and other weak characters do not), so every visual line of a
    /// wrapped paragraph carries it (ADR-9340).
    pub rtl: bool,
    /// Clusters in the order the shaper laid them out: left to right in a left-to-right run, but
    /// a right-to-left run's clusters may come in logical order (sort by `x0` for screen order,
    /// as [`CaretMap::visual_stops`] does).
    pub clusters: Vec<CaretCluster>,
}

/// Where carets go in a shaped text and which byte a point hits (ADR-336). Built by
/// [`TextSystem::caret_map`] with the same layout the glyphs are drawn with; coordinates are
/// physical px relative to the shaped block's top-left, the space of [`PlacedGlyph::pen`].
///
/// Byte offsets index [`CaretMap::text`], the spans concatenated. Lines break at `\n` only
/// (a carriage return is not treated as a break). Carets inside a ligature are placed by
/// splitting the cluster evenly between its grapheme clusters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaretMap {
    /// The laid-out text.
    pub text: String,
    /// Visual lines, top to bottom.
    pub lines: Vec<CaretLine>,
}

impl CaretMap {
    /// The visual line `byte` sits on: the line that contains it, else the line that ends at it.
    pub fn line_of(&self, byte: usize) -> usize {
        let mut ends_here = None;
        let mut before = 0;
        for (i, line) in self.lines.iter().enumerate() {
            if line.start <= byte && byte < line.end {
                return i;
            }
            if line.end == byte && ends_here.is_none() {
                ends_here = Some(i);
            }
            if line.start <= byte {
                before = i;
            }
        }
        ends_here.unwrap_or(before)
    }

    /// Caret position for `byte`: `[x, top, height]`.
    pub fn caret(&self, byte: usize) -> [f32; 3] {
        let Some(line) = self.lines.get(self.line_of(byte)) else {
            return [0.0; 3];
        };
        let x = if let Some(c) = line.clusters.iter().find(|c| c.start <= byte && byte < c.end) {
            let (k, n) = self.grapheme_split(c, byte);
            c.leading() + (c.trailing() - c.leading()) * (k as f32 / n.max(1) as f32)
        } else if let Some(c) = line.clusters.iter().find(|c| c.end == byte) {
            c.trailing()
        } else if let Some(c) = line.clusters.iter().find(|c| c.start == byte) {
            c.leading()
        } else {
            line.origin_x
        };
        [x, line.top, line.height]
    }

    /// The byte offset nearest to `p` (a caret boundary).
    pub fn hit(&self, p: [f32; 2]) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let index = self
            .lines
            .iter()
            .position(|l| p[1] < l.top + l.height)
            .unwrap_or(self.lines.len() - 1);
        self.hit_line(index, p[0])
    }

    /// The byte offset on line `index` nearest to `x`.
    pub fn hit_line(&self, index: usize, x: f32) -> usize {
        let Some(line) = self.lines.get(index) else {
            return self.text.len();
        };
        let distance = |c: &CaretCluster| {
            if x < c.x0 {
                c.x0 - x
            } else if x > c.x1 {
                x - c.x1
            } else {
                0.0
            }
        };
        let Some(c) = line
            .clusters
            .iter()
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        else {
            return line.start;
        };
        let width = (c.x1 - c.x0).max(f32::EPSILON);
        let f = ((x - c.x0) / width).clamp(0.0, 1.0);
        let reading = if c.rtl { 1.0 - f } else { f };
        let bounds = self.grapheme_bounds(c);
        let n = bounds.len().saturating_sub(1).max(1);
        let k = (reading * n as f32).round() as usize;
        bounds.get(k).copied().unwrap_or(c.end)
    }

    /// Height of the whole block.
    pub fn height(&self) -> f32 {
        self.lines.last().map_or(0.0, |l| l.top + l.height)
    }

    /// Width of the widest line.
    pub fn width(&self) -> f32 {
        self.lines
            .iter()
            .flat_map(|l| l.clusters.iter().map(|c| c.x1))
            .fold(0.0, f32::max)
    }

    /// Grapheme boundaries inside a cluster, `start` and `end` included.
    fn grapheme_bounds(&self, c: &CaretCluster) -> Vec<usize> {
        let Some(s) = self.text.get(c.start..c.end) else {
            return vec![c.start, c.end];
        };
        let mut bounds: Vec<usize> = s.grapheme_indices(true).map(|(i, _)| c.start + i).collect();
        bounds.push(c.end);
        if bounds.first() != Some(&c.start) {
            bounds.insert(0, c.start);
        }
        bounds
    }

    /// `(graphemes before byte, graphemes in cluster)` for a byte inside `c`.
    fn grapheme_split(&self, c: &CaretCluster, byte: usize) -> (usize, usize) {
        let bounds = self.grapheme_bounds(c);
        let n = bounds.len().saturating_sub(1);
        let k = bounds.iter().filter(|b| **b <= byte).count().saturating_sub(1);
        (k, n)
    }

    /// Whether line `index` belongs to a right-to-left paragraph: [`CaretLine::rtl`], the
    /// paragraph's base level by UAX #9 rules P2 and P3. A line that opens with a number inside a
    /// right-to-left paragraph is right to left (the number's own run is not; ADR-9340).
    pub fn line_rtl(&self, index: usize) -> bool {
        self.lines.get(index).is_some_and(|l| l.rtl)
    }

    /// The caret stops of line `index` in visual order, left to right, as `(x, byte)` (ADR-7208).
    ///
    /// Each cluster contributes its grapheme boundaries, ordered by x (a right-to-left cluster's
    /// bytes therefore descend). Where two runs meet, both offer a stop at the same x (the end of
    /// one run and the start or end of the other); the stop of the run that has the paragraph's
    /// direction is kept, so every screen position is one stop and logical offsets on the
    /// boundary keep the paragraph's reading order. Edge cases of that dedup: the other run's
    /// boundary byte has no stop of its own, so a caret there resolves through the
    /// nearest-stop fallback of [`CaretMap::visual_step`] and can jump two stops; zero-width
    /// cluster stops (combining marks) collapse to one; and the dedup tolerance (0.5 px) differs
    /// from `caret_near`'s hint match (0.75 px). Documented in the `ui-text-input` agent page.
    ///
    /// The line's two edges belong to its logical ends (ADR-9340): the visual start (left in a
    /// left-to-right paragraph, right in a right-to-left one) is [`CaretLine::start`] and the
    /// visual end is [`CaretLine::end`], even where a run of the other direction touches the
    /// edge. That is where Home and End draw the caret, and a byte the edge takes over keeps its
    /// stop at the run's other side.
    pub fn visual_stops(&self, index: usize) -> Vec<(f32, usize)> {
        let Some(line) = self.lines.get(index) else {
            return Vec::new();
        };
        if line.clusters.is_empty() {
            return vec![(line.origin_x, line.start)];
        }
        let para_rtl = self.line_rtl(index);
        // Every grapheme boundary of every cluster with its x. The clusters of a right-to-left
        // run may come in logical order, so the stops are sorted by x rather than taken in
        // cluster order (stable: ties keep the clusters' order).
        let mut all: Vec<(f32, usize, bool)> = Vec::new();
        for c in &line.clusters {
            let bounds = self.grapheme_bounds(c);
            let n = bounds.len().saturating_sub(1).max(1);
            for k in 0..=n {
                let x = c.leading() + (c.trailing() - c.leading()) * (k as f32 / n as f32);
                all.push((x, bounds.get(k).copied().unwrap_or(c.end), c.rtl));
            }
        }
        all.sort_by(|a, b| a.0.total_cmp(&b.0));
        // One stop per screen position: where two runs meet, the paragraph direction's run wins.
        let mut stops: Vec<(f32, usize, bool)> = Vec::with_capacity(all.len());
        for s in all {
            match stops.last_mut() {
                Some(last) if (last.0 - s.0).abs() < 0.5 => {
                    if last.1 != s.1 && last.2 != para_rtl && s.2 == para_rtl {
                        *last = s;
                    }
                }
                _ => stops.push(s),
            }
        }
        let (left, right) = if para_rtl {
            (line.end, line.start)
        } else {
            (line.start, line.end)
        };
        if let Some(first) = stops.first_mut() {
            first.1 = left;
        }
        if stops.len() > 1
            && let Some(last) = stops.last_mut()
        {
            last.1 = right;
        }
        stops.into_iter().map(|(x, b, _)| (x, b)).collect()
    }

    /// The index in `stops` of the caret drawn at `x` for `byte`: the stop at `byte` nearest to
    /// `x`, else the stop nearest to `x`.
    fn stop_index(stops: &[(f32, usize)], byte: usize, x: f32) -> Option<usize> {
        let distance = |s: &(f32, usize)| (s.0 - x).abs() + if s.1 == byte { 0.0 } else { 1.0e6 };
        stops
            .iter()
            .enumerate()
            .min_by(|a, b| distance(a.1).total_cmp(&distance(b.1)))
            .map(|(i, _)| i)
    }

    /// The grapheme clusters of `line` on screen, as `(left x, right x, first byte)`.
    fn visual_graphemes(&self, line: &CaretLine) -> Vec<(f32, f32, usize)> {
        let mut out = Vec::new();
        for c in &line.clusters {
            let bounds = self.grapheme_bounds(c);
            let n = bounds.len().saturating_sub(1).max(1);
            let at = |k: usize| c.leading() + (c.trailing() - c.leading()) * (k as f32 / n as f32);
            for k in 0..n {
                let (a, b) = (at(k), at(k + 1));
                out.push((a.min(b), a.max(b), bounds.get(k).copied().unwrap_or(c.start)));
            }
        }
        out
    }

    /// One visual word step from the caret drawn at `x` for `byte` (Ctrl+Left / Ctrl+Right,
    /// ADR-9340): to the right, the next stop with a word on its left and not the same word on
    /// its right (a word's right edge on screen); to the left, the next stop with a word on its
    /// right and not the same word on its left (a word's left edge). Words are the UAX #29 word
    /// segments that hold a letter or digit, as the logical word moves use. The line's edge stops
    /// a step; `None` when the caret is already at the edge in that direction.
    ///
    /// In left-to-right text this is the end of the next word / the start of the previous word;
    /// in right-to-left text the same screen edges are the start of the previous word / the end
    /// of the next word, and in mixed text the step follows the screen.
    pub fn visual_word_step(&self, byte: usize, x: f32, right: bool) -> Option<(f32, usize)> {
        let index = self.line_of(byte);
        let line = self.lines.get(index)?;
        let stops = self.visual_stops(index);
        let last = stops.len().checked_sub(1)?;
        let mut j = Self::stop_index(&stops, byte, x)?;
        let graphemes = self.visual_graphemes(line);
        let words = crate::text_edit::word_ranges(&self.text);
        let word_of = |b: usize| words.iter().position(|(a, e)| *a <= b && b < *e);
        // The word of the grapheme whose left (or right) edge is at `sx`.
        let word_at = |sx: f32, left_edge: bool| {
            graphemes
                .iter()
                .find(|g| {
                    let edge = if left_edge { g.0 } else { g.1 };
                    (edge - sx).abs() < 0.5
                })
                .and_then(|g| word_of(g.2))
        };
        loop {
            j = if right {
                if j >= last {
                    return None;
                }
                j + 1
            } else {
                j.checked_sub(1)?
            };
            let (sx, _) = *stops.get(j)?;
            if j == 0 || j == last {
                return stops.get(j).copied();
            }
            let (on_left, on_right) = (word_at(sx, false), word_at(sx, true));
            let edge = if right {
                on_left.is_some() && on_left != on_right
            } else {
                on_right.is_some() && on_right != on_left
            };
            if edge {
                return stops.get(j).copied();
            }
        }
    }

    /// The caret for `byte` drawn at `hint_x` when `hint_x` is one of `byte`'s visual stops (a
    /// boundary offset between two runs has two screen positions; a visual move picks one),
    /// else [`CaretMap::caret`]. `[x, top, height]`.
    pub fn caret_near(&self, byte: usize, hint_x: Option<f32>) -> [f32; 3] {
        let plain = self.caret(byte);
        let Some(h) = hint_x else {
            return plain;
        };
        let index = self.line_of(byte);
        let hit = self
            .visual_stops(index)
            .into_iter()
            .any(|(x, b)| b == byte && (x - h).abs() < 0.75);
        if hit { [h, plain[1], plain[2]] } else { plain }
    }

    /// One visual step from the caret drawn at `x` for `byte`: the neighboring stop to the right
    /// (or left) on the same line as `(x, byte)`, `None` at the line's edge.
    pub fn visual_step(&self, byte: usize, x: f32, right: bool) -> Option<(f32, usize)> {
        let stops = self.visual_stops(self.line_of(byte));
        let i = Self::stop_index(&stops, byte, x)?;
        let j = if right {
            i.checked_add(1)?
        } else {
            i.checked_sub(1)?
        };
        stops.get(j).copied()
    }
}

/// Per-span offset translation for one SA-script span (ADR-3323). The buffer (shaping input)
/// holds the *marked* string — the span's normalized text with U+200B break markers inserted —
/// while line ranges index the *normalized* string and carets index the *raw* (resolved) string.
/// Each table is a sorted delta list: for offsets at or past `offset`, the translated offset is
/// `off + extra`.
#[derive(Debug)]
struct SpanMap {
    /// Length of the marked string in the buffer.
    marked_len: u32,
    /// (marked offset just after an inserted marker, extra ≤ 0): `norm = marked + extra`.
    marked_to_norm: Vec<(u32, i32)>,
    /// (norm offset, extra ≥ 0): `raw = norm + extra`; non-empty only when the span's raw text
    /// holds CR/NEL/LS/PS that normalization rewrote.
    norm_to_raw: Vec<(u32, i32)>,
}

impl SpanMap {
    /// `norm` offset of a `marked` char boundary inside this span.
    fn marked_to_norm(&self, marked: u32) -> u32 {
        (i64::from(marked) + i64::from(apply_deltas(&self.marked_to_norm, marked))) as u32
    }

    /// `raw` offset of a `norm` char boundary inside this span.
    fn norm_to_raw(&self, norm: u32) -> u32 {
        (i64::from(norm) + i64::from(apply_deltas(&self.norm_to_raw, norm))) as u32
    }
}

/// The last delta's `extra` at or before `off` (0 when none applies).
fn apply_deltas(deltas: &[(u32, i32)], off: u32) -> i32 {
    let idx = deltas.partition_point(|e| e.0 <= off);
    if idx == 0 { 0 } else { deltas[idx - 1].1 }
}

/// Translation between the buffer's marked coordinate space and the source string spaces, for
/// one layout pass. Spans without SA text hold no [`SpanMap`] and translate identically, so
/// texts without SA shape byte-identically to before this feature (ADR-3323).
#[derive(Debug, Default)]
struct LayoutMap {
    /// Marked-concat offset at each span's start (one more entry than spans).
    marked_starts: Vec<u32>,
    /// Normalized-concat offset at each span's start.
    norm_starts: Vec<u32>,
    /// Raw-concat offset at each span's start.
    raw_starts: Vec<u32>,
    /// Per-span translations; `None` = identity.
    spans: Vec<Option<SpanMap>>,
}

impl LayoutMap {
    /// Index of the span holding the marked-concat offset `marked_abs`.
    fn span_of(&self, marked_abs: u32) -> usize {
        let idx = self.marked_starts.partition_point(|s| *s <= marked_abs);
        idx.saturating_sub(1).min(self.spans.len().saturating_sub(1))
    }

    /// Absolute normalized-concat offset of an absolute marked-concat offset.
    fn norm_abs(&self, marked_abs: u32) -> u32 {
        let s = self.span_of(marked_abs);
        let local = marked_abs - self.marked_starts[s];
        let rel = match &self.spans[s] {
            Some(map) => map.marked_to_norm(local),
            None => local,
        };
        self.norm_starts[s] + rel
    }

    /// Absolute raw-concat offset of an absolute marked-concat offset.
    fn raw_abs(&self, marked_abs: u32) -> u32 {
        let s = self.span_of(marked_abs);
        let local = marked_abs - self.marked_starts[s];
        let rel = match &self.spans[s] {
            Some(map) => map.norm_to_raw(map.marked_to_norm(local)),
            None => local,
        };
        self.raw_starts[s] + rel
    }

    /// Paragraph-relative normalized offset of the marked offset `par_start + rel`. This is the
    /// space [`ShapedLine::range`] indexes ([`text_paragraphs`] of [`shaped_string`]).
    fn to_norm_rel(&self, par_start: u32, rel: u32) -> u32 {
        self.norm_abs(par_start + rel) - self.norm_abs(par_start)
    }

    /// Paragraph-relative raw offset of the marked offset `par_start + rel` (caret space).
    fn to_raw_rel(&self, par_start: u32, rel: u32) -> u32 {
        self.raw_abs(par_start + rel) - self.raw_abs(par_start)
    }

    /// True when any span carries SA markers.
    fn has_sa(&self) -> bool {
        self.spans.iter().any(Option::is_some)
    }
}

/// Like [`normalize_line_breaks`], additionally reporting where the raw (pre-normalization)
/// string diverges: the returned deltas map normalized offsets to raw offsets (`raw = norm +
/// extra`). Only used on the SA path, where carets must index the raw string.
fn normalize_line_breaks_mapped(s: &str, after_cr: bool) -> (std::borrow::Cow<'_, str>, Vec<(u32, i32)>) {
    let needs = s
        .chars()
        .any(|c| matches!(c, '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
        || (after_cr && s.starts_with('\n'));
    if !needs {
        return (std::borrow::Cow::Borrowed(s), Vec::new());
    }
    let mut out = String::with_capacity(s.len());
    let mut deltas: Vec<(u32, i32)> = Vec::new();
    let mut extra = 0i32;
    let mut norm_off = 0u32;
    let mut chars = s.chars().peekable();
    if after_cr && chars.peek() == Some(&'\n') {
        chars.next();
        extra += 1;
        deltas.push((0, extra));
    }
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    extra += 1;
                }
                out.push('\n');
                norm_off += 1;
                deltas.push((norm_off, extra));
            }
            '\u{0085}' | '\u{2028}' | '\u{2029}' => {
                // Two raw bytes became one normalized byte.
                out.push('\n');
                norm_off += 1;
                extra += 1;
                deltas.push((norm_off, extra));
            }
            c => {
                out.push(c);
                norm_off += u32::try_from(c.len_utf8()).unwrap_or(u32::MAX);
            }
        }
    }
    (std::borrow::Cow::Owned(out), deltas)
}

/// Inserts a U+200B ZERO WIDTH SPACE break marker at every SA-run internal boundary in `norm`
/// (a segmenter break with an SA character on both sides; boundaries next to spaces or
/// punctuation already break under UAX #14 and need no marker). Returns the marked string and
/// the `marked → norm` deltas. U+200B itself is a UAX #14 break opportunity, invisible
/// (default-ignorable, skipped by the glyph pass) and zero-width.
fn mark_sa_breaks(norm: &str, breaks: &[usize]) -> (String, Vec<(u32, i32)>) {
    const ZWSP_LEN: u32 = 3; // U+200B in UTF-8
    let mut marked = String::with_capacity(norm.len() + breaks.len() * ZWSP_LEN as usize);
    let mut deltas: Vec<(u32, i32)> = Vec::new();
    let mut extra = 0i32;
    let mut last = 0usize;
    for &b in breaks {
        // Segmenter offsets are char boundaries inside the string (0 and len were dropped).
        let prev = norm.get(..b).and_then(|s| s.chars().next_back());
        let next = norm.get(b..).and_then(|s| s.chars().next());
        if !(prev.is_some_and(crate::line_break_sa::is_sa_char)
            && next.is_some_and(crate::line_break_sa::is_sa_char))
        {
            continue;
        }
        marked.push_str(&norm[last..b]);
        let at = u32::try_from(marked.len()).unwrap_or(u32::MAX);
        marked.push('\u{200B}');
        extra -= ZWSP_LEN as i32;
        deltas.push((at + ZWSP_LEN, extra));
        last = b;
    }
    marked.push_str(&norm[last..]);
    (marked, deltas)
}

impl TextSystem {
    /// Builds the [`CaretMap`] of `text` laid out as [`TextSystem::shape`] lays it out (same
    /// `scale` and `max_width`), for caret placement and pointer hit tests. `ScaleToFit` and
    /// ellipsis are not applied: a caret map describes the full text.
    pub fn caret_map(
        &mut self,
        text: &Text,
        scale: f32,
        max_width: Option<f32>,
        loc: &Localization,
    ) -> CaretMap {
        let (buffer, first_px, map, marked_pars) = self.layout_buffer(text, scale, max_width, loc, false);
        let full: String = text.spans.iter().map(|s| s.content.resolve(loc)).collect();
        // Paragraph starts in raw coordinates: the buffer's paragraphs follow the *normalized*
        // breaks, so a raw `'\n'` scan alone missed CR/NEL/U+2028/U+2029 breaks.
        let line_starts = raw_paragraph_starts(&full);
        let line_h = first_px * text.line_height.max(0.1);
        let mut lines: Vec<CaretLine> = Vec::new();
        let mut seen_last = false;
        let has_sa = map.has_sa();
        for run in buffer.layout_runs() {
            let base = line_starts.get(run.line_i).copied().unwrap_or(full.len());
            let marked_par = marked_pars.get(run.line_i).copied().unwrap_or(0);
            seen_last |= run.line_i + 1 == line_starts.len();
            let mut clusters: Vec<CaretCluster> = Vec::new();
            for g in run.glyphs {
                // Clusters index the raw source string. Identity spans keep the historic
                // arithmetic (base + cluster offset); SA spans translate through the map, which
                // removes the break markers (ADR-3323).
                let raw_rel = |rel: usize| -> usize {
                    if !has_sa {
                        return rel;
                    }
                    let abs = marked_par.saturating_add(u32::try_from(rel).unwrap_or(u32::MAX));
                    let in_sa = map.spans.get(map.span_of(abs)).is_some_and(Option::is_some);
                    if in_sa {
                        usize::try_from(map.to_raw_rel(marked_par, abs - marked_par)).unwrap_or(rel)
                    } else {
                        rel
                    }
                };
                let (start, end) = (base + raw_rel(g.start), base + raw_rel(g.end));
                let (x0, x1) = (g.x, g.x + g.w);
                // A cluster drawn with several glyphs (base plus marks) is one caret span.
                let same = clusters.last().is_some_and(|c| c.start == start && c.end == end);
                if same && let Some(c) = clusters.last_mut() {
                    c.x0 = c.x0.min(x0);
                    c.x1 = c.x1.max(x1);
                } else {
                    clusters.push(CaretCluster {
                        start,
                        end,
                        x0,
                        x1,
                        rtl: g.level.is_rtl(),
                    });
                }
            }
            let start = clusters.iter().map(|c| c.start).min().unwrap_or(base);
            let end = clusters.iter().map(|c| c.end).max().unwrap_or(base);
            let origin_x = if run.rtl { max_width.unwrap_or(0.0) } else { 0.0 };
            lines.push(CaretLine {
                top: run.line_top,
                height: run.line_height,
                start,
                end,
                origin_x,
                rtl: run.rtl,
                clusters,
            });
        }
        if !seen_last {
            // An empty text, or a trailing line break cosmic-text laid out no run for.
            let top = lines.last().map_or(0.0, |l| l.top + l.height);
            let start = line_starts.last().copied().unwrap_or(0);
            lines.push(CaretLine {
                top,
                height: line_h,
                start,
                end: start,
                origin_x: 0.0,
                rtl: false,
                clusters: Vec::new(),
            });
        }
        CaretMap { text: full, lines }
    }
}

/// Whether a shaped paragraph of `buffer` starts with a bidi run against the paragraph's
/// direction, the case cosmic-text 0.19's no-wrap layout loses the first word of (ADR-9340).
fn no_wrap_drops_a_leading_run(buffer: &ct::Buffer) -> bool {
    buffer.lines.iter().any(|line| {
        line.shape_opt()
            .is_some_and(|s| s.spans.first().is_some_and(|span| span.level.is_rtl() != s.rtl))
    })
}

/// Bundled-first per-script font fallback.
///
/// Why a custom list at all: cosmic-text's built-in platform lists (a) name the full
/// "Noto Sans CJK {SC,JP,KR}" super-families and "Noto Sans Arabic" — names the engine's
/// *subset* families do not carry, so they would never be tried — and (b) are completely
/// empty on Android (`cosmic-text` `font/fallback/other.rs`), where fontdb also loads no
/// system fonts. Without this shim any script the primary family does not cover renders as
/// tofu on a stock machine. Our bundled families come first (deterministic coverage), the
/// platform's own names follow so installed system fonts still win for scripts the engine
/// does not bundle. The platform name lists mirror cosmic-text's `han_unification` and
/// `common_fallback` tables (MIT OR Apache-2.0).
struct BundledFallback;

/// The regional Han form a locale prefers (ADR-1618). The bundled subset is Simplified Chinese;
/// a desktop that has a Japanese, Korean or Traditional face installed uses it first for the
/// matching locale, so `ja` text gets Japanese ideograph forms instead of Chinese-preferred ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HanVariant {
    /// Simplified Chinese, and every locale without a regional preference.
    Simplified,
    /// Japanese.
    Japanese,
    /// Korean (hanja).
    Korean,
    /// Traditional Chinese, Taiwan and the rest.
    Traditional,
    /// Traditional Chinese, Hong Kong.
    HongKong,
}

impl HanVariant {
    /// The variant for a BCP 47 tag (`ja`, `ja-JP`, `zh-Hant-TW`, `zh-HK`, `ko-KR`); case and
    /// `_` do not matter.
    pub(crate) fn of_locale(locale: &str) -> Self {
        let lower = locale.to_ascii_lowercase().replace('_', "-");
        let mut parts = lower.split('-');
        match (parts.next(), parts.collect::<Vec<_>>()) {
            (Some("ja"), _) => Self::Japanese,
            (Some("ko"), _) => Self::Korean,
            (Some("zh"), rest) if rest.contains(&"hk") || rest.contains(&"mo") => Self::HongKong,
            (Some("zh"), rest) if rest.contains(&"hant") || rest.contains(&"tw") => Self::Traditional,
            _ => Self::Simplified,
        }
    }

    /// Families to try for Han ideographs, hiragana and katakana: this variant's system face
    /// first, then the bundled SC subset (it carries the full kana blocks and CJK punctuation),
    /// then the other desktop CJK families.
    fn families(self) -> &'static [&'static str] {
        match self {
            Self::Simplified => han::SIMPLIFIED,
            Self::Japanese => han::JAPANESE,
            Self::Korean => han::KOREAN,
            Self::Traditional => han::TRADITIONAL,
            Self::HongKong => han::HONG_KONG,
        }
    }
}

#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
mod han {
    pub(super) const SIMPLIFIED: &[&str] = &[
        "Noto Sans SC",
        "Noto Sans CJK SC",
        "Noto Sans CJK JP",
        "Noto Sans CJK TC",
        "Noto Sans CJK HK",
        "Noto Sans CJK KR",
    ];
    pub(super) const JAPANESE: &[&str] = &[
        "Noto Sans CJK JP",
        "Noto Sans SC",
        "Noto Sans CJK SC",
        "Noto Sans CJK TC",
        "Noto Sans CJK HK",
        "Noto Sans CJK KR",
    ];
    pub(super) const KOREAN: &[&str] = &[
        "Noto Sans CJK KR",
        "Noto Sans SC",
        "Noto Sans CJK SC",
        "Noto Sans CJK JP",
        "Noto Sans CJK TC",
        "Noto Sans CJK HK",
    ];
    pub(super) const TRADITIONAL: &[&str] = &[
        "Noto Sans CJK TC",
        "Noto Sans SC",
        "Noto Sans CJK SC",
        "Noto Sans CJK JP",
        "Noto Sans CJK HK",
        "Noto Sans CJK KR",
    ];
    pub(super) const HONG_KONG: &[&str] = &[
        "Noto Sans CJK HK",
        "Noto Sans SC",
        "Noto Sans CJK SC",
        "Noto Sans CJK JP",
        "Noto Sans CJK TC",
        "Noto Sans CJK KR",
    ];
}
#[cfg(target_os = "windows")]
mod han {
    pub(super) const SIMPLIFIED: &[&str] = &[
        "Noto Sans SC",
        "Microsoft YaHei UI",
        "Yu Gothic",
        "Microsoft JhengHei UI",
        "MingLiU_HKSCS",
        "Malgun Gothic",
    ];
    pub(super) const JAPANESE: &[&str] = &[
        "Yu Gothic",
        "Noto Sans SC",
        "Microsoft YaHei UI",
        "Microsoft JhengHei UI",
        "MingLiU_HKSCS",
        "Malgun Gothic",
    ];
    pub(super) const KOREAN: &[&str] = &[
        "Malgun Gothic",
        "Noto Sans SC",
        "Microsoft YaHei UI",
        "Yu Gothic",
        "Microsoft JhengHei UI",
        "MingLiU_HKSCS",
    ];
    pub(super) const TRADITIONAL: &[&str] = &[
        "Microsoft JhengHei UI",
        "Noto Sans SC",
        "Microsoft YaHei UI",
        "Yu Gothic",
        "MingLiU_HKSCS",
        "Malgun Gothic",
    ];
    pub(super) const HONG_KONG: &[&str] = &[
        "MingLiU_HKSCS",
        "Noto Sans SC",
        "Microsoft YaHei UI",
        "Yu Gothic",
        "Microsoft JhengHei UI",
        "Malgun Gothic",
    ];
}
#[cfg(target_os = "macos")]
mod han {
    pub(super) const SIMPLIFIED: &[&str] = &[
        "Noto Sans SC",
        "PingFang SC",
        "Hiragino Sans",
        "PingFang TC",
        "PingFang HK",
        "Apple SD Gothic Neo",
    ];
    pub(super) const JAPANESE: &[&str] = &[
        "Hiragino Sans",
        "Noto Sans SC",
        "PingFang SC",
        "PingFang TC",
        "PingFang HK",
        "Apple SD Gothic Neo",
    ];
    pub(super) const KOREAN: &[&str] = &[
        "Apple SD Gothic Neo",
        "Noto Sans SC",
        "PingFang SC",
        "Hiragino Sans",
        "PingFang TC",
        "PingFang HK",
    ];
    pub(super) const TRADITIONAL: &[&str] = &[
        "PingFang TC",
        "Noto Sans SC",
        "PingFang SC",
        "Hiragino Sans",
        "PingFang HK",
        "Apple SD Gothic Neo",
    ];
    pub(super) const HONG_KONG: &[&str] = &[
        "PingFang HK",
        "Noto Sans SC",
        "PingFang SC",
        "Hiragino Sans",
        "PingFang TC",
        "Apple SD Gothic Neo",
    ];
}
/// Android loads no system fonts: every variant is the bundled subset.
#[cfg(target_os = "android")]
mod han {
    pub(super) const SIMPLIFIED: &[&str] = &["Noto Sans SC"];
    pub(super) const JAPANESE: &[&str] = SIMPLIFIED;
    pub(super) const KOREAN: &[&str] = SIMPLIFIED;
    pub(super) const TRADITIONAL: &[&str] = SIMPLIFIED;
    pub(super) const HONG_KONG: &[&str] = SIMPLIFIED;
}

/// Hangul: the bundled KR subset (all 2350 KS X 1001 syllables) first, then the platform's.
#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
const HANGUL: &[&str] = &["Noto Sans KR", "Noto Sans CJK KR"];
#[cfg(target_os = "windows")]
const HANGUL: &[&str] = &["Noto Sans KR", "Malgun Gothic"];
#[cfg(target_os = "macos")]
const HANGUL: &[&str] = &["Noto Sans KR", "Apple SD Gothic Neo"];
#[cfg(target_os = "android")]
const HANGUL: &[&str] = &["Noto Sans KR"];

/// Hebrew, Devanagari and Thai: the bundled Noto Sans subsets (ADR-1842, L06). They carry the
/// whole script block and all layout tables, so every target shapes these scripts without a system font.
const HEBREW: &[&str] = &["Noto Sans Hebrew"];
const DEVANAGARI: &[&str] = &["Noto Sans Devanagari"];
const THAI: &[&str] = &["Noto Sans Thai"];

/// Arabic: the bundled Naskh face (joining GSUB forms) first, then the platform's.
#[cfg(any(
    target_os = "android",
    all(unix, not(target_os = "macos")),
    target_os = "windows"
))]
const ARABIC: &[&str] = &["Noto Naskh Arabic", "Noto Sans Arabic"];
#[cfg(target_os = "macos")]
const ARABIC: &[&str] = &["Noto Naskh Arabic", "Geeza Pro"];

/// Tried after every script list: the bundled color-emoji face, then the symbol and
/// generic families each platform commonly installs (mirrors cosmic-text's lists).
#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
const COMMON: &[&str] = &[
    "Noto Color Emoji",
    "Noto Sans Symbols",
    "Noto Sans Symbols2",
    "Noto Sans",
    "Noto Sans Mono",
    "DejaVu Sans",
    "DejaVu Sans Mono",
    "FreeSans",
    "FreeMono",
];
#[cfg(target_os = "windows")]
const COMMON: &[&str] = &[
    "Noto Color Emoji",
    "Segoe UI",
    "Segoe UI Emoji",
    "Segoe UI Symbol",
    "Segoe UI Historic",
];
#[cfg(target_os = "macos")]
const COMMON: &[&str] = &[
    "Noto Color Emoji",
    "Apple Color Emoji",
    ".SF NS",
    "Menlo",
    "Geneva",
    "Arial Unicode MS",
];
#[cfg(target_os = "android")]
const COMMON: &[&str] = &["Noto Color Emoji"];

impl ct::Fallback for BundledFallback {
    fn common_fallback(&self) -> &'static [&'static str] {
        COMMON
    }

    fn forbidden_fallback(&self) -> &'static [&'static str] {
        ct::PlatformFallback.forbidden_fallback()
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &'static [&'static str] {
        match script {
            Script::Han | Script::Hiragana | Script::Katakana => HanVariant::of_locale(locale).families(),
            Script::Hangul => HANGUL,
            Script::Arabic => ARABIC,
            Script::Hebrew => HEBREW,
            Script::Devanagari => DEVANAGARI,
            Script::Thai => THAI,
            _ => ct::PlatformFallback.script_fallback(script, locale),
        }
    }
}

/// The embedded default font set as `(file name, bytes)`: the Inter trio plus per-script Noto subsets
/// (Arabic, Hebrew, Devanagari, Thai, Simplified Chinese with the full kana blocks, Korean, and color emoji; all
/// OFL-1.1, regenerated by `assets/fonts/subset.py`, licence texts alongside), in the order they are loaded.
/// Embedded with `include_bytes!`, so every target — Android, which loads no system fonts, included
/// — ships the same default coverage. The glyph coverage check (`loc_glyphs`) reads the same
/// bytes the renderer loads.
pub fn embedded_font_data() -> [(&'static str, &'static [u8]); 10] {
    [
        (
            "Inter-Regular.ttf",
            include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice(),
        ),
        (
            "Inter-Bold.ttf",
            include_bytes!("../assets/fonts/Inter-Bold.ttf").as_slice(),
        ),
        (
            "Inter-Italic.ttf",
            include_bytes!("../assets/fonts/Inter-Italic.ttf").as_slice(),
        ),
        (
            "NotoNaskhArabic-400-subset.ttf",
            include_bytes!("../assets/fonts/NotoNaskhArabic-400-subset.ttf").as_slice(),
        ),
        (
            "NotoSansSC-400-subset.ttf",
            include_bytes!("../assets/fonts/NotoSansSC-400-subset.ttf").as_slice(),
        ),
        (
            "NotoSansKR-400-subset.ttf",
            include_bytes!("../assets/fonts/NotoSansKR-400-subset.ttf").as_slice(),
        ),
        (
            "NotoSansHebrew-400-subset.ttf",
            include_bytes!("../assets/fonts/NotoSansHebrew-400-subset.ttf").as_slice(),
        ),
        (
            "NotoSansDevanagari-400-subset.ttf",
            include_bytes!("../assets/fonts/NotoSansDevanagari-400-subset.ttf").as_slice(),
        ),
        (
            "NotoSansThai-400-subset.ttf",
            include_bytes!("../assets/fonts/NotoSansThai-400-subset.ttf").as_slice(),
        ),
        (
            "NotoColorEmoji-subset.ttf",
            include_bytes!("../assets/fonts/NotoColorEmoji-subset.ttf").as_slice(),
        ),
    ]
}

/// The font database's warnings counted on this thread (it logs through the `log` bridge
/// with target `fontdb`); nothing is forwarded.
struct FontScanCounter(std::sync::Arc<std::sync::atomic::AtomicU32>);

impl tracing::Subscriber for FontScanCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target().starts_with("fontdb")
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        // `ERROR` and `WARN` (tracing orders levels by verbosity).
        if *event.metadata().level() <= tracing::Level::WARN {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Runs `scan` (a system font scan) with the font database's warnings counted instead of
/// logged on this thread, and returns its result and the count: each is one font file the
/// database could not parse ("malformed font") and skipped. Other targets are dropped for the
/// scan's duration (the scan logs nothing else).
pub fn count_font_scan_warnings<R>(scan: impl FnOnce() -> R) -> (R, u32) {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let dispatch = tracing::Dispatch::new(FontScanCounter(count.clone()));
    let out = tracing::dispatcher::with_default(&dispatch, scan);
    (out, count.load(std::sync::atomic::Ordering::Relaxed))
}

/// The one line per process for skipped system font files (never a path).
fn note_skipped_system_fonts(skipped: u32) {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if skipped > 0 && !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!(
            target: "regia_ui::text",
            skipped,
            "{skipped} system font file(s) could not be parsed and were skipped"
        );
    }
}

/// Loads [`embedded_font_data`] into `db`.
fn load_embedded(db: &mut ct::fontdb::Database) {
    for (_, bytes) in embedded_font_data() {
        db.load_font_data(bytes.to_vec());
    }
}

/// Horizontal padding of a node in physical px (points dimensions scaled; the same convention
/// [`crate::extract`](extract) uses to place the text block). Percent padding reports 0 there too,
/// so shaping and placement stay consistent.
pub(crate) fn pad_x_px(style: Option<&NodeStyle>, scale: f32) -> [f32; 2] {
    let p = style.map(|s| &s.0.padding);
    let dim = |d: Option<&crate::style::Dimension>| match d {
        Some(crate::style::Dimension::Points(v)) => v * scale,
        _ => 0.0,
    };
    [dim(p.map(|p| &p.left)), dim(p.map(|p| &p.right))]
}

/// `PostUpdate`, before layout: points the text system's font-fallback locale at the game's
/// selected locale (ADR-1618), so a player who picks `ja` on an English OS gets Japanese Han
/// forms. After a font load or a default-family switch it also bumps the layout/shaping
/// generation once. Cheap when nothing changed (one string comparison).
pub fn sync_font_locale(fonts: Option<ResMut<TextSystem>>, loc: Option<ResMut<Localization>>) {
    let (Some(mut fonts), Some(mut loc)) = (fonts, loc) else {
        return;
    };
    if fonts.locale() != loc.locale() && !loc.locale().is_empty() {
        let _span = tracing::debug_span!("ui_font_locale").entered();
        fonts.set_locale(loc.locale());
    }
    if fonts.fonts_changed {
        // Layout and shaping already share this generation (also used for text scale).
        loc.generation.0 = loc.generation.0.wrapping_add(1);
        fonts.fonts_changed = false;
    }
}

/// `PostUpdate`, after layout: shape every [`Text`] whose content, node width, locale or scale
/// changed since it was last shaped, and (re)measure it for the next layout pass.
pub fn shape_text(
    mut fonts: ResMut<TextSystem>,
    loc: Res<Localization>,
    mut nodes: Query<(
        Entity,
        &Text,
        Option<&ComputedNode>,
        Option<&NodeStyle>,
        Option<&mut ShapedText>,
    )>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("ui_shape_text").entered();
    // The same generation layout keys its measures with: a change of the minimum size or the
    // text scale bumps it (`scale::apply_ui_device_policy`, `scale::apply_ui_accessibility`).
    // The old key mixed the minimum size in here only, so on handhelds every text layout had
    // just shaped was shaped a second time.
    let generation = loc.generation.0;
    for (entity, text, computed, style, mut shaped) in &mut nodes {
        let digest = text_digest(text, generation);
        let scale = computed.map(|c| c.scale).unwrap_or(1.0);
        // Shape at the content-box width whenever layout has one: `WrapMode::None` still needs the
        // width so RTL paragraphs align inside the box (wrapping stays off via `set_wrap(None)`),
        // and aligned ink must land at the content edge, not the border-box edge.
        let [pad_l, pad_r] = pad_x_px(style, scale);
        let node_w = computed.map(|c| c.rect[2] - pad_l - pad_r).unwrap_or(0.0);
        let max_w = if node_w > 1.0 { Some(node_w) } else { None };
        // Read through `Deref` only: a mutable deref marks the component changed (damage).
        let current = shaped.as_ref().is_some_and(|s| {
            s.digest == digest
                && (s.shaped_scale - scale).abs() < 1e-3
                && match max_w {
                    None => s.shaped_width <= 1.0, // previously unconstrained
                    Some(w) => (s.shaped_width - w).abs() < 0.75,
                }
        });
        if current {
            // Colors are not part of the digest: a color-only change recolors in place.
            if let Some(s) = shaped.as_mut()
                && s.colors_digest != colors_digest(text)
            {
                s.recolor(text);
            }
            continue;
        }
        let mut result = fonts.shape(text, scale, max_w, &loc);
        result.digest = digest;
        match shaped {
            Some(mut s) => *s = result,
            None => {
                commands.entity(entity).insert(result);
            }
        }
    }
}

/// Digest of a text's content + locale generation (any change → reshape).
pub(crate) fn text_digest(text: &Text, generation: u64) -> u64 {
    let mut h = fnv_offset();
    for span in &text.spans {
        match &span.content {
            TextContent::Lit(s) => {
                h = fnv_write_str(h, "L");
                h = fnv_write_str(h, s);
            }
            TextContent::Loc { key, args } => {
                h = fnv_write_str(h, "K");
                h = fnv_write_str(h, key);
                for (k, v) in args {
                    h = fnv_write_str(h, k);
                    h = fnv_write_str(h, v);
                }
            }
        }
        h = fnv_write_u32(h, span.style.font_px.to_bits());
        h = fnv_write_str(h, span.style.family.as_deref().unwrap_or(""));
        h = fnv_write_u32(h, (span.style.bold as u32) | ((span.style.italic as u32) << 1));
        // ADR-2045 shaping attributes, hashed only when set so default digests are unchanged.
        let st = &span.style;
        if st.letter_spacing != 0.0
            || st.weight.is_some()
            || st.underline
            || st.strikethrough
            || !st.features.is_empty()
        {
            h = fnv_write_str(h, "X");
            h = fnv_write_u32(h, st.letter_spacing.to_bits());
            h = fnv_write_u32(h, st.weight.map_or(0, u32::from));
            h = fnv_write_u32(h, (st.underline as u32) | ((st.strikethrough as u32) << 1));
            for f in &st.features {
                h = fnv_write_str(h, f);
                h = fnv_write_str(h, ";");
            }
        }
    }
    h = fnv_write_u32(h, text.align as u32);
    h = fnv_write_u32(h, text.wrap as u32);
    h = fnv_write_u32(h, text.overflow as u32);
    h = fnv_write_u32(h, text.line_height.to_bits());
    h = fnv_write_u64(h, generation);
    h
}

fn fnv_offset() -> u64 {
    0xcbf29ce484222325
}

fn fnv_write_u32(h: u64, v: u32) -> u64 {
    (h ^ u64::from(v)).wrapping_mul(0x100000001b3)
}

fn fnv_write_u64(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x100000001b3)
}

fn fnv_write_str(mut h: u64, s: &str) -> u64 {
    for b in s.as_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

/// Parses the rich-text markup subset (`specs/18` § 2): `<b>`, `<i>`, `<u>` (underline), `<s>`
/// (strikethrough; both ADR-2045), `<color=#..>`, `<size=..>` and `<icon=name>` (Tier 1 renders
/// icons as spans with family `"icon-name"`).
pub fn parse_markup(input: &str, base: TextStyle) -> Vec<TextSpan> {
    let mut spans: Vec<TextSpan> = Vec::new();
    let mut style = base;
    let mut style_stack: Vec<TextStyle> = Vec::new();
    let mut current = String::new();
    let mut rest = input;
    while !rest.is_empty() {
        let Some(tag_start) = rest.find('<') else {
            current.push_str(rest);
            break;
        };
        let Some(close) = rest[tag_start..].find('>').map(|i| i + tag_start) else {
            current.push_str(rest);
            break;
        };
        current.push_str(&rest[..tag_start]);
        let tag = &rest[tag_start + 1..close];
        rest = &rest[close + 1..];
        let flush = |current: &mut String, style: TextStyle, spans: &mut Vec<TextSpan>| {
            if !current.is_empty() {
                spans.push(TextSpan {
                    content: TextContent::Lit(std::mem::take(current)),
                    style,
                });
            }
        };
        match tag {
            "b" | "i" | "u" | "s" => {
                flush(&mut current, style.clone(), &mut spans);
                style_stack.push(style.clone());
                match tag {
                    "b" => style.bold = true,
                    "i" => style.italic = true,
                    "u" => style.underline = true,
                    _ => style.strikethrough = true,
                }
            }
            "/b" | "/i" | "/u" | "/s" => {
                flush(&mut current, style.clone(), &mut spans);
                if let Some(prev) = style_stack.pop() {
                    style = prev;
                }
            }
            t if t.starts_with("color=") => {
                flush(&mut current, style.clone(), &mut spans);
                style_stack.push(style.clone());
                if let Some(c) = UiColor::from_hex(&t[6..]) {
                    style.color = c;
                }
            }
            "/color" => {
                flush(&mut current, style.clone(), &mut spans);
                if let Some(prev) = style_stack.pop() {
                    style = prev;
                }
            }
            t if t.starts_with("size=") => {
                flush(&mut current, style.clone(), &mut spans);
                style_stack.push(style.clone());
                if let Ok(px) = t[5..].parse::<f32>() {
                    style.font_px = px;
                }
            }
            "/size" => {
                flush(&mut current, style.clone(), &mut spans);
                if let Some(prev) = style_stack.pop() {
                    style = prev;
                }
            }
            t if t.starts_with("icon=") => {
                flush(&mut current, style.clone(), &mut spans);
                let name = &t[5..];
                spans.push(TextSpan {
                    content: TextContent::Lit(name.to_owned()),
                    style: TextStyle {
                        family: Some(format!("icon-{name}")),
                        ..style.clone()
                    },
                });
            }
            _ => {
                // Unknown or unmatched tag: emit it literally.
                current.push('<');
                current.push_str(tag);
                current.push('>');
            }
        }
    }
    if !current.is_empty() {
        spans.push(TextSpan {
            content: TextContent::Lit(current),
            style,
        });
    }
    spans
}

/// A default-font selection that cannot be applied to the current font database.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefaultFontError {
    /// No loaded font declares this family. Load it before selecting it.
    #[error("font family is not loaded: {0}")]
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_names_are_interned_once() {
        let a = intern_family("Noto Naskh Arabic");
        let b = intern_family(&String::from("Noto Naskh Arabic"));
        assert!(std::ptr::eq(a, b), "same name must reuse one leaked allocation");
        assert_eq!(a, "Noto Naskh Arabic");
        assert_ne!(intern_family("Inter"), a);
    }

    #[test]
    fn intern_family_reuses_one_allocation_per_name() {
        let a = super::intern_family("Intern Test Family");
        let b = super::intern_family(&String::from("Intern Test Family"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "Intern Test Family");
        let c = super::intern_family("Other Intern Family");
        assert!(!std::ptr::eq(a, c));
    }

    fn base() -> TextStyle {
        TextStyle::default()
    }

    fn lit_of(spans: &[TextSpan]) -> Vec<String> {
        spans
            .iter()
            .filter_map(|s| match &s.content {
                TextContent::Lit(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn markup_bold_italic_nested() {
        let spans = parse_markup("a<b>bo<i>boit</i>ld</b>c", base());
        assert_eq!(lit_of(&spans), vec!["a", "bo", "boit", "ld", "c"]);
        assert!(!spans[0].style.bold);
        assert!(spans[1].style.bold && !spans[1].style.italic);
        assert!(spans[2].style.bold && spans[2].style.italic);
        assert!(spans[3].style.bold && !spans[3].style.italic);
        assert!(!spans[4].style.bold);
    }

    #[test]
    fn markup_color_size() {
        let spans = parse_markup("<color=#FF0000>red <size=10>small</size> red</color>", base());
        assert_eq!(lit_of(&spans), vec!["red ", "small", " red"]);
        assert_eq!(spans[0].style.color, UiColor::from_hex("#FF0000").unwrap());
        assert!((spans[1].style.font_px - 10.0).abs() < 1e-4);
        assert_eq!(spans[2].style.font_px, base().font_px);
        assert_eq!(spans[2].style.color, UiColor::from_hex("#FF0000").unwrap());
    }

    #[test]
    fn markup_icon_and_unknown() {
        let spans = parse_markup("x<icon=coin>y<nope>z", base());
        // Unknown tags are emitted as literal text into the current span.
        assert_eq!(lit_of(&spans), vec!["x", "coin", "y<nope>z"]);
        assert_eq!(spans[1].style.family.as_deref(), Some("icon-coin"));
    }

    #[test]
    fn markup_plain() {
        let spans = parse_markup("hello 世界 🦀", base());
        assert_eq!(lit_of(&spans), vec!["hello 世界 🦀"]);
    }

    #[test]
    fn shaping_latin_produces_glyphs() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let shaped = sys.shape(&Text::new("Hello"), 1.0, None, &loc);
        assert!(
            !shaped.glyphs.is_empty(),
            "shaping Latin text must produce glyphs"
        );
        assert_eq!(shaped.lines.len(), 1);
    }

    /// The engine's default font set must cover every gallery script with no system fonts
    /// present (the Android situation: fontdb loads nothing there). Glyph id 0 is `.notdef`
    /// — the tofu box the M7 round-5 gallery shipped for 漢/字, which the previous SC subset
    /// did not contain.
    #[test]
    fn default_stack_covers_every_script_without_notdef() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        for sample in [
            "Hello مرحبا 你好世界 日本語のかなと漢字 한국어 🚀",
            "你好世界 — 日本語のかなと漢字",
            "한국어 가나다",
            "مرحبا بالعالم مع التشكيل",
            "界体 語漢動進 開頁項標", // variant supplement (shinjitai / legacy curation)
        ] {
            let shaped = sys.shape(&Text::new(sample), 1.0, Some(400.0), &loc);
            assert!(!shaped.glyphs.is_empty(), "{sample}: no glyphs");
            let tofu: Vec<_> = shaped
                .glyphs
                .iter()
                .filter(|g| g.cache_key.glyph_id == 0)
                .collect();
            assert!(
                tofu.is_empty(),
                "{sample}: {} .notdef glyphs remained",
                tofu.len()
            );
        }
    }

    /// Bold spans query weight 700, which no bundled CJK/Arabic face carries: the fallback
    /// walk must still resolve them (through the last-resort "any font" pass).
    #[test]
    fn bold_scripts_fall_back_without_notdef() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        for sample in ["你好世界 漢字", "مرحبا بالعالم", "한국어"] {
            let text = Text::new(sample).with_style(TextStyle {
                bold: true,
                ..TextStyle::default()
            });
            let shaped = sys.shape(&text, 1.0, Some(400.0), &loc);
            assert!(!shaped.glyphs.is_empty(), "{sample}: no glyphs");
            assert!(
                shaped.glyphs.iter().all(|g| g.cache_key.glyph_id != 0),
                "{sample}: bold pass left .notdef glyphs"
            );
        }
    }

    /// 🇯🇵 is two regional indicators; advanced shaping must produce the single flag glyph,
    /// proving the GSUB ligature closure survived subsetting (a broken closure would leave
    /// two indicator letters, or worse, tofu).
    #[test]
    fn default_stack_ligates_flag_emoji() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let shaped = sys.shape(&Text::new("🇯🇵"), 1.0, None, &loc);
        assert_eq!(shaped.glyphs.len(), 1, "the flag pair must ligate to one glyph");
        assert_ne!(shaped.glyphs[0].cache_key.glyph_id, 0);
        let both = sys.shape(&Text::new("🇺🇸🇯🇵"), 1.0, None, &loc);
        assert_eq!(both.glyphs.len(), 2, "two flag pairs → two flag glyphs");
    }

    /// The unpinned default family is the bundled Inter (fontdb's sans-serif slot is pinned
    /// in `TextSystem::with_db`); CJK runs must come from the bundled SC face, not from it.
    #[test]
    fn latin_defaults_to_inter_and_cjk_to_the_bundled_sc_face() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let family_of = |sys: &mut TextSystem, text: &str| -> Option<String> {
            let shaped = sys.shape(&Text::new(text), 1.0, None, &loc);
            let g = shaped.glyphs.first()?;
            let face = sys
                .font_system
                .db()
                .faces()
                .find(|f| f.id == g.cache_key.font_id)?;
            face.families.first().map(|(n, _)| n.to_owned())
        };
        assert_eq!(
            family_of(&mut sys, "Hello").as_deref(),
            Some("Inter"),
            "unpinned Latin must default to Inter"
        );
        assert_eq!(
            family_of(&mut sys, "你好").as_deref(),
            Some("Noto Sans SC"),
            "unpinned Han must land in the bundled SC subset"
        );
    }

    #[test]
    fn digest_changes_with_content() {
        let mut text = Text::new("hello");
        let loc_gen = 0u64;
        let d1 = text_digest(&text, loc_gen);
        text.spans[0].content = TextContent::Lit("world".into());
        assert_ne!(d1, text_digest(&text, loc_gen));
        assert_ne!(d1, text_digest(&text, loc_gen + 1));
    }

    #[test]
    fn arabic_advanced_shaping_joins_and_rtl_start_aligns() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let arabic = "مرحبا";
        let text = Text {
            spans: vec![TextSpan::lit(arabic)],
            align: TextAlign::Start,
            wrap: WrapMode::None,
            ..Default::default()
        };
        let shaped = sys.shape(&text, 1.0, Some(400.0), &loc);
        assert!(!shaped.glyphs.is_empty(), "expected shaped glyphs");

        // Isolated-form baseline: one shape call per character (no neighbour context).
        let mut isolated_ids = Vec::new();
        for ch in arabic.chars() {
            let solo = sys.shape(&Text::new(ch.to_string()), 1.0, None, &loc);
            isolated_ids.extend(solo.glyphs.iter().map(|g| g.cache_key.glyph_id));
        }
        let joined_ids: Vec<_> = shaped.glyphs.iter().map(|g| g.cache_key.glyph_id).collect();
        assert_ne!(
            joined_ids, isolated_ids,
            "Advanced shaping must produce joined glyph IDs, not isolated forms"
        );
        // Joined Arabic usually emits ≤ isolated glyph count (ligatures / contextual forms).
        assert!(
            joined_ids.len() <= isolated_ids.len(),
            "joined={} isolated={}",
            joined_ids.len(),
            isolated_ids.len()
        );

        // RTL Start aligns to the right edge of the layout box.
        let first_x = shaped.glyphs.first().map(|g| g.pen[0]).unwrap_or(0);
        assert!(
            first_x > 300,
            "first RTL glyph should sit near the right edge, got pen.x={first_x}"
        );
    }

    /// `TextOverflow::Ellipsis` marks a line only when wrapping is off. Wrapped text keeps
    /// every cluster; an unwrapped line wider than the box ends with an ellipsis.
    #[test]
    fn wrapped_ellipsis_keeps_every_line() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let sample = "Hello world this is a long label";
        let width = 40.0;
        let wrapped = Text::new(sample)
            .with_wrap(WrapMode::Word)
            .with_overflow(TextOverflow::Ellipsis);
        let shaped = sys.shape(&wrapped, 1.0, Some(width), &loc);
        let plain = sys.shape(
            &Text::new(sample).with_wrap(WrapMode::Word),
            1.0,
            Some(width),
            &loc,
        );
        assert!(shaped.lines.len() > 1, "the sample must wrap at {width} px");
        assert!(shaped.lines.iter().all(|line| !line.ellipsized));
        assert_eq!(
            shaped.glyphs.len(),
            plain.glyphs.len(),
            "wrapped ellipsis keeps every cluster"
        );

        let cut = sys.shape(
            &Text::new(sample)
                .with_wrap(WrapMode::None)
                .with_overflow(TextOverflow::Ellipsis),
            1.0,
            Some(width),
            &loc,
        );
        let full = sys.shape(
            &Text::new(sample).with_wrap(WrapMode::None),
            1.0,
            Some(width),
            &loc,
        );
        assert!(cut.lines.iter().any(|line| line.ellipsized));
        assert!(
            cut.glyphs.len() < full.glyphs.len(),
            "unwrapped ellipsis shortens the line ({} vs {})",
            cut.glyphs.len(),
            full.glyphs.len()
        );
    }

    /// ADR-359: a label in a box that layout rounded down to whole pixels (up to a pixel narrower
    /// than the text) stays on one line; a box clearly narrower than the text still wraps. Gate g1
    /// of integ/v0128: "Tiny" at 8 px (16.2 px) in its 16 px node wrapped to "Tin" over "y".
    #[test]
    fn text_in_a_box_rounded_down_by_layout_does_not_wrap() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let text = Text::new("Tiny").with_style(TextStyle {
            font_px: 8.0,
            ..Default::default()
        });
        let natural = sys.shape(&text, 1.0, None, &loc);
        assert!(!is_multi_line(&natural));
        let rounded = natural.size[0].floor();
        assert!(rounded < natural.size[0], "pick a size whose width is fractional");

        let in_box = sys.shape(&text, 1.0, Some(rounded), &loc);
        assert!(!is_multi_line(&in_box), "wrapped in a {rounded} px box");
        assert_eq!(in_box.size, natural.size);
        assert_eq!(in_box.glyphs.len(), natural.glyphs.len());
        assert_eq!(in_box.shaped_width, rounded, "cache key keeps the box width");

        // Two pixels short is a real overflow: word-or-glyph wrapping still applies.
        let narrow = sys.shape(&text, 1.0, Some(natural.size[0] - 2.0), &loc);
        assert!(is_multi_line(&narrow), "a box 2 px too narrow must still wrap");
    }

    #[test]
    fn arabic_rtl_start_aligns_every_line_when_wrapped() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        // Four words: wrapping in a narrow box must split them across lines.
        let text = Text::new("مرحبا بالعالم مع التشكيل").with_wrap(WrapMode::Word);
        let max_w = 150.0f32;
        let shaped = sys.shape(&text, 1.0, Some(max_w), &loc);
        assert!(!shaped.glyphs.is_empty(), "expected shaped glyphs");

        // Group glyphs into lines by their baseline y, preserving draw order.
        let mut lines: Vec<(i32, Vec<i32>)> = Vec::new();
        for g in &shaped.glyphs {
            match lines.iter_mut().find(|(y, _)| *y == g.pen[1]) {
                Some((_, xs)) => xs.push(g.pen[0]),
                None => lines.push((g.pen[1], vec![g.pen[0]])),
            }
        }
        assert!(
            lines.len() >= 2,
            "expected the paragraph to wrap to ≥2 lines at {max_w}px, got {}",
            lines.len()
        );
        // Each RTL line starts (visually first glyph = rightmost ink) at the right edge.
        for (y, xs) in &lines {
            let first_x = *xs.first().expect("line has glyphs");
            assert!(
                first_x as f32 > max_w - 90.0,
                "line at y={y} starts at x={first_x}, not near the right edge {max_w}"
            );
            // Right edge = the line's first glyph; its leftmost ink sits inside the box.
            assert!(
                xs.iter().all(|x| *x >= -1),
                "line at y={y} has ink before the left edge"
            );
        }
    }

    /// UAX #14 class SA (Thai; Lao, Khmer and Myanmar behave the same) writes words without
    /// spaces, so the shaper's word wrap has no break opportunities of its own and falls back to
    /// mid-word glyph breaks. Regia inserts zero-width break opportunities at dictionary word
    /// boundaries (ADR-3323). The fixture's words are hand-checked Thai vocabulary (ฉัน "I",
    /// กิน "eat", ข้าว "rice", วันนี้ "today", อากาศ "weather", ดี "good"); their boundaries are
    /// exactly what the engine's ICU-based segmenter produces (docs/provenance.md).
    /// Probe: break markers must be zero-width — the shaped width of a Thai sentence equals the
    /// sum of its words' widths (Thai has no cross-word kerning; any ZWSP advance would show).
    #[test]
    fn sa_markers_are_zero_width() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let w = |sys: &mut TextSystem, s: &str| sys.shape(&Text::new(s), 1.0, None, &loc).size[0];
        let whole = w(&mut sys, "ฉันกินข้าว");
        let parts = w(&mut sys, "ฉัน") + w(&mut sys, "กิน") + w(&mut sys, "ข้าว");
        assert!(
            (whole - parts).abs() < 0.5,
            "break markers must add no advance: whole={whole} parts={parts}"
        );
    }

    #[test]
    fn thai_wraps_only_at_dictionary_word_boundaries() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        let words = ["ฉัน", "กิน", "ข้าว", "วัน", "นี้", "อากาศ", "ดี"];
        let thai: String = words.concat();
        let w = |sys: &mut TextSystem, s: &str| sys.shape(&Text::new(s), 1.0, None, &loc).size[0];
        // Wide enough for the widest word alone; greedy groups shorter words per line.
        let width = w(&mut sys, words[5]) + 1.0;
        for word in words {
            assert!(
                w(&mut sys, word) <= width,
                "fixture: {word} must fit the box alone"
            );
        }

        let text = Text::new(thai.clone()).with_wrap(WrapMode::Word);
        let shaped = sys.shape(&text, 1.0, Some(width), &loc);
        let string = shaped_string(&text, &loc);
        let paragraphs = text_paragraphs(&string);
        let lines: Vec<&str> = shaped
            .lines
            .iter()
            .map(|l| {
                paragraphs
                    .get(l.paragraph as usize)
                    .and_then(|p| p.get(l.range[0] as usize..l.range[1] as usize))
                    .unwrap_or("")
            })
            .collect();
        assert!(
            shaped.lines.len() >= 3,
            "expected the sentence to wrap to at least three lines, got {lines:?}"
        );
        // Every line is a non-empty run of consecutive whole words, and the lines concatenate
        // to the sentence: breaks fall only at dictionary word boundaries.
        let mut pos = 0;
        for line in &lines {
            let mut rest = *line;
            assert!(!rest.is_empty(), "no empty lines in {lines:?}");
            while !rest.is_empty() {
                let word = words
                    .iter()
                    .find(|word| rest.starts_with(**word))
                    .unwrap_or_else(|| panic!("{rest:?} of line {line:?} starts mid-word"));
                assert_eq!(&thai[pos..pos + word.len()], *word, "words stay in order");
                pos += word.len();
                rest = &rest[word.len()..];
            }
        }
        assert_eq!(pos, thai.len(), "the lines concatenate to the sentence");
        for line in &shaped.lines {
            assert!(
                line.width <= width + f32::EPSILON,
                "line wider than the box: {line:?}"
            );
        }
    }

    /// Break markers must never leak into the source string: the caret map indexes the original
    /// text, mixed Latin + Thai wraps at word boundaries in both scripts, and hit testing lands
    /// on the original byte offsets.
    #[test]
    fn thai_carets_and_hits_index_the_original_string() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        // Words: save | ฉัน | กิน | ข้าว | now — with no spaces inside the Thai run, as Thai
        // writes it (fixture words hand-checked, ADR-3323).
        let mixed = format!("save {} ข้าว now", "ฉันกิน");
        assert_eq!(mixed.find('ข'), Some(24));
        let w = |sys: &mut TextSystem, s: &str| sys.shape(&Text::new(s), 1.0, None, &loc).size[0];
        // Wide enough for "save ฉันกิน", too narrow for "save ฉันกินข้าว".
        let width = w(&mut sys, "save ฉันกิน") + 2.0;
        assert!(
            width < w(&mut sys, "save ฉันกินข้าว"),
            "fixture width must not fit the Thai run"
        );

        let text = Text::new(mixed.clone()).with_wrap(WrapMode::Word);
        let shaped = sys.shape(&text, 1.0, Some(width), &loc);
        assert!(shaped.lines.len() >= 2, "expected the paragraph to wrap");
        let map = sys.caret_map(&text, 1.0, Some(width), &loc);
        assert_eq!(map.text, mixed, "the caret map must index the source string");

        // Word-boundary byte offsets of the fixture: save 0..4, ฉัน 5..14, กิน 14..23,
        // ข้าว 24..36, now 37..40.
        let bounds = [0, 5, 14, 23, 24, 37, 40];
        for line in &map.lines {
            assert!(
                bounds.contains(&line.start),
                "line starts mid-word at byte {} of {mixed:?}",
                line.start
            );
            assert!(
                bounds.contains(&line.end),
                "line ends mid-word at byte {} of {mixed:?}",
                line.end
            );
        }
        // Hitting just inside the second visual line lands at ข้าว's byte offset in the source.
        let khao_at = mixed.find('ข').unwrap_or(0);
        let Some(line_two) = map.lines.get(1) else {
            panic!("expected a second line");
        };
        let hit = map.hit_line(
            1,
            line_two.clusters.first().map_or(line_two.origin_x, |c| c.x0) + 1.0,
        );
        assert_eq!(hit, khao_at, "hit must land on ข้าว's first byte");
    }

    /// `caret_map` must follow the *normalized* paragraph structure: `normalize_line_breaks`
    /// turns CR, CRLF, NEL (U+0085), U+2028 and U+2029 into `\n`, and the shaped buffer breaks
    /// paragraphs there, while the caret map indexes the raw source string. Scanning the raw
    /// string for `\n` alone missed those breaks, so every cluster of the second paragraph fell
    /// back to `full.len()` and reported offsets past the end of the text (rc1 finding).
    #[test]
    fn caret_map_breaks_paragraphs_at_normalized_separators() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        for raw in ["a\u{2028}b", "a\u{2029}b", "a\rb", "a\u{0085}b", "a\r\nb", "a\nb"] {
            let text = Text::new(raw);
            let shaped = sys.shape(&text, 1.0, None, &loc);
            assert_eq!(
                shaped.lines.len(),
                2,
                "{raw:?}: the separator must break the paragraph"
            );
            let map = sys.caret_map(&text, 1.0, None, &loc);
            assert_eq!(map.text, raw, "the caret map must index the source string");
            for line in &map.lines {
                for c in &line.clusters {
                    assert!(
                        c.start <= c.end && c.end <= map.text.len(),
                        "{raw:?}: cluster {c:?} indexes past the end of the text"
                    );
                }
            }
            // 'b' keeps its raw byte offset on the second line, and a hit on that line lands on it.
            let b_at = raw.rfind('b').unwrap_or(0);
            let Some(line_two) = map.lines.get(1) else {
                panic!("{raw:?}: expected a second line");
            };
            assert_eq!(
                (line_two.start, line_two.end),
                (b_at, raw.len()),
                "{raw:?}: the second line must cover 'b' at its raw offset"
            );
            let hit = map.hit_line(
                1,
                line_two.clusters.first().map_or(line_two.origin_x, |c| c.x0) + 1.0,
            );
            assert_eq!(hit, b_at, "{raw:?}: the hit must land on 'b'");
        }
    }

    /// Lao, Khmer and Myanmar wrap at word boundaries too (the same UAX #14 class SA; the
    /// segmenter ships data for all four). Fixture words: Lao ຂ້າພະເຈົ້າ "I" + ກິນ "eat" +
    /// ເຂົ້າ "rice"; Khmer ខ្ញុំ "I" + ញ៉ាំ "eat" + បាយ "rice"; Myanmar ကျွန်တော် "I" + စား
    /// "eat" + ဘောင်... no — ဆန် "rice". Sources: common vocabulary, ICU segmentation.
    #[test]
    fn lao_khmer_myanmar_wrap_at_word_boundaries() {
        let mut sys = TextSystem::embedded_only();
        let loc = Localization::default();
        for (words, name) in [
            (vec!["ຂ້າພະເຈົ້າ", "ກິນ", "ເຂົ້າ"], "Lao"),
            (vec!["ខ្ញុំ", "ញ៉ាំ", "បាយ"], "Khmer"),
            (vec!["ကျွန်တော်", "စား", "ဆန်"], "Myanmar"),
        ] {
            let text_str: String = words.concat();
            let w = |sys: &mut TextSystem, s: &str| sys.shape(&Text::new(s), 1.0, None, &loc).size[0];
            let width = w(&mut sys, words[0]) + w(&mut sys, words[1]) + 2.0;
            assert!(
                width < w(&mut sys, words[0]) + w(&mut sys, words[1]) + w(&mut sys, words[2]),
                "{name}: fixture width must not fit all three words"
            );
            let text = Text::new(text_str).with_wrap(WrapMode::Word);
            let shaped = sys.shape(&text, 1.0, Some(width), &loc);
            let string = shaped_string(&text, &loc);
            let paragraphs = text_paragraphs(&string);
            let lines: Vec<&str> = shaped
                .lines
                .iter()
                .map(|l| {
                    paragraphs
                        .get(l.paragraph as usize)
                        .and_then(|p| p.get(l.range[0] as usize..l.range[1] as usize))
                        .unwrap_or("")
                })
                .collect();
            let expected: Vec<String> = vec![words[0].to_string() + words[1], words[2].to_string()];
            assert_eq!(
                lines,
                expected.iter().map(String::as_str).collect::<Vec<_>>(),
                "{name}: each line must hold whole words"
            );
        }
    }

    #[test]
    fn han_variant_follows_the_language_and_region() {
        let cases = [
            ("ja", HanVariant::Japanese),
            ("ja-JP", HanVariant::Japanese),
            ("ko_KR", HanVariant::Korean),
            ("zh-Hant", HanVariant::Traditional),
            ("zh-TW", HanVariant::Traditional),
            ("zh-Hant-HK", HanVariant::HongKong),
            ("zh-HK", HanVariant::HongKong),
            ("zh-Hans", HanVariant::Simplified),
            ("zh-CN", HanVariant::Simplified),
            ("zh", HanVariant::Simplified),
            ("en-US", HanVariant::Simplified),
            ("", HanVariant::Simplified),
        ];
        for (tag, variant) in cases {
            assert_eq!(HanVariant::of_locale(tag), variant, "{tag}");
        }
    }

    #[test]
    fn han_fallback_lists_follow_the_locale_where_the_platform_has_regional_faces() {
        let fallback = BundledFallback;
        let first = |locale: &str| {
            ct::Fallback::script_fallback(&fallback, Script::Han, locale)
                .first()
                .copied()
        };
        // Every variant keeps the bundled subset in its list (coverage never depends on the OS).
        for locale in ["en", "ja", "ko", "zh-TW", "zh-HK"] {
            assert!(
                ct::Fallback::script_fallback(&fallback, Script::Han, locale).contains(&"Noto Sans SC"),
                "{locale}"
            );
        }
        assert_eq!(first("en"), Some("Noto Sans SC"));
        assert_eq!(first("zh-Hans"), Some("Noto Sans SC"));
        if cfg!(target_os = "android") {
            // No system fonts on Android: the bundled subset for everyone.
            assert_eq!(first("ja"), Some("Noto Sans SC"));
        } else {
            assert_ne!(first("ja"), Some("Noto Sans SC"));
            assert_ne!(first("ko"), first("ja"));
            assert_ne!(first("zh-TW"), first("zh-HK"));
        }
        // Kana follow the Han rule; Hangul does not depend on the locale.
        assert_eq!(
            ct::Fallback::script_fallback(&fallback, Script::Katakana, "ja"),
            ct::Fallback::script_fallback(&fallback, Script::Han, "ja")
        );
        assert_eq!(
            ct::Fallback::script_fallback(&fallback, Script::Hangul, "ja"),
            ct::Fallback::script_fallback(&fallback, Script::Hangul, "en")
        );
    }

    #[test]
    fn the_font_locale_switches_and_shaping_still_works() {
        let mut fonts = TextSystem::embedded_only();
        let start = fonts.locale().to_owned();
        assert!(!fonts.set_locale(&start), "same locale: nothing to do");
        assert!(!fonts.set_locale(""), "empty tag: nothing to do");
        assert!(fonts.set_locale("ja"));
        assert_eq!(fonts.locale(), "ja");
        assert_eq!(fonts.font_system.locale(), "ja");
        // The database survived: the bundled faces are still there.
        assert!(fonts.font_system.db().faces().count() >= 7);
        assert!(fonts.set_locale("fr-CA"));
        assert_eq!(fonts.font_system.locale(), "fr-CA");
    }

    #[test]
    fn sync_follows_the_game_locale_not_the_os() {
        let sources = vec![
            ("en".to_owned(), "hello = Hello\n".to_owned()),
            ("ja".to_owned(), "hello = こんにちは\n".to_owned()),
        ];
        let mut app = App::new();
        app.insert_resource(Localization::new(sources, Some("en".to_owned())).expect("loads"))
            .insert_resource(TextSystem::embedded_only())
            .add_systems(Update, sync_font_locale);
        app.update();
        assert_eq!(app.world().resource::<TextSystem>().locale(), "en");
        assert!(app.world_mut().resource_mut::<Localization>().set_locale("ja"));
        app.update();
        assert_eq!(app.world().resource::<TextSystem>().locale(), "ja");
        assert_eq!(app.world().resource::<TextSystem>().font_system.locale(), "ja");
    }

    /// v0.5.1 (DO27 tester B): the font database's per-file warnings during a scan are counted,
    /// not logged (they name a user's font paths); the scan's own result comes back.
    #[test]
    fn font_scan_warnings_are_counted_not_logged() {
        let (value, n) = count_font_scan_warnings(|| {
            tracing::warn!(target: "fontdb", "Failed to load a font face 0 from source cause malformed font.");
            tracing::warn!(target: "fontdb::loader", "another skipped file");
            tracing::info!(target: "fontdb", "not a failure");
            tracing::warn!(target: "regia_ui", "not the font database");
            7
        });
        assert_eq!((value, n), (7, 2));
        let ((), none) = count_font_scan_warnings(|| ());
        assert_eq!(none, 0);
    }
}
