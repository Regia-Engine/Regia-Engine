// SPDX-License-Identifier: MPL-2.0
//! UI colours: linear RGBA with sRGB hex parsing (themes and `.ui.ron` author in sRGB; the
//! renderer works in linear space and the sRGB surface encodes on write).

use serde::{Deserialize, Serialize};

/// A linear RGBA colour, components in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiColor(pub [f32; 4]);

impl UiColor {
    /// Transparent black.
    pub const TRANSPARENT: Self = Self([0.0; 4]);
    /// Opaque white (linear).
    pub const WHITE: Self = Self([1.0, 1.0, 1.0, 1.0]);
    /// Opaque black.
    pub const BLACK: Self = Self([0.0, 0.0, 0.0, 1.0]);

    /// Builds a colour from linear components.
    pub const fn linear(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self([r, g, b, a])
    }

    /// Builds a colour from 8-bit sRGB components and alpha `0..=255`.
    pub fn from_srgb8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self([
            srgb_to_linear(r),
            srgb_to_linear(g),
            srgb_to_linear(b),
            f32::from(a) / 255.0,
        ])
    }

    /// Parses `#RRGGBB` or `#RRGGBBAA` (sRGB, authoring format). Returns `None` on malformed input.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let h = hex.strip_prefix('#')?;
        // `from_str_radix` also accepts a leading `+`; a colour literal is hex digits only.
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let (r, g, b, a) = match h.len() {
            6 => {
                let v = u32::from_str_radix(h, 16).ok()?;
                (
                    ((v >> 16) & 0xFF) as u8,
                    ((v >> 8) & 0xFF) as u8,
                    (v & 0xFF) as u8,
                    255,
                )
            }
            8 => {
                let v = u32::from_str_radix(h, 16).ok()?;
                (
                    ((v >> 24) & 0xFF) as u8,
                    ((v >> 16) & 0xFF) as u8,
                    ((v >> 8) & 0xFF) as u8,
                    (v & 0xFF) as u8,
                )
            }
            _ => return None,
        };
        Some(Self::from_srgb8(r, g, b, a))
    }

    /// This colour with a different alpha component.
    pub const fn with_alpha(self, a: f32) -> Self {
        Self([self.0[0], self.0[1], self.0[2], a])
    }

    /// Linear interpolation between two colours (used by theme transitions).
    pub fn lerp(self, other: Self, t: f32) -> Self {
        // A NaN `t` (clamp passes it through) would poison every channel: it counts as 0.
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        Self(std::array::from_fn(|i| self.0[i] + (other.0[i] - self.0[i]) * t))
    }

    /// Multiplies this colour by `other` (component-wise tinting).
    pub const fn multiply(self, other: Self) -> Self {
        Self([
            self.0[0] * other.0[0],
            self.0[1] * other.0[1],
            self.0[2] * other.0[2],
            self.0[3] * other.0[3],
        ])
    }
}

/// One linear-light channel to sRGB-encoded `0.0..=1.0`.
pub(crate) fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// One linear-light channel to an 8-bit sRGB value.
pub(crate) fn linear_to_srgb8(v: f32) -> u8 {
    (linear_to_srgb(v) * 255.0 + 0.5) as u8
}

/// One sRGB channel (`0..=255`) to linear light.
pub(crate) fn srgb_to_linear(c: u8) -> f32 {
    let c = f32::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

impl Default for UiColor {
    fn default() -> Self {
        Self::WHITE
    }
}

impl<'de> Deserialize<'de> for UiColor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Hex(String),
            Channels([f32; 4]),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Hex(hex) => UiColor::from_hex(&hex).ok_or_else(|| {
                D::Error::custom(format!(
                    "invalid color {hex:?}; expected \"#RRGGBB[AA]\" or linear [r,g,b,a]"
                ))
            }),
            Repr::Channels(c) => Ok(UiColor(c)),
        }
    }
}

impl Serialize for UiColor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialise as linear channels (round-trips exactly); hex is the authoring form.
        self.0.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_and_converts() {
        let white = UiColor::from_hex("#FFFFFF").unwrap();
        assert!(white.0.iter().all(|&c| (c - 1.0).abs() < 1e-4));
        let black = UiColor::from_hex("#000000").unwrap();
        assert!(black.0.iter().take(3).all(|&c| c.abs() < 1e-4));
        let half = UiColor::from_hex("#FF000080").unwrap();
        assert!((half.0[3] - 128.0 / 255.0).abs() < 1e-4);
        // sRGB 0x80 ≈ 0.216 linear
        let mid = UiColor::from_hex("#808080").unwrap();
        assert!((mid.0[0] - 0.2159).abs() < 1e-3);
    }

    #[test]
    fn bad_hex_rejected() {
        assert!(UiColor::from_hex("FFFFFF").is_none());
        assert!(UiColor::from_hex("#12345").is_none());
        assert!(UiColor::from_hex("#GGGGGG").is_none());
    }

    #[test]
    fn lerp_and_tint() {
        let a = UiColor::linear(0.0, 0.0, 0.0, 1.0);
        let b = UiColor::linear(1.0, 1.0, 1.0, 1.0);
        assert_eq!(a.lerp(b, 0.5), UiColor::linear(0.5, 0.5, 0.5, 1.0));
        let tinted = b.multiply(UiColor::linear(0.5, 1.0, 1.0, 1.0));
        assert_eq!(tinted, UiColor::linear(0.5, 1.0, 1.0, 1.0));
    }
}
