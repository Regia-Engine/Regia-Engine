// SPDX-License-Identifier: MPL-2.0
//! Player-facing caption options, the board policy and the contrast rule
//! (`docs/specs/56-dialogue-and-subtitles.md` §§ 2.7, 2.8).

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A plain 8-bit sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// White.
    pub const WHITE: Self = Self(255, 255, 255);
    /// Black.
    pub const BLACK: Self = Self(0, 0, 0);
    /// The high-contrast yellow preset.
    pub const YELLOW: Self = Self(255, 255, 0);

    /// Parses `#RRGGBB`; `None` on anything else.
    pub fn from_hex(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        // Hex digits only: `from_str_radix` also takes a leading `+` (`#+12345`).
        // Regression: tests/tqs_sweep.rs::caption_text_color_rejects_sign_prefixed_hex
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        Some(Self((value >> 16) as u8, (value >> 8) as u8, value as u8))
    }

    /// `#rrggbb`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// WCAG relative luminance (the published formula on sRGB components).
    pub fn luminance(self) -> f64 {
        fn channel(value: u8) -> f64 {
            let c = f64::from(value) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.0) + 0.7152 * channel(self.1) + 0.0722 * channel(self.2)
    }

    /// The WCAG contrast ratio against `other` (`1.0..=21.0`).
    pub fn contrast(self, other: Self) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

/// The minimum text/background contrast (WCAG 2.x criterion 1.4.3).
pub const MIN_CONTRAST: f64 = 4.5;

/// What sits behind the text.
#[derive(Reflect, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionBackground {
    /// Nothing.
    None,
    /// A rectangle behind each row.
    #[default]
    Box,
    /// A stroke around the glyphs.
    Outline,
}

/// When the speaker label shows.
#[derive(Reflect, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpeakerLabels {
    /// Never.
    Off,
    /// When the speaker differs from the previous visible row.
    #[default]
    OnChange,
    /// On every row.
    Always,
}

/// Where the block sits.
#[derive(Reflect, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionPosition {
    /// Bottom of the safe area.
    #[default]
    Bottom,
    /// Top of the safe area.
    Top,
}

/// The caption options a player changes (a reflected resource; persisted in the user settings).
#[derive(Resource, Reflect, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[reflect(Resource, Default)]
pub struct CaptionOptions {
    /// Off drops speech and narration (counted as dropped), never sound captions.
    pub enabled: bool,
    /// On: sound captions are shown and the speaker label is always shown.
    pub caption_mode: bool,
    /// Multiplies the base text size (`0.75..=2.0`, step 0.05).
    pub text_scale: f32,
    /// What sits behind the text.
    pub background: CaptionBackground,
    /// Opacity of the background (`0.0..=1.0`); 0 with `Box` falls back to `Outline`.
    pub background_opacity: f32,
    /// When the speaker label shows.
    pub speaker_labels: SpeakerLabels,
    /// Tint the speaker label with the speaker's color.
    pub speaker_colors: bool,
    /// `white`, `yellow` or `#RRGGBB`.
    pub text_color: String,
    /// Top or bottom of the safe area.
    pub position: CaptionPosition,
    /// Reading speed in characters per second (`8..=40`), or the locale default.
    pub reading_cps: Option<u32>,
}

impl Default for CaptionOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            caption_mode: false,
            text_scale: 1.0,
            background: CaptionBackground::Box,
            background_opacity: 0.6,
            speaker_labels: SpeakerLabels::OnChange,
            speaker_colors: true,
            text_color: "white".to_string(),
            position: CaptionPosition::Bottom,
            reading_cps: None,
        }
    }
}

/// Why an options patch was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OptionError {
    /// The option name is not one of the options.
    #[error("unknown option `{0}`")]
    Unknown(String),
    /// The value is outside the range (or has the wrong type).
    #[error("option `{name}` is out of range {min}..={max}")]
    OutOfRange {
        /// The option.
        name: String,
        /// Smallest allowed value.
        min: f64,
        /// Largest allowed value.
        max: f64,
    },
    /// A color value is neither a preset nor `#RRGGBB`.
    #[error("option `{0}` is not a valid color")]
    InvalidColor(String),
    /// A value of the wrong type or an unknown variant name.
    #[error("option `{0}` has an invalid value")]
    InvalidValue(String),
}

fn enum_value<T: for<'de> Deserialize<'de>>(name: &str, value: &Value) -> Result<T, OptionError> {
    serde_json::from_value(value.clone()).map_err(|_| OptionError::InvalidValue(name.to_string()))
}

fn bool_value(name: &str, value: &Value) -> Result<bool, OptionError> {
    value
        .as_bool()
        .ok_or_else(|| OptionError::InvalidValue(name.to_string()))
}

fn ranged(name: &str, value: &Value, min: f64, max: f64) -> Result<f64, OptionError> {
    let out_of_range = || OptionError::OutOfRange {
        name: name.to_string(),
        min,
        max,
    };
    let number = value.as_f64().ok_or_else(out_of_range)?;
    if !number.is_finite() || number < min || number > max {
        return Err(out_of_range());
    }
    Ok(number)
}

impl CaptionOptions {
    /// The option names, in the order the agent lists them.
    pub const NAMES: [&'static str; 10] = [
        "enabled",
        "caption_mode",
        "text_scale",
        "background",
        "background_opacity",
        "speaker_labels",
        "speaker_colors",
        "text_color",
        "position",
        "reading_cps",
    ];

    /// The resolved text color: a preset or `#RRGGBB` (white when the stored text is invalid).
    pub fn text_rgb(&self) -> Rgb {
        parse_text_color(&self.text_color).unwrap_or(Rgb::WHITE)
    }

    /// The label mode in force: `caption_mode` forces `Always`.
    pub fn effective_labels(&self) -> SpeakerLabels {
        if self.caption_mode {
            SpeakerLabels::Always
        } else {
            self.speaker_labels
        }
    }

    /// The background kind in force: `Box` at opacity 0 becomes `Outline` so text stays legible.
    pub fn effective_background(&self) -> CaptionBackground {
        if self.background == CaptionBackground::Box && self.background_opacity <= 0.0 {
            CaptionBackground::Outline
        } else {
            self.background
        }
    }

    /// Applies a subset of options. All-or-nothing: on an error nothing changes. Returns the names
    /// whose value changed, in [`CaptionOptions::NAMES`] order.
    pub fn apply_patch(&mut self, patch: &Map<String, Value>) -> Result<Vec<String>, OptionError> {
        let mut next = self.clone();
        for (name, value) in patch {
            match name.as_str() {
                "enabled" => next.enabled = bool_value(name, value)?,
                "caption_mode" => next.caption_mode = bool_value(name, value)?,
                "text_scale" => {
                    let scale = ranged(name, value, 0.75, 2.0)?;
                    // Step 0.05: snap to the grid so equal inputs give equal bits.
                    next.text_scale = ((scale * 20.0).round() / 20.0) as f32;
                }
                "background" => next.background = enum_value(name, value)?,
                "background_opacity" => next.background_opacity = ranged(name, value, 0.0, 1.0)? as f32,
                "speaker_labels" => next.speaker_labels = enum_value(name, value)?,
                "speaker_colors" => next.speaker_colors = bool_value(name, value)?,
                "text_color" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| OptionError::InvalidColor(name.clone()))?;
                    if parse_text_color(text).is_none() {
                        return Err(OptionError::InvalidColor(name.clone()));
                    }
                    next.text_color = text.to_string();
                }
                "position" => next.position = enum_value(name, value)?,
                "reading_cps" => {
                    next.reading_cps = if value.is_null() {
                        None
                    } else {
                        Some(ranged(name, value, 8.0, 40.0)?.round() as u32)
                    };
                }
                other => return Err(OptionError::Unknown(other.to_string())),
            }
        }
        let mut changed = Vec::new();
        let (before, after) = (self.to_json(), next.to_json());
        for name in Self::NAMES {
            if before.get(name) != after.get(name) {
                changed.push(name.to_string());
            }
        }
        *self = next;
        Ok(changed)
    }

    /// The options as a JSON object (agent `captions.state` and `captions.set_options`).
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `white`, `yellow` or `#RRGGBB`.
pub fn parse_text_color(text: &str) -> Option<Rgb> {
    match text {
        "white" => Some(Rgb::WHITE),
        "yellow" => Some(Rgb::YELLOW),
        other => Rgb::from_hex(other),
    }
}

/// The colors a row uses: the speaker label color (replaced by the neutral text color when it
/// fails 4.5:1 against the background), the background color, and whether the speaker color was
/// replaced. The background is the box/outline color, black, so the ratio is that of the speaker
/// color against black; opacity does not enter (the opacity only fades the box over the scene).
pub fn effective_colors(opts: &CaptionOptions, speaker_color: Rgb) -> (Rgb, Rgb, bool) {
    let background = Rgb::BLACK;
    if speaker_color.contrast(background) >= MIN_CONTRAST {
        (speaker_color, background, false)
    } else {
        (opts.text_rgb(), background, true)
    }
}

/// How the board orders and limits captions (project policy from `conversation.ron`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptionPolicy {
    /// What happens when more captions are active than fit.
    pub overlap: OverlapPolicy,
    /// Rows shown at once under `Stack`.
    pub max_visible: usize,
    /// Entries held at once, including pending and waiting ones.
    pub max_entries: usize,
    /// Minimum time a shown row stays, in microseconds.
    pub min_display_us: u64,
    /// The most a row is held past its planned end for reading speed, in microseconds.
    pub max_extend_us: u64,
    /// How long a waiting entry may wait past its planned end, in microseconds.
    pub max_wait_us: u64,
    /// Reading speed for alphabetic scripts, in graphemes per second.
    pub cps_alphabetic: u32,
    /// Reading speed for Han, Kana and Hangul locales.
    pub cps_cjk: u32,
    /// Lines per page (3 when `text_scale >= 1.5`).
    pub max_lines: usize,
    /// Block width as a fraction of the safe width.
    pub max_width_frac: f32,
    /// Block width limit in logical pixels.
    pub max_width_px: f32,
    /// Raises the block above the game's own HUD, in logical pixels.
    pub bottom_offset: f32,
    /// Keeps a literal newline in a translator-supplied (Fluent) message as an author break, as
    /// `keep_breaks` does for a whole line; off flattens it to a space (spec 56 § 2.8). Set by the
    /// game, like `bottom_offset`; `conversation.ron` does not own it (ADR-7237).
    #[serde(default)]
    pub honor_newlines: bool,
}

impl Default for CaptionPolicy {
    fn default() -> Self {
        Self {
            overlap: OverlapPolicy::Stack,
            max_visible: 3,
            max_entries: 8,
            min_display_us: 1_000_000,
            max_extend_us: 2_000_000,
            max_wait_us: 1_000_000,
            cps_alphabetic: 17,
            cps_cjk: 9,
            max_lines: 2,
            max_width_frac: 0.62,
            max_width_px: 1400.0,
            bottom_offset: 0.0,
            honor_newlines: false,
        }
    }
}

/// What the board does when a caption has no free row.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OverlapPolicy {
    /// Show up to `max_visible` rows; a higher-ranked caption evicts the lowest-ranked.
    #[default]
    Stack,
    /// A new speech row first ends any visible speech row of the same speaker.
    ReplaceSameSpeaker,
    /// One row at a time, in `(priority desc, start asc, id asc)` order.
    Queue,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patch(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn hex_roundtrip_and_rejects() {
        assert_eq!(Rgb::from_hex("#F2C14E"), Some(Rgb(0xF2, 0xC1, 0x4E)));
        assert_eq!(Rgb(1, 2, 3).to_hex(), "#010203");
        assert_eq!(Rgb::from_hex("F2C14E"), None);
        assert_eq!(Rgb::from_hex("#F2C1"), None);
        assert_eq!(Rgb::from_hex("#GGGGGG"), None);
    }

    #[test]
    fn contrast_matches_the_published_endpoints() {
        assert!((Rgb::WHITE.contrast(Rgb::BLACK) - 21.0).abs() < 1e-9);
        assert!((Rgb::WHITE.contrast(Rgb::WHITE) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn dark_speaker_color_is_replaced() {
        let opts = CaptionOptions::default();
        let (color, bg, replaced) = effective_colors(&opts, Rgb(0x20, 0x20, 0x60));
        assert!(replaced);
        assert_eq!(color, Rgb::WHITE);
        assert_eq!(bg, Rgb::BLACK);
        let (color, _, replaced) = effective_colors(&opts, Rgb(0xF2, 0xC1, 0x4E));
        assert!(!replaced);
        assert_eq!(color, Rgb(0xF2, 0xC1, 0x4E));
    }

    #[test]
    fn every_range_is_enforced() {
        let mut opts = CaptionOptions::default();
        for (bad, name) in [
            (json!({"text_scale": 3}), "text_scale"),
            (json!({"text_scale": 0.5}), "text_scale"),
            (json!({"background_opacity": 1.5}), "background_opacity"),
            (json!({"reading_cps": 7}), "reading_cps"),
            (json!({"reading_cps": 41}), "reading_cps"),
        ] {
            match opts.apply_patch(&patch(bad)) {
                Err(OptionError::OutOfRange { name: got, .. }) => assert_eq!(got, name),
                other => panic!("expected out of range for {name}, got {other:?}"),
            }
        }
        assert_eq!(opts, CaptionOptions::default());
    }

    #[test]
    fn unknown_option_and_bad_color_and_bad_value() {
        let mut opts = CaptionOptions::default();
        assert_eq!(
            opts.apply_patch(&patch(json!({"bogus": 1}))),
            Err(OptionError::Unknown("bogus".into()))
        );
        assert_eq!(
            opts.apply_patch(&patch(json!({"text_color": "#12"}))),
            Err(OptionError::InvalidColor("text_color".into()))
        );
        assert_eq!(
            opts.apply_patch(&patch(json!({"position": "Middle"}))),
            Err(OptionError::InvalidValue("position".into()))
        );
    }

    #[test]
    fn a_failed_patch_changes_nothing() {
        let mut opts = CaptionOptions::default();
        let result = opts.apply_patch(&patch(json!({"caption_mode": true, "text_scale": 9})));
        assert!(result.is_err());
        assert!(!opts.caption_mode);
    }

    #[test]
    fn patch_reports_changed_names_and_snaps_scale() {
        let mut opts = CaptionOptions::default();
        let changed = opts
            .apply_patch(&patch(
                json!({"text_scale": 1.52, "enabled": true, "position": "Top"}),
            ))
            .unwrap();
        assert_eq!(changed, vec!["text_scale", "position"]);
        assert!((opts.text_scale - 1.5).abs() < 1e-6);
    }

    #[test]
    fn zero_opacity_box_falls_back_to_outline() {
        let mut opts = CaptionOptions::default();
        opts.background_opacity = 0.0;
        assert_eq!(opts.effective_background(), CaptionBackground::Outline);
        opts.background = CaptionBackground::None;
        assert_eq!(opts.effective_background(), CaptionBackground::None);
    }

    #[test]
    fn caption_mode_forces_labels() {
        let mut opts = CaptionOptions::default();
        opts.speaker_labels = SpeakerLabels::Off;
        assert_eq!(opts.effective_labels(), SpeakerLabels::Off);
        opts.caption_mode = true;
        assert_eq!(opts.effective_labels(), SpeakerLabels::Always);
    }

    #[test]
    fn cps_can_be_reset_to_the_locale_default() {
        let mut opts = CaptionOptions::default();
        opts.apply_patch(&patch(json!({"reading_cps": 20}))).unwrap();
        assert_eq!(opts.reading_cps, Some(20));
        opts.apply_patch(&patch(json!({"reading_cps": null}))).unwrap();
        assert_eq!(opts.reading_cps, None);
    }
}
