// SPDX-License-Identifier: MPL-2.0
//! `.hoverhull.ron` v1 (ADR-8512): a named hover hull.
//!
//! The text is parsed here, on the host. A lockstep match does not compare the text. It compares
//! the hull handshake,
//! [`HoverHullParts::handshake_bytes`] (built
//! by [`hull_handshake_bytes`]): the raw bits of every
//! `params` number, the flight-mode byte and the raw bits of every `limits` number, after the host
//! fills an omitted `limits` block or flight mode with the values it will step. Everything the
//! hull step reads from a file is in those bytes, so a float token that two hosts parse
//! differently, or a peer with other limits or another flight mode, fails the handshake before
//! the first tick instead of desyncing.
//!
//! ```ron
//! (
//!     format_version: 1,
//!     name: "scout",
//!     params: (ride_height: 1.5),
//!     limits: Some((hop_apex_m: 2.0)),
//!     flight: Some("air_time"),
//!     notes: Some("artist note"),
//! )
//! ```
//!
//! Every `params` and `limits` field is optional. A missing field takes
//! [`HoverParams::DEFAULT_HULL`] or
//! [`HoverLimits::DEFAULT`], not `0.0`. An omitted
//! `limits` block means the host keeps its own limits. `flight` is a
//! [`FlightMode`] name (`"air_time"` or `"clamped"`); omitted
//! means the host's mode.
//!
//! [`HoverHullAsset::from_ron`] enables RON
//! `implicit_some`, so a bare number and `Some(number)`
//! are the same (`ride_height: 1.5` or `Some(1.5)`), and a `limits` block may be `(...)` or
//! `Some((...))`. A file may also start with `#![enable(implicit_some)]`. `None` stays written
//! out. A positional tuple is `parse_error`.

use std::fmt;

use regia_detsim::hover::{
    FlightMode, HULL_HANDSHAKE_BYTES, HoverLimits, HoverParams, HoverParamsError, HoverParamsErrorKind,
    hull_handshake_bytes,
};
use serde::{Deserialize, Serialize};

/// Current `.hoverhull.ron` format version.
pub const HOVERHULL_FORMAT_VERSION: u32 = 1;

/// Versions this build reads, and the version a file of that generation is written back as.
/// v1 reads as v1 (there is no older layout).
pub const HOVERHULL_VERSION_TABLE: &[(u32, u32)] = &[(1, 1)];

/// A `.hoverhull.ron`: a name, resolved hull numbers, optional model limits, an optional flight
/// mode name and an optional note.
///
/// Load with [`HoverHullAsset::from_ron`]. The derived deserializer is the schema fingerprint's
/// view of the named fields; a file's missing numbers are filled by `from_ron`, not by `0.0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoverHullAsset {
    /// Must be [`HOVERHULL_FORMAT_VERSION`].
    pub format_version: u32,
    /// Display name. Empty or all whitespace fails `bad_name`.
    pub name: String,
    /// Hull tuning. Missing file fields are [`HoverParams::DEFAULT_HULL`].
    pub params: HoverParams,
    /// Model limits, when the file has a `limits` block. Missing fields inside the block are
    /// [`HoverLimits::DEFAULT`].
    pub limits: Option<HoverLimits>,
    /// [`FlightMode::from_name`] spelling (`"air_time"` or `"clamped"`), or `None` when the file
    /// leaves the mode to the host.
    pub flight: Option<String>,
    /// Free text a tool may show. The step does not read it.
    pub notes: Option<String>,
}

/// The parts a host keeps after a hull file loads.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverHullParts {
    /// Display name.
    pub name: String,
    /// Resolved hull.
    pub params: HoverParams,
    /// Resolved limits, when the file had a block.
    pub limits: Option<HoverLimits>,
    /// Flight mode, when the file named one.
    pub flight: Option<FlightMode>,
    /// Free text, when the file had a note.
    pub notes: Option<String>,
}

impl HoverHullParts {
    /// The hull's lockstep handshake: [`hull_handshake_bytes`] of the params, the file's limits
    /// (or `host_limits` when the file has no `limits` block) and the file's flight mode (or
    /// `host_flight` when it names none).
    ///
    /// Pass the limits and mode the host steps when the file leaves them out, so the bytes are
    /// what the match steps. Every peer compares these bytes before the first tick
    /// ([`hull_handshake_mismatch`](regia_detsim::hover::hull_handshake_mismatch)) and refuses a
    /// peer whose bytes differ.
    #[must_use]
    pub fn handshake_bytes(
        &self,
        host_limits: &HoverLimits,
        host_flight: FlightMode,
    ) -> [u8; HULL_HANDSHAKE_BYTES] {
        hull_handshake_bytes(
            &self.params,
            self.limits.as_ref().unwrap_or(host_limits),
            self.flight.unwrap_or(host_flight),
        )
    }
}

/// Why a `.hoverhull.ron` did not load. [`HoverHullError::code`] is the protocol code.
#[derive(Debug, Clone, PartialEq)]
pub enum HoverHullError {
    /// The text is not a named `.hoverhull.ron` (`parse_error`).
    Parse(String),
    /// `format_version` is not [`HOVERHULL_FORMAT_VERSION`] (`bad_version`).
    BadVersion {
        /// The version the file asked for.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// `name` is empty or only whitespace (`bad_name`).
    BadName,
    /// `flight` is not a [`FlightMode`] name (`bad_flight`).
    BadFlight {
        /// The name the file wrote.
        name: String,
    },
    /// A hull or limits field failed validation (`not_finite`, `negative`, `zero_rate`,
    /// `out_of_range`, `inverted_range`). A limits field's name starts with `limits.`.
    Field {
        /// The field name.
        field: String,
        /// What is wrong with it.
        kind: HoverParamsErrorKind,
        /// The value it holds.
        value: f32,
        /// The valid range, `[min, max]`.
        range: [f32; 2],
    },
}

impl HoverHullError {
    /// The error code: lowercase ascii, digits and `_`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            HoverHullError::Parse(_) => "parse_error",
            HoverHullError::BadVersion { .. } => "bad_version",
            HoverHullError::BadName => "bad_name",
            HoverHullError::BadFlight { .. } => "bad_flight",
            HoverHullError::Field { kind, .. } => kind.code(),
        }
    }

    /// The field the error names, when it names one.
    #[must_use]
    pub fn field_name(&self) -> Option<&str> {
        match self {
            HoverHullError::Field { field, .. } => Some(field),
            HoverHullError::BadFlight { .. } => Some("flight"),
            HoverHullError::BadName => Some("name"),
            HoverHullError::BadVersion { .. } => Some("format_version"),
            HoverHullError::Parse(_) => None,
        }
    }

    fn from_limits(err: HoverParamsError) -> Self {
        HoverHullError::Field {
            field: format!("limits.{}", err.field),
            kind: err.kind,
            value: err.value,
            range: err.range,
        }
    }
}

impl From<HoverParamsError> for HoverHullError {
    fn from(err: HoverParamsError) -> Self {
        HoverHullError::Field {
            field: err.field.to_string(),
            kind: err.kind,
            value: err.value,
            range: err.range,
        }
    }
}

impl fmt::Display for HoverHullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HoverHullError::Parse(message) => write!(f, "parse_error: {message}"),
            HoverHullError::BadVersion { found, expected } => write!(
                f,
                "bad_version: .hoverhull.ron version {found} (this build reads {expected})"
            ),
            HoverHullError::BadName => write!(f, "bad_name: hover hull name is empty"),
            HoverHullError::BadFlight { name } => {
                write!(f, "bad_flight: unknown flight mode `{name}`")
            }
            HoverHullError::Field {
                field,
                kind,
                value,
                range,
            } => {
                let [min, max] = range;
                write!(
                    f,
                    "{}: hover field `{field}` = {value} (valid {min} to {max})",
                    kind.code()
                )
            }
        }
    }
}

impl std::error::Error for HoverHullError {}

/// One optional `f32` per resolved field, so a missing file field stays the default hull's bits
/// (including a written `-0.0`) instead of `f32`'s `0.0`.
macro_rules! opt_f32_file {
    ($name:ident, $base:ty, $($field:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct $name {
            $(
                #[serde(default)]
                $field: Option<f32>,
            )+
        }

        impl $name {
            fn apply(self, mut base: $base) -> $base {
                $(
                    if let Some(value) = self.$field {
                        base.$field = value;
                    }
                )+
                base
            }
        }
    };
}

opt_f32_file!(
    FileParams,
    HoverParams,
    ride_height,
    coast_drag,
    thrust_drag,
    height_spring,
    height_damper,
    lean_pitch_per_throttle,
    lean_pitch_per_speed,
    lean_roll_per_strafe,
    lean_roll_per_yaw_rate,
    max_forward_speed,
    max_reverse_speed,
    max_strafe_speed,
    thrust_accel,
    brake_accel,
    yaw_rate_at_rest,
    yaw_rate_at_speed,
    yaw_response,
    jump_speed,
    lateral_drag,
    thrust_taper,
    yaw_release_response,
    yaw_response_at_speed,
    yaw_deadband,
    roll_slip,
    hover_lead,
    slope_pull,
    accel_drag_high,
    drag_knee,
    turbo_scale,
    lat_grip,
    turn_bleed,
    strafe_thrust_share,
    reverse_thrust_share,
    coast_drop,
    coast_drop_share,
    roll_bank,
    coast_drop_tau,
    roll_spring_omega,
    roll_spring_zeta,
    cushion_compress_damp_share,
    cushion_terrain_share,
    cushion_pitch_omega,
    cushion_pitch_zeta,
    cushion_reach_full_m,
    cushion_reach_band_m,
);

opt_f32_file!(
    FileLimits,
    HoverLimits,
    hop_apex_m,
    hop_cooldown_s,
    hop_land_keep,
    jump_feel_gravity_min,
    aim_modern_yaw_alpha,
    lat_straighten_grip,
    airborne_steer_authority,
    airborne_eps_m,
    drag_input_threshold,
    crest_fall_min_m_s2,
    crest_soft_band_m,
    slope_sin_max,
    hover_lead_smooth_s,
    hover_cliff_slope,
    grade_smooth_s,
    grade_teleport_m,
    overspeed_bleed,
    air_attitude_m,
    air_lift_max_g,
    free_roll_rate_rad_s,
    free_roll_track,
    roll_lean_max,
    cushion_pitch_max,
);

/// The file shape `from_ron` reads. Public [`HoverHullAsset`] is the resolved value.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HoverHullFile {
    format_version: u32,
    name: String,
    #[serde(default)]
    params: FileParams,
    #[serde(default)]
    limits: Option<FileLimits>,
    #[serde(default)]
    flight: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

impl HoverHullAsset {
    /// Parses and validates a `.hoverhull.ron` text. Fields are named; a positional tuple is
    /// `parse_error`.
    ///
    /// RON `implicit_some` is on, so `ride_height: 1.5` and `Some(1.5)` both load, and so do
    /// `limits: (hop_apex_m: 2.0)` and `limits: Some((hop_apex_m: 2.0))`. A file may also start
    /// with `#![enable(implicit_some)]`.
    ///
    /// # Errors
    /// [`HoverHullError`] when the text is not this version, the name or flight mode is unusable,
    /// or a number is outside its range.
    pub fn from_ron(text: &str) -> Result<Self, HoverHullError> {
        let found = regia_core::version::peek_ron_version(text).map_err(HoverHullError::Parse)?;
        if found != HOVERHULL_FORMAT_VERSION {
            return Err(HoverHullError::BadVersion {
                found,
                expected: HOVERHULL_FORMAT_VERSION,
            });
        }
        let file: HoverHullFile = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(text)
            .map_err(|err| HoverHullError::Parse(err.to_string()))?;
        if file.format_version != HOVERHULL_FORMAT_VERSION {
            return Err(HoverHullError::BadVersion {
                found: file.format_version,
                expected: HOVERHULL_FORMAT_VERSION,
            });
        }
        let asset = Self {
            format_version: HOVERHULL_FORMAT_VERSION,
            name: file.name,
            params: file.params.apply(HoverParams::DEFAULT_HULL),
            limits: file.limits.map(|block| block.apply(HoverLimits::DEFAULT)),
            flight: file.flight,
            notes: file.notes,
        };
        asset.validate()?;
        Ok(asset)
    }

    /// Checks the version, the name, the flight mode and every hull and limits field.
    ///
    /// # Errors
    /// The same codes as [`HoverHullAsset::from_ron`].
    pub fn validate(&self) -> Result<(), HoverHullError> {
        if self.format_version != HOVERHULL_FORMAT_VERSION {
            return Err(HoverHullError::BadVersion {
                found: self.format_version,
                expected: HOVERHULL_FORMAT_VERSION,
            });
        }
        if self.name.trim().is_empty() {
            return Err(HoverHullError::BadName);
        }
        if let Some(name) = &self.flight
            && FlightMode::from_name(name).is_none()
        {
            return Err(HoverHullError::BadFlight { name: name.clone() });
        }
        self.params.validate()?;
        if let Some(limits) = self.limits {
            limits.validate().map_err(HoverHullError::from_limits)?;
        }
        Ok(())
    }

    /// The asset as pretty RON text at [`HOVERHULL_FORMAT_VERSION`].
    ///
    /// # Errors
    /// [`HoverHullError::Parse`] when RON cannot write the value (never for a valid one).
    pub fn to_ron(&self) -> Result<String, HoverHullError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|err| HoverHullError::Parse(err.to_string()))
    }

    /// The resolved parts, after [`HoverHullAsset::validate`].
    ///
    /// # Errors
    /// The same codes as [`HoverHullAsset::validate`].
    pub fn into_parts(self) -> Result<HoverHullParts, HoverHullError> {
        self.validate()?;
        let flight = match &self.flight {
            Some(name) => Some(
                FlightMode::from_name(name)
                    .ok_or_else(|| HoverHullError::BadFlight { name: name.clone() })?,
            ),
            None => None,
        };
        Ok(HoverHullParts {
            name: self.name,
            params: self.params,
            limits: self.limits,
            flight,
            notes: self.notes,
        })
    }
}

/// Reads a v1 file and writes it back in canonical form. Any other version is `bad_version`.
///
/// # Errors
/// The same codes as [`HoverHullAsset::from_ron`].
pub fn migrate_hoverhull(text: &str) -> Result<String, HoverHullError> {
    HoverHullAsset::from_ron(text)?.to_ron()
}

/// Versions a migrator accepts below the current one (none at v1).
#[must_use]
pub fn hoverhull_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.hoverhull.ron` (for `cargo xtask check-formats`).
///
/// The file's optional number lists and the resolved [`HoverParams`] / [`HoverLimits`] lists are
/// both included, so a field added on either side changes the hash.
#[must_use]
pub fn hoverhull_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<HoverHullAsset>(),
        serde_field_names::<HoverParams>(),
        serde_field_names::<HoverLimits>(),
        serde_field_names::<FileParams>(),
        serde_field_names::<FileLimits>(),
        serde_field_names::<HoverHullFile>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(text: &str) -> &'static str {
        HoverHullAsset::from_ron(text).expect_err(text).code()
    }

    #[test]
    fn hoverhull_v1_round_trips_canonical_text() {
        let asset = HoverHullAsset::from_ron(
            r#"(
                format_version: 1,
                name: "scout",
                params: (ride_height: 2.0),
                limits: Some((hop_apex_m: 2.0)),
                flight: Some("clamped"),
                notes: Some("note"),
            )"#,
        )
        .expect("load");
        let text = asset.to_ron().expect("write");
        let again = HoverHullAsset::from_ron(&text).expect("reload");
        assert_eq!(again, asset);
        assert_eq!(again.to_ron().expect("rewrite"), text);
        let parts = asset.into_parts().expect("parts");
        assert_eq!(parts.name, "scout");
        assert_eq!(parts.flight, Some(FlightMode::Clamped));
        assert_eq!(parts.limits.expect("limits").hop_apex_m, 2.0);
        assert_eq!(parts.notes.as_deref(), Some("note"));
        assert_eq!(parts.params.ride_height, 2.0);
        assert_eq!(parts.params.coast_drag, HoverParams::DEFAULT_HULL.coast_drag);
    }

    #[test]
    fn missing_fields_take_the_default_hull() {
        let asset = HoverHullAsset::from_ron(r#"(format_version: 1, name: "sparse")"#).expect("load");
        assert_eq!(asset.params, HoverParams::DEFAULT_HULL);
        assert!(asset.limits.is_none());
        assert!(asset.flight.is_none());
        assert!(asset.notes.is_none());
        let raised =
            HoverHullAsset::from_ron(r#"(format_version: 1, name: "sparse", params: (ride_height: 2.0))"#)
                .expect("one field");
        assert_eq!(raised.params.ride_height, 2.0);
        assert_eq!(
            raised.params.max_forward_speed,
            HoverParams::DEFAULT_HULL.max_forward_speed
        );
        let signed =
            HoverHullAsset::from_ron(r#"(format_version: 1, name: "sparse", params: (ride_height: -0.0))"#)
                .expect("signed zero");
        assert_eq!(signed.params.ride_height.to_bits(), (-0.0_f32).to_bits());
        assert_eq!(
            signed.params.to_le_bytes()[0..4],
            (-0.0_f32).to_bits().to_le_bytes()
        );
    }

    #[test]
    fn a_bad_field_names_it_on_load() {
        let params = |body: &str| format!(r#"(format_version: 1, name: "hull", params: ({body}))"#);
        assert_eq!(code_of(&params("ride_height: NaN")), "not_finite");
        assert_eq!(code_of(&params("ride_height: -1.0")), "negative");
        assert_eq!(code_of(&params("coast_drop_tau: 0.0")), "zero_rate");
        assert_eq!(code_of(&params("ride_height: 100.0")), "out_of_range");
        assert_eq!(code_of(&params("yaw_deadband: 4.0")), "inverted_range");
        assert_eq!(code_of("(("), "parse_error");
        assert_eq!(code_of(r#"(format_version: 2, name: "hull")"#), "bad_version");
        assert_eq!(code_of(r#"(format_version: 1, name: " ")"#), "bad_name");
        assert_eq!(
            code_of(r#"(format_version: 1, name: "hull", flight: Some("nope"))"#),
            "bad_flight"
        );
        assert_eq!(code_of(r#"(1, "hull")"#), "parse_error");
        assert_eq!(
            code_of(r#"(format_version: 1, name: "hull", params: (1.0))"#),
            "parse_error"
        );

        let finite = HoverHullAsset::from_ron(&params("ride_height: NaN")).expect_err("nan");
        assert_eq!(finite.field_name(), Some("ride_height"));
        let negative = HoverHullAsset::from_ron(&params("ride_height: -1.0")).expect_err("neg");
        assert_eq!(negative.field_name(), Some("ride_height"));
        let rate = HoverHullAsset::from_ron(&params("coast_drop_tau: 0.0")).expect_err("rate");
        assert_eq!(rate.field_name(), Some("coast_drop_tau"));
        let range = HoverHullAsset::from_ron(&params("ride_height: 100.0")).expect_err("range");
        assert_eq!(range.field_name(), Some("ride_height"));
        let inverted = HoverHullAsset::from_ron(&params("yaw_deadband: 4.0")).expect_err("yaw");
        assert_eq!(inverted.code(), "inverted_range");
        assert_eq!(inverted.field_name(), Some("yaw_deadband"));

        let limits = HoverHullAsset::from_ron(
            r#"(format_version: 1, name: "hull", limits: Some((hop_apex_m: -1.0)))"#,
        )
        .expect_err("limits");
        assert_eq!(limits.code(), "negative");
        assert_eq!(limits.field_name(), Some("limits.hop_apex_m"));

        let version = HoverHullAsset::from_ron(r#"(format_version: 9, name: "hull")"#).expect_err("ver");
        assert!(version.to_string().to_ascii_lowercase().contains("version"));
    }

    #[test]
    fn migrate_v1_rewrites_canonically_and_refuses_another_version() {
        let text = HoverHullAsset::from_ron(r#"(format_version: 1, name: "sparse")"#)
            .expect("load")
            .to_ron()
            .expect("write");
        assert_eq!(migrate_hoverhull(&text).expect("migrate"), text);
        assert_eq!(
            migrate_hoverhull(r#"(format_version: 3, name: "x")"#)
                .expect_err("future")
                .code(),
            "bad_version"
        );
        assert!(hoverhull_migration_sources().is_empty());
        assert_eq!(HOVERHULL_VERSION_TABLE, &[(1, 1)]);
    }

    fn parts(text: &str) -> HoverHullParts {
        HoverHullAsset::from_ron(text)
            .expect(text)
            .into_parts()
            .expect(text)
    }

    /// ADR-8632: a file that differs only in one `limits` field or only in `flight` steps
    /// differently, so its handshake differs and names the field. At 5dcfea41f the handshake was
    /// `params.to_le_bytes()` alone and these pairs matched.
    #[test]
    fn every_step_input_of_the_file_is_in_the_handshake() {
        use regia_detsim::hover::{HullHandshakeMismatch, hull_handshake_mismatch};

        let host = HoverLimits::DEFAULT;
        let mode = FlightMode::AirTime;
        let digest = |text: &str| parts(text).handshake_bytes(&host, mode);
        let base = digest(r#"(format_version: 1, name: "a", limits: ())"#);
        assert_eq!(
            digest(r#"(format_version: 1, name: "a")"#),
            base,
            "omitted = host default"
        );
        assert_eq!(
            digest(r#"(format_version: 1, name: "a", flight: Some("air_time"))"#),
            base,
            "named mode = host mode"
        );

        for field in HoverLimits::FIELD_RANGES {
            let default = (field.get)(&HoverLimits::DEFAULT);
            let value = if default.to_bits() == field.max.to_bits() {
                field.min
            } else {
                field.max
            };
            let text = format!(
                r#"(format_version: 1, name: "b", limits: ({}: {value:?}))"#,
                field.name
            );
            let peer = parts(&text);
            assert_eq!(
                peer.params.to_le_bytes(),
                HoverParams::DEFAULT_HULL.to_le_bytes(),
                "{text}"
            );
            assert_eq!(
                hull_handshake_mismatch(&base, &peer.handshake_bytes(&host, mode)),
                Some(HullHandshakeMismatch::Limits(field.name)),
                "{text}"
            );
        }

        let clamped = digest(r#"(format_version: 1, name: "c", flight: Some("clamped"))"#);
        assert_eq!(
            hull_handshake_mismatch(&base, &clamped),
            Some(HullHandshakeMismatch::Flight)
        );

        let plus = digest(r#"(format_version: 1, name: "z", limits: (hop_apex_m: 0.0))"#);
        let minus = digest(r#"(format_version: 1, name: "z", limits: (hop_apex_m: -0.0))"#);
        assert_eq!(
            hull_handshake_mismatch(&plus, &minus),
            Some(HullHandshakeMismatch::Limits("hop_apex_m"))
        );

        // An omitted block takes the host's limits: a host that steps other limits has other bytes.
        let mut tall = HoverLimits::DEFAULT;
        tall.hop_apex_m = 10.0;
        let on_tall_host = parts(r#"(format_version: 1, name: "a")"#).handshake_bytes(&tall, mode);
        assert_eq!(
            hull_handshake_mismatch(&base, &on_tall_host),
            Some(HullHandshakeMismatch::Limits("hop_apex_m"))
        );
        let on_clamped_host =
            parts(r#"(format_version: 1, name: "a")"#).handshake_bytes(&host, FlightMode::Clamped);
        assert_eq!(on_clamped_host, clamped);
    }

    /// Canonical `to_ron` text keeps every bit the handshake reads: a hull with signed zeros in
    /// `params` and `limits` and a named mode reloads to the same 273 bytes.
    #[test]
    fn canonical_text_round_trips_the_handshake_bytes() {
        let text = r#"(format_version: 1, name: "z", params: (ride_height: -0.0, lean_pitch_per_throttle: -0.0),
            limits: (roll_lean_max: -0.0, hop_apex_m: 0.1), flight: Some("clamped"))"#;
        let asset = HoverHullAsset::from_ron(text).expect("load");
        let again = HoverHullAsset::from_ron(&asset.to_ron().expect("write")).expect("reload");
        let host = HoverLimits::DEFAULT;
        let before = asset
            .into_parts()
            .expect("parts")
            .handshake_bytes(&host, FlightMode::AirTime);
        let after = again
            .into_parts()
            .expect("parts")
            .handshake_bytes(&host, FlightMode::AirTime);
        assert_eq!(before, after);
        assert_eq!(before[0..4], (-0.0_f32).to_bits().to_le_bytes());
        assert_eq!(before[180], 1);
    }

    /// The review's pairs (sprint-buildout-mp-hover-hull-format-review): same params, different
    /// step. The handshake must refuse each pair.
    #[test]
    fn the_handshake_refuses_pairs_that_step_differently() {
        use regia_detsim::hover::{
            HoverState, WorldPhysics, hop_launch_speed_for, hull_handshake_mismatch, step_tuned,
        };

        let host = HoverLimits::DEFAULT;
        let air = parts(r#"(format_version: 1, name: "air")"#);
        let clamped = parts(r#"(format_version: 1, name: "clamped", flight: Some("clamped"))"#);
        assert_eq!(air.params.to_le_bytes(), clamped.params.to_le_bytes());
        let dt = 1.0 / 60.0;
        let mut air_state = HoverState::default();
        let mut clamped_state = HoverState::default();
        let air_mode = air.flight.unwrap_or_default();
        let clamped_mode = clamped.flight.unwrap_or_default();
        step_tuned(
            &air.params,
            &mut air_state,
            &[0.0; 3],
            0.0,
            dt,
            9.8,
            0.0,
            air_mode.climb(),
        );
        step_tuned(
            &clamped.params,
            &mut clamped_state,
            &[0.0; 3],
            0.0,
            dt,
            9.8,
            0.0,
            clamped_mode.climb(),
        );
        assert_ne!(
            air_state.vertical_vel.to_bits(),
            clamped_state.vertical_vel.to_bits()
        );
        assert!(
            hull_handshake_mismatch(
                &air.handshake_bytes(&host, FlightMode::AirTime),
                &clamped.handshake_bytes(&host, FlightMode::AirTime)
            )
            .is_some()
        );

        let plain = parts(r#"(format_version: 1, name: "a")"#);
        let tall = parts(r#"(format_version: 1, name: "b", limits: (hop_apex_m: 10.0))"#);
        assert_eq!(plain.params.to_le_bytes(), tall.params.to_le_bytes());
        let world = WorldPhysics::EARTH;
        let plain_limits = plain.limits.unwrap_or(host);
        let tall_limits = tall.limits.unwrap_or(host);
        assert_ne!(
            hop_launch_speed_for(&plain_limits, &world, dt).to_bits(),
            hop_launch_speed_for(&tall_limits, &world, dt).to_bits()
        );
        assert!(
            hull_handshake_mismatch(
                &plain.handshake_bytes(&host, FlightMode::AirTime),
                &tall.handshake_bytes(&host, FlightMode::AirTime)
            )
            .is_some()
        );
    }

    #[test]
    fn schema_fingerprint_covers_every_tunable() {
        let fp = hoverhull_schema_fingerprint();
        for field in HoverParams::FIELD_RANGES {
            assert!(
                fp.split([',', ';']).any(|name| name == field.name),
                "{name} missing from {fp}",
                name = field.name
            );
        }
        for field in HoverLimits::FIELD_RANGES {
            assert!(
                fp.split([',', ';']).any(|name| name == field.name),
                "{name} missing from {fp}",
                name = field.name
            );
        }
        let hash = blake3::hash(fp.as_bytes()).to_hex().to_string();
        assert_eq!(
            hash, "6c8d0ac68f306f40d08ddcb53abc6b3937d4c8782197b2f3b78a137b0342a7e5",
            "live fingerprint hash {hash} of {fp}"
        );
    }
}
