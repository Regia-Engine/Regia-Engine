// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/text.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! The **Text** primitive: nannou's text drawing API, shaped with `regia_ui`'s cosmic-text
//! [`TextSystem`] and drawn as glyph quads from a glyph atlas.

use std::ops::Range;

use regia_core::math::geom::Align;
use regia_ui::Localization;
use regia_ui::atlas::{GlyphAtlases, PAGE_SIZE};
use regia_ui::text::{Text as UiText, TextAlign, TextSpan, TextStyle as UiTextStyle, TextSystem, WrapMode};

use crate::draw::drawing::DrawingContext;
use crate::draw::primitive::Primitive;
use crate::draw::properties::spatial::{self, dimension, orientation, position};
use crate::draw::properties::{SetColor, SetDimensions, SetOrientation, SetPosition};
use crate::draw::{self, Drawing};
use crate::prelude_internal::*;

/// Font size in logical units.
pub type FontSize = u32;

/// How text wraps inside its rectangle.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Wrap {
    /// Wrap at word boundaries (glyphs when a word is longer than the line).
    #[default]
    Whitespace,
    /// Wrap at any character.
    Character,
}

/// Horizontal alignment of each line inside the text rectangle.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Justify {
    /// Start edge (`TextAlign::Start`): the left edge for left-to-right text, the right edge for
    /// right-to-left text.
    Left,
    /// Centred.
    #[default]
    Center,
    /// End edge (`TextAlign::End`): the right edge for left-to-right text, the left edge for
    /// right-to-left text.
    Right,
}

/// Default font size.
pub const DEFAULT_FONT_SIZE: FontSize = 12;

/// Text layout parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Extra space between lines, logical units.
    pub line_spacing: f32,
    /// `None`: one line per `\n`, never wrapped.
    pub line_wrap: Option<Wrap>,
    /// Horizontal alignment.
    pub justify: Justify,
    /// Font size, logical units.
    pub font_size: FontSize,
    /// Font family name (`None`: the default sans-serif stack).
    pub font_family: Option<String>,
    /// Vertical alignment of the text block inside the rectangle.
    pub y_align: Align,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            line_spacing: 0.0,
            line_wrap: Some(Wrap::Whitespace),
            justify: Justify::Center,
            font_size: DEFAULT_FONT_SIZE,
            font_family: None,
            y_align: Align::Middle,
        }
    }
}

/// Styling properties for the **Text** primitive.
#[derive(Clone, Debug, Default)]
pub struct Style {
    /// Colour of every glyph (theme default when `None`). Colour glyphs such as emoji keep their
    /// own colours; only the alpha of this colour applies to them.
    pub color: Option<Color>,
    /// Per-glyph colours, overriding `color` for the first glyphs.
    pub glyph_colors: Vec<Color>,
    /// Layout.
    pub layout: Layout,
}

/// Properties related to drawing the **Text** primitive.
#[derive(Clone, Debug)]
pub struct Text {
    spatial: spatial::Properties,
    style: Style,
    // The byte range into the `Draw` context's text buffer.
    text: Range<usize>,
}

/// The drawing context for the **Text** primitive.
pub type DrawingText<'a> = Drawing<'a, Text>;

/// Everything that changes the glyph quads of one text drawing.
///
/// A later frame with the same stamp replays the quads and does not shape again.
#[derive(Clone, PartialEq)]
pub(crate) struct TextStamp {
    text: String,
    layout: Layout,
    color: Option<[u32; 4]>,
    glyph_colors: Vec<[u32; 4]>,
    matrix: [u32; 16],
    wh: [u32; 2],
    scale: u32,
    theme: [u32; 4],
}

fn color_bits(color: Color) -> [u32; 4] {
    [color.r, color.g, color.b, color.a].map(f32::to_bits)
}

fn matrix_bits(m: &Mat4) -> [u32; 16] {
    m.to_cols_array().map(f32::to_bits)
}

/// One glyph quad of a rendered text, ready for the batch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphQuad {
    /// Corners (top-left, top-right, bottom-right, bottom-left), transformed.
    pub corners: [Vec3; 4],
    /// Atlas texture coordinates `[u0, v0, u1, v1]`.
    pub uv: [f32; 4],
    /// Vertex colour.
    pub color: Color,
    /// `0` = coverage mask page, `1` = colour (emoji) page.
    pub kind: u8,
    /// Atlas page.
    pub page: u32,
}

impl Text {
    /// Begin drawing some text.
    pub fn new(ctxt: DrawingContext, text: &str) -> Self {
        let start = ctxt.text_buffer.len();
        ctxt.text_buffer.push_str(text);
        let end = ctxt.text_buffer.len();
        Text {
            spatial: Default::default(),
            style: Default::default(),
            text: start..end,
        }
    }

    fn map_layout(mut self, map: impl FnOnce(&mut Layout)) -> Self {
        map(&mut self.style.layout);
        self
    }

    /// The font size to use for the text.
    pub fn font_size(self, size: FontSize) -> Self {
        self.map_layout(|l| l.font_size = size)
    }

    /// Specify whether or not text should be wrapped around some width and how to do so.
    pub fn line_wrap(self, line_wrap: Option<Wrap>) -> Self {
        self.map_layout(|l| l.line_wrap = line_wrap)
    }

    /// Specify that the text should not wrap lines around the width.
    pub fn no_line_wrap(self) -> Self {
        self.line_wrap(None)
    }

    /// Line wrap the text at word boundaries.
    pub fn wrap_by_word(self) -> Self {
        self.line_wrap(Some(Wrap::Whitespace))
    }

    /// Line wrap the text at any character.
    pub fn wrap_by_character(self) -> Self {
        self.line_wrap(Some(Wrap::Character))
    }

    /// The font family to use.
    pub fn font(self, family: impl Into<String>) -> Self {
        let family = family.into();
        self.map_layout(|l| l.font_family = Some(family))
    }

    /// Describe the end along the *x* axis to which the text should be aligned.
    pub fn justify(self, justify: Justify) -> Self {
        self.map_layout(|l| l.justify = justify)
    }

    /// Align the text to the start edge of its bounding rectangle (left for left-to-right text,
    /// right for right-to-left text).
    pub fn left_justify(self) -> Self {
        self.justify(Justify::Left)
    }

    /// Align the text to the middle of its bounding rectangle.
    pub fn center_justify(self) -> Self {
        self.justify(Justify::Center)
    }

    /// Align the text to the end edge of its bounding rectangle (right for left-to-right text,
    /// left for right-to-left text).
    pub fn right_justify(self) -> Self {
        self.justify(Justify::Right)
    }

    /// Specify extra spacing between lines, in logical units.
    pub fn line_spacing(self, spacing: f32) -> Self {
        self.map_layout(|l| l.line_spacing = spacing)
    }

    /// Specify how the whole text block should be aligned along the *y* axis of its rectangle.
    pub fn y_align(self, align: Align) -> Self {
        self.map_layout(|l| l.y_align = align)
    }

    /// Align the top of the text with the top of its bounding rectangle.
    pub fn align_top(self) -> Self {
        self.y_align(Align::End)
    }

    /// Align the middle of the text with the middle of its bounding rectangle.
    pub fn align_middle_y(self) -> Self {
        self.y_align(Align::Middle)
    }

    /// Align the bottom of the text with the bottom of its bounding rectangle.
    pub fn align_bottom(self) -> Self {
        self.y_align(Align::Start)
    }

    /// Set all the layout parameters at once.
    pub fn layout(self, layout: &Layout) -> Self {
        let layout = layout.clone();
        self.map_layout(|l| *l = layout)
    }

    /// Specify the entire styling for the **Text**.
    pub fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set a colour for each glyph, in order (glyphs past the end use the text colour).
    pub fn glyph_colors(mut self, colors: Vec<Color>) -> Self {
        self.style.glyph_colors = colors;
        self
    }

    /// The drawn string.
    pub(crate) fn string<'b>(&self, text_buffer: &'b str) -> &'b str {
        text_buffer.get(self.text.clone()).unwrap_or("")
    }

    fn placed(&self, transform: &Mat4) -> Mat4 {
        let local = self.spatial.position.transform() * self.spatial.orientation.transform();
        *transform * local
    }

    fn block_wh(&self, output_size: Vec2) -> [f32; 2] {
        [
            self.spatial.dimensions.x.unwrap_or(output_size.x),
            self.spatial.dimensions.y.unwrap_or(output_size.y),
        ]
    }

    /// True when `stamp` was built from the same string, layout, colour, placement and theme.
    pub(crate) fn matches_stamp(
        &self,
        text_buffer: &str,
        stamp: &TextStamp,
        theme: &draw::Theme,
        transform: &Mat4,
        output_size: Vec2,
        scale_factor: f32,
    ) -> bool {
        if stamp.text != self.string(text_buffer) || stamp.layout != self.style.layout {
            return false;
        }
        if stamp.color != self.style.color.map(color_bits) {
            return false;
        }
        if stamp.glyph_colors.len() != self.style.glyph_colors.len()
            || stamp
                .glyph_colors
                .iter()
                .zip(&self.style.glyph_colors)
                .any(|(bits, color)| *bits != color_bits(*color))
        {
            return false;
        }
        let theme_fill = theme.fill(&draw::theme::Primitive::Text);
        stamp.theme == color_bits(theme_fill)
            && stamp.scale == scale_factor.to_bits()
            && stamp.wh == self.block_wh(output_size).map(f32::to_bits)
            && stamp.matrix == matrix_bits(&self.placed(transform))
    }

    /// The stamp of this drawing. Allocates the string and the font-family copy once per miss.
    pub(crate) fn to_stamp(
        &self,
        text_buffer: &str,
        theme: &draw::Theme,
        transform: &Mat4,
        output_size: Vec2,
        scale_factor: f32,
    ) -> TextStamp {
        TextStamp {
            text: self.string(text_buffer).to_string(),
            layout: self.style.layout.clone(),
            color: self.style.color.map(color_bits),
            glyph_colors: self.style.glyph_colors.iter().copied().map(color_bits).collect(),
            matrix: matrix_bits(&self.placed(transform)),
            wh: self.block_wh(output_size).map(f32::to_bits),
            scale: scale_factor.to_bits(),
            theme: color_bits(theme.fill(&draw::theme::Primitive::Text)),
        }
    }

    /// Shapes the text and appends one quad per visible glyph to `out`, rasterising glyphs into
    /// `atlas` on demand.
    ///
    /// Glyphs are shaped at physical-pixel size (`scale_factor` physical px per logical unit) so
    /// rasterised glyphs map 1:1 to screen pixels when the transform does not scale, then
    /// placed back in logical units.
    #[allow(clippy::too_many_arguments)] // the frame's text context
    pub(crate) fn render_glyphs(
        self,
        text_buffer: &str,
        theme: &draw::Theme,
        transform: &Mat4,
        output_size: Vec2,
        scale_factor: f32,
        fonts: &mut TextSystem,
        localization: &Localization,
        atlas: &mut GlyphAtlases,
        out: &mut Vec<GlyphQuad>,
    ) {
        let Some(s) = text_buffer.get(self.text.clone()) else {
            return;
        };
        if s.is_empty() {
            return;
        }
        let scale = scale_factor.max(1e-3);
        let layout = &self.style.layout;
        let w = self.spatial.dimensions.x.unwrap_or(output_size.x);
        let h = self.spatial.dimensions.y.unwrap_or(output_size.y);
        let font_px = layout.font_size.max(1) as f32;
        let mut ui_text = UiText {
            spans: vec![TextSpan::lit(s).with_style(UiTextStyle {
                font_px,
                family: layout.font_family.clone(),
                ..Default::default()
            })],
            ..Default::default()
        };
        ui_text.wrap = match layout.line_wrap {
            None => WrapMode::None,
            Some(Wrap::Whitespace) => WrapMode::Word,
            Some(Wrap::Character) => WrapMode::Glyph,
        };
        ui_text.align = match layout.justify {
            Justify::Left => TextAlign::Start,
            Justify::Center => TextAlign::Center,
            Justify::Right => TextAlign::End,
        };
        ui_text.line_height = 1.2 + layout.line_spacing / font_px;
        let shaped = fonts.shape(&ui_text, scale, Some((w * scale).max(1.0)), localization);

        // The block's top-left in logical units (+Y up), inside the rectangle centred on the
        // drawing's position.
        let block_h = shaped.size[1] / scale;
        let rect_top = h * 0.5;
        let block_top = match layout.y_align {
            Align::End => rect_top,
            Align::Middle => block_h * 0.5,
            Align::Start => -rect_top + block_h,
        };
        let left = -w * 0.5;
        let local = self.spatial.position.transform() * self.spatial.orientation.transform();
        let m = *transform * local;
        let default_color = self
            .style
            .color
            .unwrap_or_else(|| theme.fill(&draw::theme::Primitive::Text));
        let page = PAGE_SIZE as f32;
        for (i, glyph) in shaped.glyphs.iter().enumerate() {
            let key = glyph.cache_key;
            let Some((place, offset)) = atlas.get_or_insert(key, || fonts.glyph_image(key)) else {
                continue;
            };
            let gx = (glyph.pen[0] + offset[0]) as f32 / scale;
            let gy = (glyph.pen[1] + offset[1]) as f32 / scale;
            let (gw, gh) = (place.rect[2] as f32 / scale, place.rect[3] as f32 / scale);
            let x0 = left + gx;
            let y0 = block_top - gy;
            let corners = [
                Vec3::new(x0, y0, 0.0),
                Vec3::new(x0 + gw, y0, 0.0),
                Vec3::new(x0 + gw, y0 - gh, 0.0),
                Vec3::new(x0, y0 - gh, 0.0),
            ]
            .map(|p| m.transform_point3(p));
            let color = self.style.glyph_colors.get(i).copied().unwrap_or(default_color);
            out.push(GlyphQuad {
                corners,
                uv: [
                    place.rect[0] as f32 / page,
                    place.rect[1] as f32 / page,
                    (place.rect[0] + place.rect[2]) as f32 / page,
                    (place.rect[1] + place.rect[3]) as f32 / page,
                ],
                // Colour glyphs keep their bitmap colours; only the alpha fades them.
                color: if place.kind == 1 {
                    Color::WHITE.with_alpha(color.a)
                } else {
                    color
                },
                kind: place.kind,
                page: place.page,
            });
        }
    }
}

impl<'a> DrawingText<'a> {
    fn update(self, f: impl FnOnce(&mut Text)) -> Self {
        update_text(self.draw, self.index, f);
        self
    }

    /// The font size to use for the text.
    pub fn font_size(self, size: FontSize) -> Self {
        self.update(|t| t.style.layout.font_size = size)
    }

    /// Specify that the text should not wrap lines around the width.
    pub fn no_line_wrap(self) -> Self {
        self.update(|t| t.style.layout.line_wrap = None)
    }

    /// Line wrap the text at word boundaries.
    pub fn wrap_by_word(self) -> Self {
        self.update(|t| t.style.layout.line_wrap = Some(Wrap::Whitespace))
    }

    /// Line wrap the text at any character.
    pub fn wrap_by_character(self) -> Self {
        self.update(|t| t.style.layout.line_wrap = Some(Wrap::Character))
    }

    /// The font family to use.
    pub fn font(self, family: impl Into<String>) -> Self {
        let family = family.into();
        self.update(|t| t.style.layout.font_family = Some(family))
    }

    /// Specify the entire styling for the **Text**.
    pub fn with_style(self, style: Style) -> Self {
        self.update(|t| t.style = style)
    }

    /// Describe the end along the *x* axis to which the text should be aligned.
    pub fn justify(self, justify: Justify) -> Self {
        self.update(|t| t.style.layout.justify = justify)
    }

    /// Align the text to the start edge of its bounding rectangle (left for left-to-right text,
    /// right for right-to-left text).
    pub fn left_justify(self) -> Self {
        self.justify(Justify::Left)
    }

    /// Align the text to the middle of its bounding rectangle.
    pub fn center_justify(self) -> Self {
        self.justify(Justify::Center)
    }

    /// Align the text to the end edge of its bounding rectangle (right for left-to-right text,
    /// left for right-to-left text).
    pub fn right_justify(self) -> Self {
        self.justify(Justify::Right)
    }

    /// Specify extra spacing between lines, in logical units.
    pub fn line_spacing(self, spacing: f32) -> Self {
        self.update(|t| t.style.layout.line_spacing = spacing)
    }

    /// Specify how the whole text block should be aligned along the *y* axis of its rectangle.
    pub fn y_align_text(self, align: Align) -> Self {
        self.update(|t| t.style.layout.y_align = align)
    }

    /// Align the top of the text with the top of its bounding rectangle.
    pub fn align_text_top(self) -> Self {
        self.y_align_text(Align::End)
    }

    /// Align the middle of the text with the middle of its bounding rectangle.
    pub fn align_text_middle_y(self) -> Self {
        self.y_align_text(Align::Middle)
    }

    /// Align the bottom of the text with the bottom of its bounding rectangle.
    pub fn align_text_bottom(self) -> Self {
        self.y_align_text(Align::Start)
    }

    /// Set all the layout parameters at once.
    pub fn layout(self, layout: &Layout) -> Self {
        let layout = layout.clone();
        self.update(|t| t.style.layout = layout)
    }

    /// Set a colour for each glyph, in order.
    pub fn glyph_colors<I, C>(self, glyph_colors: I) -> Self
    where
        I: IntoIterator<Item = C>,
        C: Into<Color>,
    {
        let colors: Vec<Color> = glyph_colors.into_iter().map(Into::into).collect();
        self.update(|t| t.style.glyph_colors = colors)
    }
}

// Update the inner `Text` of the primitive being drawn at `index`.
fn update_text(draw: &crate::draw::Draw, index: usize, f: impl FnOnce(&mut Text)) {
    crate::draw::drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::Text(text) => f(text),
        _ => crate::warn_once!("expected a `Text` primitive"),
    })
}

impl SetOrientation for Text {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.spatial)
    }
}

impl SetPosition for Text {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.spatial)
    }
}

impl SetDimensions for Text {
    fn properties(&mut self) -> &mut dimension::Properties {
        SetDimensions::properties(&mut self.spatial)
    }
}

impl SetColor for Text {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.style.color)
    }
}

impl From<Text> for Primitive {
    fn from(prim: Text) -> Self {
        Primitive::Text(prim)
    }
}
