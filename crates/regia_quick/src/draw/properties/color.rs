// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/properties/color.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use regia_core::math::Color;

use crate::draw::{Draw, drawing};

/// Nodes that support setting colors.
pub trait SetColor: Sized {
    /// Provide a mutable reference to the RGBA field which can be used for setting colors.
    fn color_mut(&mut self) -> &mut Option<Color>;

    /// Specify a color.
    ///
    /// This method supports any color type that can be converted into RGBA.
    ///
    /// Colors that have no alpha channel will be given an opaque alpha channel value `1.0`.
    fn color<C>(mut self, color: C) -> Self
    where
        C: Into<Color>,
    {
        *self.color_mut() = Some(color.into());
        self
    }

    /// Specify the color via red, green and blue channels.
    fn srgb(self, r: f32, g: f32, b: f32) -> Self {
        self.color(crate::color::srgb(r, g, b))
    }

    /// Specify the color via red, green and blue channels as bytes
    fn srgb_u8(self, r: u8, g: u8, b: u8) -> Self {
        self.color(crate::color::srgb_u8(r, g, b))
    }

    /// Specify the color via red, green, blue and alpha channels.
    fn srgba(self, r: f32, g: f32, b: f32, a: f32) -> Self {
        self.color(crate::color::srgba(r, g, b, a))
    }

    /// Specify the color via red, green, blue and alpha channels as bytes
    fn srgba_u8(self, r: u8, g: u8, b: u8, a: u8) -> Self {
        self.color(crate::color::srgba_u8(r, g, b, a))
    }

    /// Specify the color via linear red, green and blue channels.
    fn linear_rgb(self, r: f32, g: f32, b: f32) -> Self {
        self.color(crate::color::linear_rgb(r, g, b))
    }

    /// Specify the color via linear red, green, blue and alpha channels.
    fn linear_rgba(self, r: f32, g: f32, b: f32, a: f32) -> Self {
        self.color(crate::color::linear_rgba(r, g, b, a))
    }

    /// Specify the color via hue, saturation and luminance.
    ///
    /// If you're looking for HSV or HSB, use the `hsv` method instead.
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    fn hsl(self, h: f32, s: f32, l: f32) -> Self {
        let hue = h * 360.0;
        self.color(crate::color::hsl(hue, s, l))
    }

    /// Specify the color via hue, saturation, luminance and an alpha channel.
    ///
    /// If you're looking for HSVA or HSBA, use the `hsva` method instead.
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    fn hsla(self, h: f32, s: f32, l: f32, a: f32) -> Self {
        let hue = h * 360.0;
        self.color(crate::color::hsla(hue, s, l, a))
    }

    /// Specify the color via hue, saturation and *value* (brightness).
    ///
    /// This is sometimes also known as "hsb".
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    fn hsv(self, h: f32, s: f32, v: f32) -> Self {
        let hue = h * 360.0;
        self.color(crate::color::hsv(hue, s, v))
    }

    /// Specify the color via hue, saturation, *value* (brightness) and an alpha channel.
    ///
    /// This is sometimes also known as "hsba".
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    fn hsva(self, h: f32, s: f32, v: f32, a: f32) -> Self {
        let hue = h * 360.0;
        self.color(crate::color::hsva(hue, s, v, a))
    }

    /// Specify the color via hue (`0..1` turns), whiteness and blackness.
    fn hwb(self, h: f32, w: f32, b: f32) -> Self {
        let hue = h * 360.0;
        self.color(crate::color::hwb(hue, w, b))
    }

    /// Specify the color via hue (`0..1` turns), whiteness, blackness and alpha.
    fn hwba(self, h: f32, w: f32, b: f32, a: f32) -> Self {
        let hue = h * 360.0;
        self.color(crate::color::hwba(hue, w, b, a))
    }

    /// Specify the color via Oklab lightness and opponent axes.
    fn oklab(self, l: f32, a: f32, b: f32) -> Self {
        self.color(crate::color::oklab(l, a, b))
    }

    /// Specify the color via Oklab lightness, opponent axes and alpha.
    fn oklaba(self, l: f32, a: f32, b: f32, alpha: f32) -> Self {
        self.color(crate::color::oklaba(l, a, b, alpha))
    }

    /// Specify the color via Oklch lightness, chroma and hue (degrees).
    fn oklch(self, l: f32, c: f32, h: f32) -> Self {
        self.color(crate::color::oklch(l, c, h))
    }

    /// Specify the color via Oklch lightness, chroma, hue (degrees) and alpha.
    fn oklcha(self, l: f32, c: f32, h: f32, alpha: f32) -> Self {
        self.color(crate::color::oklcha(l, c, h, alpha))
    }

    /// Specify the color as gray scale
    ///
    /// The given g expects a value between `0.0` and `1.0` where `0.0` is black and `1.0` is white
    fn gray(self, g: f32) -> Self {
        self.color(crate::color::srgb(g, g, g))
    }
}

impl SetColor for Option<Color> {
    fn color_mut(&mut self) -> &mut Option<Color> {
        self
    }
}

// Set the color of the primitive being drawn at `index`.
pub(crate) fn set_color(draw: &Draw, index: usize, color: Color) {
    drawing::with_primitive(draw, index, |prim| match prim.color_mut() {
        Some(c) => *c = Some(color),
        None => crate::warn_once!("drawing primitive does not support `color`"),
    })
}
