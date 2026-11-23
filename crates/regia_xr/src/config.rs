// SPDX-License-Identifier: MPL-2.0
//! `config/xr.ron` (spec 58 § 4.1): the project's XR settings, `format_version` 1.

use regia_app::prelude::*;
use regia_platform::xr::XrError;
use serde::{Deserialize, Serialize};

use crate::comfort::ComfortSettings;
use crate::space::TrackingSpace;

/// The `format_version` this build writes and the newest it reads.
pub const XR_CONFIG_FORMAT_VERSION: u32 = 1;

/// Where the project keeps the file (relative to the project root).
pub const XR_CONFIG_PATH: &str = "config/xr.ron";

/// Smallest and largest render scale (a multiplier on the runtime's recommended eye size).
pub const RENDER_SCALE_RANGE: (f32, f32) = (0.5, 2.0);

/// Largest `retry_frames` (about a minute of fixed steps at 120 Hz).
pub const MAX_RETRY_FRAMES: u32 = 7200;

/// Which backend [`crate::XrPlugin`] opens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum XrBackendKind {
    /// The OpenXR runtime when one is reachable, else XR stays off and the game runs on the desktop.
    #[default]
    Auto,
    /// The OpenXR runtime only.
    OpenXr,
    /// The deterministic simulator (`NullXr` plus the script player): no headset, no GPU needed.
    Simulator,
    /// XR disabled.
    Off,
}

impl XrBackendKind {
    /// Every backend, in order.
    pub const ALL: [XrBackendKind; 4] = [
        XrBackendKind::Auto,
        XrBackendKind::OpenXr,
        XrBackendKind::Simulator,
        XrBackendKind::Off,
    ];

    /// The backend named `name`.
    pub fn from_name(name: &str) -> Option<XrBackendKind> {
        XrBackendKind::ALL.into_iter().find(|b| b.name() == name)
    }

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            XrBackendKind::Auto => "auto",
            XrBackendKind::OpenXr => "openxr",
            XrBackendKind::Simulator => "simulator",
            XrBackendKind::Off => "off",
        }
    }
}

/// Fixed-foveation level applied to the eye swapchains when the runtime offers the extension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum Foveation {
    /// No foveation.
    #[default]
    Off,
    /// Light.
    Low,
    /// Medium.
    Medium,
    /// Strong.
    High,
}

impl Foveation {
    /// Every level, in order.
    pub const ALL: [Foveation; 4] = [Foveation::Off, Foveation::Low, Foveation::Medium, Foveation::High];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            Foveation::Off => "off",
            Foveation::Low => "low",
            Foveation::Medium => "medium",
            Foveation::High => "high",
        }
    }

    /// The level named `name`.
    pub fn from_name(name: &str) -> Option<Foveation> {
        Foveation::ALL.into_iter().find(|f| f.name() == name)
    }
}

/// What the desktop mirror window shows (no mirror on Android).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum Mirror {
    /// No mirror.
    Off,
    /// The left eye.
    #[default]
    Left,
    /// The right eye.
    Right,
    /// Both eyes, cropped to fill.
    Both,
}

impl Mirror {
    /// Every mode, in order.
    pub const ALL: [Mirror; 4] = [Mirror::Off, Mirror::Left, Mirror::Right, Mirror::Both];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            Mirror::Off => "off",
            Mirror::Left => "left",
            Mirror::Right => "right",
            Mirror::Both => "both",
        }
    }

    /// The mode named `name`.
    pub fn from_name(name: &str) -> Option<Mirror> {
        Mirror::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// The project's XR settings (`config/xr.ron`). An absent file is `XrConfig::default()`: backend
/// `Auto`, not required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Resource, Reflect)]
#[serde(deny_unknown_fields, default)]
pub struct XrConfig {
    /// The file's format version ([`XR_CONFIG_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Which backend to open.
    pub backend: XrBackendKind,
    /// True: the app refuses to start without a session. False (default): a missing headset or
    /// runtime leaves the desktop game running.
    pub required: bool,
    /// The requested tracking space (the actual one may fall back, see `resolve_tracking_space`).
    pub tracking_space: TrackingSpace,
    /// Multiplier on the runtime's recommended per-eye size, 0.5 to 2.0.
    pub render_scale: f32,
    /// Fixed-foveation level (best effort).
    pub foveation: Foveation,
    /// The desktop mirror window.
    pub mirror: Mirror,
    /// Comfort options.
    pub comfort: ComfortSettings,
    /// Display refresh rate to request, when the runtime offers refresh-rate control.
    pub preferred_refresh_hz: Option<f32>,
    /// Fixed steps between attempts to reach a headset or to re-create a lost session.
    pub retry_frames: u32,
    /// Near plane of the eye projections, meters.
    pub near_m: f32,
}

impl Default for XrConfig {
    fn default() -> Self {
        XrConfig {
            format_version: XR_CONFIG_FORMAT_VERSION,
            backend: XrBackendKind::Auto,
            required: false,
            tracking_space: TrackingSpace::Stage,
            render_scale: 1.0,
            foveation: Foveation::Off,
            mirror: Mirror::Left,
            comfort: ComfortSettings::default(),
            preferred_refresh_hz: None,
            retry_frames: 120,
            near_m: 0.05,
        }
    }
}

fn invalid(key: &str, reason: impl Into<String>) -> XrError {
    XrError::InvalidValue {
        key: key.to_string(),
        reason: reason.into(),
    }
}

impl XrConfig {
    /// Parses `config/xr.ron`. The `format_version` is checked first: a newer one is
    /// [`XrError::FormatTooNew`]; an unknown field is a load error naming the field
    /// ([`XrError::InvalidValue`] with key `xr.ron`); then [`XrConfig::validate`].
    pub fn from_ron(text: &str) -> Result<XrConfig, XrError> {
        let version = read_format_version(text).map_err(|e| invalid("xr.ron", e))?;
        if version > XR_CONFIG_FORMAT_VERSION {
            return Err(XrError::FormatTooNew {
                what: "xr.ron",
                found: version,
                supported: XR_CONFIG_FORMAT_VERSION,
            });
        }
        let config: XrConfig = ron::from_str(text).map_err(|e| invalid("xr.ron", e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// The canonical RON text ([`XrConfig::from_ron`] reads it back equal).
    pub fn to_ron(&self) -> Result<String, XrError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| invalid("xr.ron", e.to_string()))
    }

    /// Checks every range: `format_version` 1, `render_scale` 0.5-2.0, `retry_frames` 1 to
    /// [`MAX_RETRY_FRAMES`], `near_m` finite and positive, `preferred_refresh_hz` 30-240, and the
    /// comfort ranges ([`ComfortSettings::validate`]).
    pub fn validate(&self) -> Result<(), XrError> {
        if self.format_version == 0 || self.format_version > XR_CONFIG_FORMAT_VERSION {
            return Err(invalid(
                "format_version",
                format!("must be 1..={XR_CONFIG_FORMAT_VERSION}"),
            ));
        }
        let (lo, hi) = RENDER_SCALE_RANGE;
        if !(self.render_scale.is_finite() && (lo..=hi).contains(&self.render_scale)) {
            return Err(invalid("render_scale", format!("must be {lo}..={hi}")));
        }
        if !(1..=MAX_RETRY_FRAMES).contains(&self.retry_frames) {
            return Err(invalid("retry_frames", format!("must be 1..={MAX_RETRY_FRAMES}")));
        }
        if !(self.near_m.is_finite() && self.near_m > 0.0) {
            return Err(invalid("near_m", "must be finite and > 0"));
        }
        if let Some(hz) = self.preferred_refresh_hz
            && !(hz.is_finite() && (30.0..=240.0).contains(&hz))
        {
            return Err(invalid("preferred_refresh_hz", "must be 30..=240"));
        }
        self.comfort.validate()
    }
}

/// Reads `format_version` from a RON struct before the full parse, so a newer file is refused by
/// version rather than by whichever field it added. A file without the field is version 1.
pub(crate) fn read_format_version(text: &str) -> Result<u32, String> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default = "one")]
        format_version: u32,
    }
    fn one() -> u32 {
        1
    }
    // Unknown fields are ignored by the probe (no deny_unknown_fields).
    let probe: Probe = ron::from_str(text).map_err(|e| e.to_string())?;
    Ok(probe.format_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = "(format_version: 1, backend: Auto, required: false, tracking_space: Stage,
 render_scale: 1.0, foveation: Off, mirror: Left, preferred_refresh_hz: Some(90.0),
 retry_frames: 120,
 comfort: (snap_turn_deg: 30.0, vignette_strength: 0.5, eye_height_m: 1.65, dominant_hand: Right))";

    #[test]
    fn the_spec_example_loads() {
        let c = XrConfig::from_ron(SPEC_EXAMPLE).expect("loads");
        assert_eq!(c.backend, XrBackendKind::Auto);
        assert_eq!(c.tracking_space, TrackingSpace::Stage);
        assert_eq!(c.preferred_refresh_hz, Some(90.0));
        assert_eq!(c.comfort.snap_turn_deg, 30.0);
        assert_eq!(c.comfort.dominant_hand, crate::comfort::DominantHand::Right);
    }

    #[test]
    fn an_empty_struct_is_the_default_and_round_trips() {
        let c = XrConfig::from_ron("()").expect("defaults");
        assert_eq!(c, XrConfig::default());
        assert!(!c.required);
        let text = c.to_ron().expect("ser");
        assert_eq!(XrConfig::from_ron(&text).expect("reload"), c);
    }

    #[test]
    fn an_unknown_field_is_an_error_naming_it() {
        let e = XrConfig::from_ron("(format_version: 1, warp_drive: true)").expect_err("unknown");
        assert_eq!(e.code(), "xr_invalid_value");
        assert!(e.to_string().contains("warp_drive"), "{e}");
    }

    #[test]
    fn a_newer_format_is_refused_by_version() {
        let e = XrConfig::from_ron("(format_version: 2, a_v2_field: 1)").expect_err("newer");
        assert_eq!(
            e,
            XrError::FormatTooNew {
                what: "xr.ron",
                found: 2,
                supported: 1
            }
        );
        assert_eq!(e.code(), "xr_format_too_new");
    }

    #[test]
    fn ranges_are_checked_with_the_range_in_the_message() {
        for (text, key) in [
            ("(render_scale: 3.0)", "render_scale"),
            ("(render_scale: 0.1)", "render_scale"),
            ("(retry_frames: 0)", "retry_frames"),
            ("(near_m: 0.0)", "near_m"),
            ("(preferred_refresh_hz: Some(1000.0))", "preferred_refresh_hz"),
            ("(format_version: 0)", "format_version"),
            ("(comfort: (snap_turn_deg: 200.0))", "snap_turn_deg"),
        ] {
            let e = XrConfig::from_ron(text).expect_err(text);
            match e {
                XrError::InvalidValue { key: got, .. } => assert_eq!(got, key, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
    }
}
