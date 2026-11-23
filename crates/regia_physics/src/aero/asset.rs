// SPDX-License-Identifier: MPL-2.0
//! `.aircraft.ron` v1 (ADR-3374): a named [`AircraftConfig`]. See `docs/formats/aircraft-ron.md`.
//!
//! ```ron
//! (
//!     format_version: 1,
//!     name: "trainer",
//!     config: (
//!         mass: 1000.0,
//!         surfaces: [(name: "wing", span: 11.0, chord: 1.5, cl0: 0.25)],
//!     ),
//! )
//! ```
//!
//! Every field of the config and its parts has a default, so a file lists only what differs; the
//! defaults are the sample trainer's ([`crate::aero::trainer`]) for the top level and
//! [`AeroSurface::default`], [`EngineConfig::default`] and [`GearConfig::default`] for list items.

use serde::{Deserialize, Serialize};

use super::{AeroError, AeroSurface, AircraftConfig, ControlLink, EngineConfig, GearConfig};

/// Current `.aircraft.ron` format version.
pub const AIRCRAFT_FORMAT_VERSION: u32 = 1;

/// A `.aircraft.ron`: a named aircraft configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AircraftAsset {
    /// Must be [`AIRCRAFT_FORMAT_VERSION`].
    pub format_version: u32,
    /// A display name (not empty).
    pub name: String,
    /// The aircraft.
    pub config: AircraftConfig,
}

impl Default for AircraftAsset {
    /// The sample trainer at the current version.
    fn default() -> Self {
        Self {
            format_version: AIRCRAFT_FORMAT_VERSION,
            name: "trainer".into(),
            config: AircraftConfig::default(),
        }
    }
}

impl AircraftAsset {
    /// Parses and validates a `.aircraft.ron` text.
    ///
    /// # Errors
    /// [`AeroError::Parse`], [`AeroError::Version`] for another version, or the configuration's own
    /// validation error.
    pub fn from_ron(text: &str) -> Result<Self, AeroError> {
        let found = regia_core::version::peek_ron_version(text).map_err(AeroError::Parse)?;
        if found != AIRCRAFT_FORMAT_VERSION {
            return Err(AeroError::Version {
                found,
                expected: AIRCRAFT_FORMAT_VERSION,
            });
        }
        let asset: Self = ron::from_str(text).map_err(|e| AeroError::Parse(e.to_string()))?;
        asset.validate()?;
        Ok(asset)
    }

    /// Checks the name and the configuration.
    ///
    /// # Errors
    /// [`AeroError::InvalidConfig`] for an empty name, or the configuration's error.
    pub fn validate(&self) -> Result<(), AeroError> {
        if self.name.trim().is_empty() {
            return Err(AeroError::InvalidConfig { field: "name".into() });
        }
        self.config.validate()
    }

    /// The asset as pretty RON text.
    ///
    /// # Errors
    /// [`AeroError::Parse`] when RON cannot write the value (never for a valid one).
    pub fn to_ron(&self) -> Result<String, AeroError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| AeroError::Parse(e.to_string()))
    }

    /// The configuration, consuming the asset.
    #[must_use]
    pub fn into_config(self) -> AircraftConfig {
        self.config
    }
}

/// Versions a migrator accepts below the current one (none at v1).
#[must_use]
pub fn aircraft_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.aircraft.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn aircraft_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<AircraftAsset>(),
        serde_field_names::<AircraftConfig>(),
        serde_field_names::<AeroSurface>(),
        serde_field_names::<ControlLink>(),
        serde_field_names::<EngineConfig>(),
        serde_field_names::<GearConfig>(),
        serde_variant_names::<super::SurfaceKind>(),
        serde_variant_names::<super::ControlChannel>(),
        serde_variant_names::<super::ThrustModel>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_round_trips() {
        let asset = AircraftAsset::default();
        let text = asset.to_ron().expect("write");
        let back = AircraftAsset::from_ron(&text).expect("read");
        assert_eq!(back, asset);
    }

    #[test]
    fn a_sparse_file_takes_the_defaults() {
        let text = r#"(format_version: 1, name: "glider", config: (engines: [], gear: []))"#;
        let asset = AircraftAsset::from_ron(text).expect("read");
        assert!(asset.config.engines.is_empty());
        assert_eq!(asset.config.surfaces.len(), 4);
    }

    #[test]
    fn errors_have_codes() {
        let code = |t: &str| AircraftAsset::from_ron(t).map_err(|e| e.code());
        assert_eq!(code("(("), Err("aircraft_parse"));
        assert_eq!(code(r#"(format_version: 2, name: "x")"#), Err("aircraft_version"));
        assert_eq!(
            code(r#"(format_version: 1, name: " ")"#),
            Err("aircraft_invalid_config")
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (mass: -1.0))"#),
            Err("aircraft_invalid_config")
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (surfaces: []))"#),
            Err("aircraft_no_surfaces")
        );
    }

    #[test]
    fn the_fingerprint_names_the_fields() {
        let f = aircraft_schema_fingerprint();
        for name in [
            "format_version",
            "stall_angle_deg",
            "static_thrust",
            "lateral_friction",
            "Propeller",
            "Fin",
        ] {
            assert!(f.contains(name), "{name} missing from {f}");
        }
    }
}
