// SPDX-License-Identifier: MPL-2.0
//! Text effects (ADR-273 T07, ADR-2045): outline and shadow layers stacked under the glyphs,
//! underline and strikethrough rects, letter spacing, explicit weights and OpenType features.
//!
//! Everything here is off by default: a [`crate::text::TextStyle`] without effects, decorations,
//! spacing, weight or features shapes and draws exactly as before. The shaping half (spacing,
//! weight, features, decorations) goes through cosmic-text's own attributes; the drawn half is
//! pure CPU geometry ([`effect_quad`], `decoration_rects`) plus one overlay shader kind (7)
//! that dilates or blurs a glyph's atlas coverage (`ui-overlay.wgsl`).

use cosmic_text as ct;
use serde::{Deserialize, Serialize};

use crate::color::UiColor;
use crate::draw::{TextureRef, UiQuad};

/// Largest outline width or shadow blur in physical px; larger values clamp. The overlay shader
/// walks a square of `(2 * reach + 1)^2` atlas taps, so the cap bounds the per-pixel cost.
pub const TEXT_EFFECT_MAX_PX: f32 = 8.0;

/// Most effect layers one span draws; extra layers are ignored.
pub const MAX_TEXT_EFFECTS: usize = 4;

/// Most OpenType features one span sets; extra entries are ignored.
pub const MAX_FONT_FEATURES: usize = 16;

/// One effect layer drawn under a span's glyphs: an outline (`width > 0`), a shadow (an
/// `offset`, usually with `blur`), or both (a glow is a wide soft outline).
///
/// A span's [`crate::text::TextStyle::effects`] stack front to back: the first layer sits right
/// under the glyphs, later layers further below, like a list of CSS text shadows. Every layer of
/// a text block draws before any glyph of it, so one glyph's outline never covers its neighbor.
/// Effects apply to monochrome glyphs; color emoji draw without them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextEffect {
    /// Layer color (linear, straight alpha).
    pub color: UiColor,
    /// Offset from the glyphs in UI px (x right, y down); snapped to whole physical px.
    pub offset: [f32; 2],
    /// Dilation in UI px: the outline width. 0 = no dilation.
    pub width: f32,
    /// Softness in UI px. With `width == 0` the layer is the glyph blurred by a Gaussian of
    /// σ = blur / 2 (CSS `text-shadow` convention); with `width > 0` it widens the outline's
    /// edge ramp (a glow).
    pub blur: f32,
}

impl Default for TextEffect {
    fn default() -> Self {
        Self {
            color: UiColor([0.0, 0.0, 0.0, 1.0]),
            offset: [0.0, 0.0],
            width: 0.0,
            blur: 0.0,
        }
    }
}

impl TextEffect {
    /// A hard outline of `width` UI px.
    pub fn outline(width: f32, color: UiColor) -> Self {
        Self {
            color,
            width,
            ..Self::default()
        }
    }

    /// A drop shadow at `offset` UI px, blurred by `blur` UI px.
    pub fn shadow(offset: [f32; 2], blur: f32, color: UiColor) -> Self {
        Self {
            color,
            offset,
            blur,
            ..Self::default()
        }
    }

    /// The layer in physical px at `scale`, with width and blur clamped to
    /// [`TEXT_EFFECT_MAX_PX`], non-finite values zeroed and the offset rounded to whole px.
    pub fn physical(&self, scale: f32) -> Self {
        let px = |v: f32| {
            let v = v * scale;
            if v.is_finite() { v } else { 0.0 }
        };
        Self {
            color: self.color,
            offset: [px(self.offset[0]).round(), px(self.offset[1]).round()],
            width: px(self.width).clamp(0.0, TEXT_EFFECT_MAX_PX),
            blur: px(self.blur).clamp(0.0, TEXT_EFFECT_MAX_PX),
        }
    }

    /// How many whole px the layer reaches past the glyph box (the shader's tap radius): a pure
    /// blur reaches 3σ = 1.5 × blur, an outline its width plus its edge ramp. Expects a
    /// [`Self::physical`] layer.
    pub fn reach(&self) -> u32 {
        let r = if self.width <= 0.0 {
            self.blur * 1.5
        } else {
            self.width + self.blur
        };
        (r.ceil() as u32).min(TEXT_EFFECT_MAX_PX as u32)
    }

    /// True when the layer draws nothing visible (transparent, or no width, blur or offset).
    pub fn is_noop(&self) -> bool {
        self.color.0[3] <= 0.0 || (self.width <= 0.0 && self.blur <= 0.0 && self.offset == [0.0, 0.0])
    }
}

/// The effect quad for one glyph: the glyph quad grown by the layer's reach on every side and
/// moved by its offset, sampling the same atlas page through overlay kind 7.
///
/// `glyph` is the plain glyph quad ([`TextureRef::MonoAtlas`]); `effect` a [`TextEffect::physical`]
/// layer; `page_px` the atlas page size. The quad's `corner_radius` carries the glyph's own uv
/// rect (taps outside it read zero, so neighbors in the atlas never bleed in) and
/// `border_width[0..2]` the width and blur in px. Returns `None` for color glyphs and no-op
/// layers.
pub fn effect_quad(glyph: &UiQuad, effect: &TextEffect, page_px: f32) -> Option<UiQuad> {
    let TextureRef::MonoAtlas(page) = glyph.texture else {
        return None;
    };
    if effect.is_noop() {
        return None;
    }
    let m = effect.reach() as f32;
    let du = m / page_px.max(1.0);
    Some(UiQuad {
        pos: [
            glyph.pos[0] - m + effect.offset[0],
            glyph.pos[1] - m + effect.offset[1],
        ],
        size: [glyph.size[0] + 2.0 * m, glyph.size[1] + 2.0 * m],
        uv: [
            glyph.uv[0] - du,
            glyph.uv[1] - du,
            glyph.uv[2] + du,
            glyph.uv[3] + du,
        ],
        color: effect.color,
        color2: effect.color,
        corner_radius: glyph.uv,
        border_width: [effect.width, effect.blur, 0.0, 0.0],
        texture: TextureRef::GlyphEffect(page),
        ..*glyph
    })
}

/// The physical effect layers of each span of `text` at `scale`, or an empty list when no span
/// has a visible layer (the common case: extract then draws glyphs exactly as before).
pub fn span_effects(text: Option<&crate::text::Text>, scale: f32) -> Vec<Vec<TextEffect>> {
    let Some(text) = text else {
        return Vec::new();
    };
    if text.spans.iter().all(|s| s.style.effects.is_empty()) {
        return Vec::new();
    }
    let layers: Vec<Vec<TextEffect>> = text
        .spans
        .iter()
        .map(|s| {
            s.style
                .effects
                .iter()
                .take(MAX_TEXT_EFFECTS)
                .map(|e| e.physical(scale))
                .filter(|e| !e.is_noop())
                .collect()
        })
        .collect();
    if layers.iter().all(Vec::is_empty) {
        Vec::new()
    } else {
        layers
    }
}

/// Pushes a text block's quads in effect order: the deepest layer of every glyph first, then
/// the next one up, then the glyphs themselves. `glyphs` holds `(glyph index, glyph quad)`;
/// `glyph_spans` maps glyph index → span ([`crate::text::ShapedText::glyph_spans`]); `finish`
/// clips a quad and stamps its z and alpha (returning `None` when it is clipped away).
pub fn push_effect_layers(
    layers: &[Vec<TextEffect>],
    glyphs: &[(usize, UiQuad)],
    glyph_spans: &[u16],
    page_px: f32,
    out: &mut Vec<UiQuad>,
    finish: impl Fn(UiQuad) -> Option<UiQuad>,
) {
    let depth = layers.iter().map(Vec::len).max().unwrap_or(0);
    let span_of = |index: usize| usize::from(glyph_spans.get(index).copied().unwrap_or(0));
    for level in (0..depth).rev() {
        for (index, glyph) in glyphs {
            let Some(effect) = layers.get(span_of(*index)).and_then(|l| l.get(level)) else {
                continue;
            };
            if let Some(q) = effect_quad(glyph, effect, page_px).and_then(&finish) {
                out.push(q);
            }
        }
    }
    for (_, glyph) in glyphs {
        if let Some(q) = finish(*glyph) {
            out.push(q);
        }
    }
}

/// A drawn text decoration: an underline or strikethrough rect in physical px relative to the
/// text block's top-left, with the color and span it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecorationRect {
    /// x, y, w, h in physical px from the block origin.
    pub rect: [f32; 4],
    /// Line color (the span's color unless the decoration names one).
    pub color: UiColor,
    /// Index of the span the decoration belongs to.
    pub span: u16,
    /// True for a strikethrough (drawn over the glyphs), false for an underline.
    pub strike: bool,
}

/// Parses `"smcp"`, `"tnum"` or `"liga=0"` style OpenType feature settings (a four-character
/// ASCII tag, optionally `=` and a value). Invalid entries are skipped; at most
/// [`MAX_FONT_FEATURES`] are kept.
pub fn parse_features(features: &[String]) -> ct::FontFeatures {
    let mut out = ct::FontFeatures::new();
    for raw in features.iter().take(MAX_FONT_FEATURES) {
        if let Some((tag, value)) = parse_feature(raw) {
            out.set(ct::FeatureTag::new(&tag), value);
        }
    }
    out
}

/// One feature setting as `(tag, value)`, or `None` when it is not a four-character printable
/// ASCII tag with an optional `=<u32>` value.
pub fn parse_feature(raw: &str) -> Option<([u8; 4], u32)> {
    let (tag, value) = match raw.split_once('=') {
        Some((t, v)) => (t.trim(), v.trim().parse::<u32>().ok()?),
        None => (raw.trim(), 1),
    };
    let bytes: [u8; 4] = tag.as_bytes().try_into().ok()?;
    if !bytes.iter().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    Some((bytes, value))
}

/// The underline and strikethrough rects of one cosmic-text layout run, in the run's own
/// coordinates (the shaper adds nothing else). `span_color(span)` gives a span's color for
/// decorations that name none; `line_y` is the baseline the glyphs were placed on.
pub(crate) fn decoration_rects(
    run: &ct::LayoutRun<'_>,
    line_y: f32,
    span_color: impl Fn(usize) -> UiColor,
    decode: impl Fn(ct::Color) -> UiColor,
    out: &mut Vec<DecorationRect>,
) {
    for deco in run.decorations {
        let glyphs = run.glyphs.get(deco.glyph_range.clone()).unwrap_or(&[]);
        let Some(first) = glyphs.first() else {
            continue;
        };
        // RTL runs store glyphs right to left: take the extent over all of them.
        let (mut x0, mut x1) = (f32::INFINITY, f32::NEG_INFINITY);
        for g in glyphs {
            x0 = x0.min(g.x);
            x1 = x1.max(g.x + g.w);
        }
        if x1 - x0 <= 0.0 || !x0.is_finite() {
            continue;
        }
        let span = first.metadata;
        let color = |own: Option<ct::Color>| own.map_or_else(|| span_color(span), &decode);
        let span16 = u16::try_from(span).unwrap_or(u16::MAX);
        let td = &deco.data.text_decoration;
        let size = deco.font_size;
        if !matches!(td.underline, ct::UnderlineStyle::None) {
            let m = deco.data.underline_metrics;
            let h = (m.thickness * size).max(1.0).ceil();
            let y = (line_y - m.offset * size).round();
            out.push(DecorationRect {
                rect: [x0.round(), y, (x1 - x0).round().max(1.0), h],
                color: color(td.underline_color_opt),
                span: span16,
                strike: false,
            });
        }
        if td.strikethrough {
            let m = deco.data.strikethrough_metrics;
            let h = (m.thickness * size).max(1.0).ceil();
            let y = (line_y - m.offset * size).round();
            out.push(DecorationRect {
                rect: [x0.round(), y, (x1 - x0).round().max(1.0), h],
                color: color(td.strikethrough_color_opt),
                span: span16,
                strike: true,
            });
        }
    }
}

/// Pure model of what overlay kind 7 computes for one output pixel, for tests and tools:
/// `sample(dx, dy)` is the glyph coverage at a tap offset (0 outside the glyph). With
/// `width == 0` and `blur > 0` it is a Gaussian average (σ = blur / 2); otherwise the
/// maximum of the taps weighted by an edge ramp falling from 1 at `width` to 0 at
/// `width + blur + 1` px.
pub fn effect_coverage(effect: &TextEffect, sample: impl Fn(i32, i32) -> f32) -> f32 {
    let reach = effect.reach() as i32;
    let blur_only = effect.width <= 0.0 && effect.blur > 0.0;
    let sigma = (effect.blur * 0.5).max(1e-3);
    let (mut best, mut acc, mut total) = (0.0f32, 0.0f32, 0.0f32);
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let a = sample(dx, dy);
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if blur_only {
                let w = (-(d * d) / (2.0 * sigma * sigma)).exp();
                acc += a * w;
                total += w;
            } else {
                let k = ((effect.width + effect.blur + 1.0 - d) / (effect.blur + 1.0)).clamp(0.0, 1.0);
                best = best.max(a * k);
            }
        }
    }
    if blur_only {
        if total > 0.0 {
            (acc / total).clamp(0.0, 1.0)
        } else {
            0.0
        }
    } else {
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_parse_tags_and_values() {
        assert_eq!(parse_feature("smcp"), Some((*b"smcp", 1)));
        assert_eq!(parse_feature("liga=0"), Some((*b"liga", 0)));
        assert_eq!(parse_feature(" tnum = 1 "), Some((*b"tnum", 1)));
        assert_eq!(parse_feature("toolong"), None);
        assert_eq!(parse_feature("ab c"), None);
        assert_eq!(parse_feature("liga=x"), None);
        let many: Vec<String> = (0..40).map(|_| "kern".to_owned()).collect();
        assert_eq!(parse_features(&many).features.len(), MAX_FONT_FEATURES);
    }

    #[test]
    fn physical_clamps_and_snaps() {
        let e = TextEffect {
            color: UiColor::WHITE,
            offset: [1.3, -2.6],
            width: 100.0,
            blur: f32::NAN,
        }
        .physical(2.0);
        assert_eq!(e.offset, [3.0, -5.0]);
        assert_eq!(e.width, TEXT_EFFECT_MAX_PX);
        assert_eq!(e.blur, 0.0);
        assert_eq!(e.reach(), TEXT_EFFECT_MAX_PX as u32);
    }

    #[test]
    fn reach_covers_blur_and_outline() {
        assert_eq!(TextEffect::shadow([2.0, 2.0], 4.0, UiColor::WHITE).reach(), 6);
        assert_eq!(TextEffect::outline(2.0, UiColor::WHITE).reach(), 2);
        let glow = TextEffect {
            width: 1.5,
            blur: 2.0,
            ..TextEffect::default()
        };
        assert_eq!(glow.reach(), 4);
        assert!(TextEffect::default().is_noop());
        assert!(!TextEffect::shadow([1.0, 0.0], 0.0, UiColor::WHITE).is_noop());
    }

    #[test]
    fn effect_quad_grows_the_glyph_and_keeps_its_uv_rect() {
        let glyph = UiQuad {
            pos: [10.0, 20.0],
            size: [8.0, 12.0],
            uv: [0.25, 0.5, 0.3, 0.6],
            texture: TextureRef::MonoAtlas(2),
            ..UiQuad::fill([10.0, 20.0], [8.0, 12.0], UiColor::WHITE)
        };
        let e = TextEffect::outline(2.0, UiColor([1.0, 0.0, 0.0, 1.0]));
        let q = effect_quad(&glyph, &e, 1000.0).expect("mono glyph");
        assert_eq!(q.pos, [8.0, 18.0]);
        assert_eq!(q.size, [12.0, 16.0]);
        assert_eq!(q.corner_radius, glyph.uv);
        assert_eq!(q.texture, TextureRef::GlyphEffect(2));
        assert!((q.uv[0] - 0.248).abs() < 1e-6 && (q.uv[3] - 0.602).abs() < 1e-6);
        assert_eq!(q.border_width, [2.0, 0.0, 0.0, 0.0]);
        let s = effect_quad(
            &glyph,
            &TextEffect::shadow([3.0, 4.0], 0.0, UiColor::WHITE),
            1000.0,
        )
        .expect("shadow");
        assert_eq!((s.pos, s.size), ([13.0, 24.0], glyph.size));
        let color = UiQuad {
            texture: TextureRef::ColorAtlas(0),
            ..glyph
        };
        assert!(effect_quad(&color, &e, 1000.0).is_none());
        assert!(effect_quad(&glyph, &TextEffect::default(), 1000.0).is_none());
    }

    #[test]
    fn layers_draw_deepest_first_then_glyphs() {
        let glyph = |x: f32| UiQuad {
            texture: TextureRef::MonoAtlas(0),
            uv: [0.0, 0.0, 0.01, 0.01],
            ..UiQuad::fill([x, 0.0], [4.0, 4.0], UiColor::WHITE)
        };
        let red = UiColor([1.0, 0.0, 0.0, 1.0]);
        let blue = UiColor([0.0, 0.0, 1.0, 1.0]);
        // Span 0: outline (front) + shadow (back); span 1: no effects.
        let layers = vec![
            vec![
                TextEffect::outline(1.0, red),
                TextEffect::shadow([2.0, 2.0], 0.0, blue),
            ],
            vec![],
        ];
        let glyphs = vec![(0, glyph(0.0)), (1, glyph(10.0)), (2, glyph(20.0))];
        let spans = [0u16, 0, 1];
        let mut out = Vec::new();
        push_effect_layers(&layers, &glyphs, &spans, 1000.0, &mut out, Some);
        let kinds: Vec<(TextureRef, UiColor)> = out.iter().map(|q| (q.texture, q.color)).collect();
        assert_eq!(
            kinds,
            vec![
                (TextureRef::GlyphEffect(0), blue),
                (TextureRef::GlyphEffect(0), blue),
                (TextureRef::GlyphEffect(0), red),
                (TextureRef::GlyphEffect(0), red),
                (TextureRef::MonoAtlas(0), UiColor::WHITE),
                (TextureRef::MonoAtlas(0), UiColor::WHITE),
                (TextureRef::MonoAtlas(0), UiColor::WHITE),
            ]
        );
    }

    #[test]
    fn span_effects_is_empty_for_plain_text() {
        let plain = crate::text::Text::new("plain");
        assert!(span_effects(Some(&plain), 1.0).is_empty());
        assert!(span_effects(None, 1.0).is_empty());
        let mut styled = crate::text::Text::new("styled");
        styled.spans[0].style.effects = vec![TextEffect::default(), TextEffect::outline(1.0, UiColor::WHITE)];
        let layers = span_effects(Some(&styled), 2.0);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 1, "the no-op layer is dropped");
        assert_eq!(layers[0][0].width, 2.0);
    }

    /// A one-pixel dot: an outline of width 2 covers the disk of radius 2 around it fully, ramps
    /// to zero one px further out; a pure blur spreads it into a normalized Gaussian.
    #[test]
    fn coverage_model_dilates_and_blurs() {
        let dot = |dx: i32, dy: i32| if dx == 0 && dy == 0 { 1.0 } else { 0.0 };
        // The model evaluates at the output pixel; shift the dot instead of the pixel.
        let at = |px: i32, py: i32, e: &TextEffect| effect_coverage(e, |dx, dy| dot(px + dx, py + dy));
        let outline = TextEffect::outline(2.0, UiColor::WHITE);
        assert_eq!(at(0, 0, &outline), 1.0);
        assert_eq!(at(2, 0, &outline), 1.0);
        assert!((at(2, 1, &outline) - (3.0 - 5f32.sqrt())).abs() < 1e-5);
        assert_eq!(at(3, 0, &outline), 0.0);
        let blur = TextEffect::shadow([0.0, 0.0], 2.0, UiColor::WHITE);
        let center = at(0, 0, &blur);
        assert!(center > at(1, 0, &blur) && at(1, 0, &blur) > at(2, 0, &blur));
        assert!(center < 1.0 && center > 0.0);
        // Coverage of a full field stays 1 under a blur (normalized kernel).
        assert!((effect_coverage(&blur, |_, _| 1.0) - 1.0).abs() < 1e-5);
    }
}
