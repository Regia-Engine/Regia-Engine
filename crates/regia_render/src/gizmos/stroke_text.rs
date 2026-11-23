// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/stroke_text.rs. Copyright (c) Bevy contributors, dual-licensed
// MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
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
//! Stroke text: ASCII labels drawn as line strips with the Simplex Hershey font.

use std::ops::Range;

use regia_core::math::{Color, Vec2};

use super::buffer::{GizmoBuffer, IntoIsometry};
use super::config::GizmoConfigGroup;
use super::simplex_stroke_font::{SIMPLEX_STROKE_FONT, TAB_WIDTH_IN_SPACES};

/// A stroke font: glyphs made of polylines on a small integer grid.
pub(crate) struct StrokeFont<'a> {
    /// Line height as a multiple of [`Self::height`].
    pub line_height: f32,
    /// Full glyph height (cap height + descender).
    pub height: f32,
    /// Cap height in grid units.
    pub cap_height: f32,
    /// Default advance (unused by the Simplex data, kept for the format).
    #[allow(dead_code)]
    pub advance: i8,
    /// Every stroke point.
    pub positions: &'a [[i8; 2]],
    /// Ranges into `positions`, one per stroke.
    pub strokes: &'a [Range<usize>],
    /// Per printable ASCII glyph (0x20..=0x7E): advance and its range of strokes.
    pub glyphs: &'a [(i8, Range<usize>); 95],
}

impl<'a> StrokeFont<'a> {
    fn layout(&'a self, sections: &'a [(&'a str, Color)], font_size: f32) -> StrokeTextLayout<'a> {
        let scale = font_size / self.cap_height;
        let glyph_height = self.height * scale;
        let line_height = self.line_height * glyph_height;
        let margin_top = line_height - glyph_height;
        let space_advance = f32::from(self.glyphs[0].0) * scale;
        let tab_advance = space_advance * TAB_WIDTH_IN_SPACES as f32;
        StrokeTextLayout {
            font: self,
            sections,
            scale,
            line_height,
            margin_top,
            space_advance,
            tab_advance,
        }
    }

    fn glyph_index(c: char) -> Option<usize> {
        let code = c as u32;
        (0x20..=0x7E).contains(&code).then(|| code as usize - 0x20)
    }

    fn glyph(&self, c: char) -> Option<(i8, Range<usize>)> {
        Some(self.glyphs[Self::glyph_index(c)?].clone())
    }

    fn glyph_advance(&self, c: char) -> Option<i8> {
        Some(self.glyphs[Self::glyph_index(c)?].0)
    }
}

struct StrokeTextLayout<'a> {
    font: &'a StrokeFont<'a>,
    sections: &'a [(&'a str, Color)],
    scale: f32,
    line_height: f32,
    margin_top: f32,
    space_advance: f32,
    tab_advance: f32,
}

impl<'a> StrokeTextLayout<'a> {
    /// Width and height of the laid-out text.
    fn measure(&self) -> Vec2 {
        let mut size = Vec2::new(0.0, self.line_height);
        let mut line_width = 0.0f32;
        for (c, _) in colored_chars(self.sections) {
            match c {
                '\n' => {
                    size.x = size.x.max(line_width);
                    line_width = 0.0;
                    size.y += self.line_height;
                }
                '\t' => line_width = next_tab_stop(line_width, self.tab_advance),
                _ => {
                    line_width += self
                        .font
                        .glyph_advance(c)
                        .map_or(self.space_advance, |advance| f32::from(advance) * self.scale);
                }
            }
        }
        size.x = size.x.max(line_width);
        size
    }

    /// Every stroke as (colour, points), top-left of the first line at the origin, +Y up.
    fn render(&'a self) -> impl Iterator<Item = (Color, impl Iterator<Item = Vec2> + 'a)> + 'a {
        let mut chars = colored_chars(self.sections);
        let mut x = 0.0f32;
        let mut y = -self.margin_top;
        let mut current_strokes: Range<usize> = 0..0;
        let mut current_x = 0.0f32;
        let mut current_color = Color::WHITE;
        std::iter::from_fn(move || {
            loop {
                for stroke_index in current_strokes.by_ref() {
                    let stroke = self.font.strokes[stroke_index].clone();
                    if stroke.len() < 2 {
                        continue;
                    }
                    // A closed stroke gets one extra point so its seam has a joint.
                    let join = (self.font.positions[stroke.start] == self.font.positions[stroke.end - 1])
                        .then_some(stroke.start + 1);
                    let (scale, cap_height, glyph_x, line_y) =
                        (self.scale, self.font.cap_height, current_x, y);
                    let positions = self.font.positions;
                    return Some((
                        current_color,
                        stroke.chain(join).map(move |index| {
                            let [p, q] = positions[index];
                            Vec2::new(
                                glyph_x + scale * f32::from(p),
                                line_y - scale * (cap_height - f32::from(q)),
                            )
                        }),
                    ));
                }
                let (c, char_color) = chars.next()?;
                match c {
                    '\n' => {
                        x = 0.0;
                        y -= self.line_height;
                    }
                    '\t' => x = next_tab_stop(x, self.tab_advance),
                    _ => match self.font.glyph(c) {
                        Some((advance, strokes)) => {
                            current_color = char_color;
                            current_strokes = strokes;
                            current_x = x;
                            x += f32::from(advance) * self.scale;
                        }
                        None => x += self.space_advance,
                    },
                }
            }
        })
    }
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws `text` (printable ASCII; `\n` and `\t` lay out lines and tab stops) in the local XY
    /// plane of `isometry`. `font_size` is the cap height in world units; `anchor` picks the
    /// point of the text box placed at the isometry (`(0, 0)` centre, `(-0.5, 0.5)` top-left).
    pub fn text(
        &mut self,
        isometry: impl IntoIsometry,
        text: &str,
        font_size: f32,
        anchor: Vec2,
        color: impl Into<Color>,
    ) {
        self.text_sections(isometry, &[(text, color.into())], font_size, anchor);
    }

    /// [`Self::text`] with a colour per section.
    pub fn text_sections(
        &mut self,
        isometry: impl IntoIsometry,
        sections: &[(&str, Color)],
        font_size: f32,
        anchor: Vec2,
    ) {
        if !self.enabled {
            return;
        }
        let isometry = isometry.into_isometry();
        let layout = SIMPLEX_STROKE_FONT.layout(sections, font_size);
        let layout_anchor = layout.measure() * (Vec2::new(-0.5, 0.5) - anchor);
        for (color, points) in layout.render() {
            self.linestrip(
                points.map(|point| isometry.transform_point((layout_anchor + point).extend(0.0))),
                color,
            );
        }
    }
}

// New code, not from the ported source (KI-ED-019, ADR-10070): the same layout as segments.
/// Stroke text as 2D line segments in the text's own plane (+X right, +Y up): the layout of
/// [`GizmoBuffer::text`] with the same `font_size` (cap height) and `anchor`, for a drawer that takes
/// segments instead of a gizmo buffer (the editor's segment overlays, KI-ED-019).
pub fn stroke_text_segments(text: &str, font_size: f32, anchor: Vec2) -> Vec<[Vec2; 2]> {
    let sections = [(text, Color::WHITE)];
    let layout = SIMPLEX_STROKE_FONT.layout(&sections, font_size);
    let layout_anchor = layout.measure() * (Vec2::new(-0.5, 0.5) - anchor);
    let mut out = Vec::new();
    for (_, points) in layout.render() {
        let mut last: Option<Vec2> = None;
        for p in points {
            let p = layout_anchor + p;
            if let Some(a) = last {
                out.push([a, p]);
            }
            last = Some(p);
        }
    }
    out
}

fn colored_chars<'a>(sections: &'a [(&'a str, Color)]) -> impl Iterator<Item = (char, Color)> + 'a {
    sections
        .iter()
        .flat_map(|&(text, color)| text.chars().map(move |c| (c, color)))
}

fn next_tab_stop(x: f32, tab_advance: f32) -> f32 {
    if tab_advance <= 0.0 {
        return x;
    }
    let tab_index = (x / tab_advance).floor() as i64 + 1;
    tab_index as f32 * tab_advance
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;
    use regia_core::math::Vec3;

    #[test]
    fn letters_become_strokes_and_spaces_do_not() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.text(Vec3::ZERO, "  ", 1.0, Vec2::ZERO, Color::WHITE);
        assert!(b.is_empty());
        b.text(Vec3::ZERO, "H", 1.0, Vec2::ZERO, Color::WHITE);
        // 'H' is three strokes: two uprights and the bar.
        let strokes = b
            .strip_positions
            .split(|p| p.is_nan())
            .filter(|r| !r.is_empty())
            .count();
        assert_eq!(strokes, 3);
    }

    #[test]
    fn centred_text_is_centred_and_cap_height_matches_font_size() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.text(Vec3::ZERO, "HI", 2.0, Vec2::ZERO, Color::WHITE);
        let points: Vec<_> = b.strip_positions.iter().filter(|p| !p.is_nan()).collect();
        let (min_y, max_y) = points
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p.y), hi.max(p.y)));
        assert!(
            ((max_y - min_y) - 2.0).abs() < 1e-3,
            "cap height {}",
            max_y - min_y
        );
        let (min_x, max_x) = points
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p.x), hi.max(p.x)));
        assert!((min_x + max_x).abs() < 0.6, "roughly centered: {min_x}..{max_x}");
    }

    #[test]
    fn newlines_stack_downwards() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.text(Vec3::ZERO, "-\n-", 1.0, Vec2::new(-0.5, 0.5), Color::WHITE);
        let ys: Vec<f32> = b
            .strip_positions
            .split(|p| p.is_nan())
            .filter(|r| !r.is_empty())
            .map(|r| r[0].y)
            .collect();
        assert_eq!(ys.len(), 2);
        assert!(ys[1] < ys[0] - 1.0);
    }
}
