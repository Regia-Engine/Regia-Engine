// SPDX-License-Identifier: MPL-2.0
//! Color picker model (ADR-273 W11, ADR-1842): hue, saturation, value and alpha with hex
//! conversion and the pointer mappings of the usual picker layout (a saturation/value square, a hue
//! bar and an alpha bar).
//!
//! [`ColorPicker`] stores HSV so dragging in the square never loses the hue of a gray, and edits
//! through hex round-trip exactly for 8-bit sRGB colors. The conversion is the textbook HSV hexcone.
//! Positions are fractions `0..=1` of the square or bar, so the widget can map any pixel size.
//!
//! The agent surface is `ui.color_picker` (`book/agents/ui-color-picker.md`).

use regia_app::prelude::*;

use crate::color::UiColor;

/// An HSV color with alpha. `hue` is in degrees `0..360`, the rest in `0..=1`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct ColorPicker {
    /// Hue in degrees, `0..360`.
    pub hue: f32,
    /// Saturation, `0..=1`.
    pub sat: f32,
    /// Value (brightness), `0..=1`.
    pub val: f32,
    /// Alpha, `0..=1`.
    pub alpha: f32,
}

impl Default for ColorPicker {
    fn default() -> Self {
        Self {
            hue: 0.0,
            sat: 0.0,
            val: 1.0,
            alpha: 1.0,
        }
    }
}

fn unit(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

impl ColorPicker {
    /// Builds a picker from 8-bit sRGB components and alpha.
    pub fn from_srgb8(r: u8, g: u8, b: u8, a: u8) -> Self {
        let (r, g, b) = (f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let hue = if d <= 0.0 {
            0.0
        } else if max == r {
            60.0 * (((g - b) / d).rem_euclid(6.0))
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        Self {
            hue,
            sat: if max <= 0.0 { 0.0 } else { d / max },
            val: max,
            alpha: f32::from(a) / 255.0,
        }
    }

    /// The 8-bit sRGB components and alpha.
    pub fn to_srgb8(&self) -> [u8; 4] {
        let h = if self.hue.is_finite() {
            self.hue.rem_euclid(360.0)
        } else {
            0.0
        } / 60.0;
        let (s, v) = (unit(self.sat), unit(self.val));
        let c = v * s;
        let x = c * (1.0 - (h % 2.0 - 1.0).abs());
        let m = v - c;
        let (r, g, b) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let byte = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        [
            byte(r),
            byte(g),
            byte(b),
            (unit(self.alpha) * 255.0).round() as u8,
        ]
    }

    /// Parses `#RRGGBB` or `#RRGGBBAA`. The picker is unchanged on failure. A gray keeps the
    /// current hue so the hue bar does not jump.
    pub fn set_hex(&mut self, hex: &str) -> bool {
        let Some(digits) = hex.strip_prefix('#') else {
            return false;
        };
        // Hex digits only: `from_str_radix` also takes a leading `+` (`#+F+F+F`).
        // Regression: tests/tqs_sweep.rs::color_picker_set_hex_rejects_sign_prefixed_pairs
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) || !(digits.len() == 6 || digits.len() == 8) {
            return false;
        }
        let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        let (Some(r), Some(g), Some(b)) = (byte(0), byte(2), byte(4)) else {
            return false;
        };
        let a = if digits.len() == 8 {
            match byte(6) {
                Some(a) => a,
                None => return false,
            }
        } else {
            255
        };
        let keep_hue = self.hue;
        *self = Self::from_srgb8(r, g, b, a);
        if self.sat == 0.0 {
            self.hue = keep_hue;
        }
        true
    }

    /// `#RRGGBB`, or `#RRGGBBAA` when the quantized alpha byte is below 255.
    ///
    /// A stored alpha from `254.5 / 255` up to but not including 1 rounds to 255 and is written
    /// as six digits.
    pub fn to_hex(&self) -> String {
        let [r, g, b, a] = self.to_srgb8();
        if a == 255 {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        }
    }

    /// The color in the UI's linear space.
    pub fn color(&self) -> UiColor {
        let [r, g, b, a] = self.to_srgb8();
        UiColor::from_srgb8(r, g, b, a)
    }

    /// Pointer at `(x, y)` fractions of the square: left to right is saturation, top to bottom
    /// goes from full value to black.
    pub fn pick_square(&mut self, x: f32, y: f32) {
        self.sat = unit(x);
        self.val = 1.0 - unit(y);
    }

    /// Where the current color sits in the square, as `(x, y)` fractions.
    pub fn square_pos(&self) -> (f32, f32) {
        (unit(self.sat), 1.0 - unit(self.val))
    }

    /// Pointer at fraction `t` of the hue bar (`0..=1` maps to 0 to 360 degrees; `1` wraps to a
    /// hue just below 360 so the color stays continuous).
    pub fn pick_hue(&mut self, t: f32) {
        self.hue = (unit(t) * 360.0).min(359.999);
    }

    /// Where the hue sits in the bar, as a fraction.
    pub fn hue_pos(&self) -> f32 {
        if self.hue.is_finite() {
            self.hue.rem_euclid(360.0) / 360.0
        } else {
            0.0
        }
    }

    /// Pointer at fraction `t` of the alpha bar.
    pub fn pick_alpha(&mut self, t: f32) {
        self.alpha = unit(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_colors_and_round_trip_all_bytes() {
        assert_eq!(ColorPicker::from_srgb8(255, 0, 0, 255).hue, 0.0);
        assert_eq!(ColorPicker::from_srgb8(0, 255, 0, 255).hue, 120.0);
        assert_eq!(ColorPicker::from_srgb8(0, 0, 255, 255).hue, 240.0);
        let step = 5;
        for r in (0..=255).step_by(step) {
            for g in (0..=255).step_by(step) {
                for b in (0..=255).step_by(17) {
                    let p = ColorPicker::from_srgb8(r as u8, g as u8, b as u8, 200);
                    assert_eq!(p.to_srgb8(), [r as u8, g as u8, b as u8, 200], "{r} {g} {b}");
                }
            }
        }
    }

    #[test]
    fn hex_parse_and_format() {
        let mut p = ColorPicker::default();
        assert!(p.set_hex("#FF8000"));
        assert_eq!(p.to_hex(), "#FF8000");
        assert!(p.set_hex("#11223380"));
        assert_eq!(p.to_hex(), "#11223380");
        let before = p;
        for bad in ["FF8000", "#FFF", "#GG0000", "#12345", "#é23456", ""] {
            assert!(!p.set_hex(bad), "{bad}");
            assert_eq!(p, before);
        }
    }

    #[test]
    fn gray_keeps_the_hue() {
        let mut p = ColorPicker::default();
        p.pick_hue(0.5);
        let hue = p.hue;
        assert!(p.set_hex("#808080"));
        assert_eq!(p.hue, hue);
        assert_eq!(p.sat, 0.0);
    }

    #[test]
    fn square_and_bars_map_pointer_fractions() {
        let mut p = ColorPicker::default();
        p.pick_hue(0.0);
        p.pick_square(1.0, 0.0);
        assert_eq!(p.to_hex(), "#FF0000");
        p.pick_square(0.0, 0.0);
        assert_eq!(p.to_hex(), "#FFFFFF");
        p.pick_square(0.5, 1.0);
        assert_eq!(p.to_hex(), "#000000");
        p.pick_square(1.0, 0.0);
        p.pick_hue(1.0 / 3.0);
        assert_eq!(p.to_hex(), "#00FF00");
        p.pick_hue(1.0);
        assert!(p.hue < 360.0);
        p.pick_alpha(0.5);
        assert_eq!(p.to_srgb8()[3], 128);
        assert_eq!(p.square_pos(), (1.0, 0.0));
        // Out-of-range and non-finite pointer input is clamped.
        p.pick_square(f32::NAN, 7.0);
        assert_eq!(p.square_pos(), (0.0, 1.0));
        p.pick_hue(-3.0);
        assert_eq!(p.hue, 0.0);
    }

    #[test]
    fn linear_color_matches_ui_color() {
        let mut p = ColorPicker::default();
        p.set_hex("#336699");
        assert_eq!(p.color(), UiColor::from_hex("#336699").unwrap());
    }
}
