// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/theme.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use std::collections::HashMap;

use regia_core::math::Color;

/// A set of styling defaults used for coloring texturing geometric primitives that have no entry
/// within the **Draw**'s inner **ColorMap**.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Fill color defaults.
    pub fill_color: ThemeColor,
    /// Stroke color defaults.
    pub stroke_color: ThemeColor,
}

/// A set of defaults used for coloring.
#[derive(Clone, Debug)]
pub struct ThemeColor {
    /// Colour of primitives without an entry.
    pub default: Color,
    /// Per-primitive colours.
    pub primitive: HashMap<Primitive, Color>,
}

/// Primitive geometry types that may have unique default styles.
///
/// These are used as keys into the **Theme**'s geometry primitive default values.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Primitive {
    /// The arrow primitive.
    Arrow,
    /// The cuboid primitive.
    Cuboid,
    /// The ellipse primitive.
    Ellipse,
    /// The line primitive.
    Line,
    /// The mesh primitive.
    Mesh,
    /// The path primitive.
    Path,
    /// The polygon primitive.
    Polygon,
    /// The quad primitive.
    Quad,
    /// The rect primitive.
    Rect,
    /// The text primitive.
    Text,
    /// The texture primitive.
    Texture,
    /// The tri primitive.
    Tri,
}

impl Theme {
    /// Retrieve the fill color representation for the given primitive.
    pub fn fill(&self, prim: &Primitive) -> Color {
        if self.fill_color.primitive.is_empty() {
            return self.fill_color.default;
        }
        self.fill_color
            .primitive
            .get(prim)
            .copied()
            .unwrap_or(self.fill_color.default)
    }

    /// Retrieve the  stroke color representation for the given primitive.
    pub fn stroke(&self, prim: &Primitive) -> Color {
        self.stroke_color
            .primitive
            .get(prim)
            .copied()
            .unwrap_or(self.stroke_color.default)
    }
}

impl Default for Theme {
    fn default() -> Self {
        // TODO: This should be pub const.
        let default_fill = crate::color::srgba(1.0, 1.0, 1.0, 1.0);
        let default_stroke = crate::color::srgba(0.0, 0.0, 0.0, 1.0);

        let fill_color = ThemeColor {
            default: default_fill,
            primitive: Default::default(),
        };
        let mut stroke_color = ThemeColor {
            default: default_stroke,
            primitive: Default::default(),
        };
        stroke_color.primitive.insert(Primitive::Arrow, default_fill);

        Theme {
            fill_color,
            stroke_color,
        }
    }
}
