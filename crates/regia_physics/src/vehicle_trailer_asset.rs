// SPDX-License-Identifier: MPL-2.0
//! `.trailer.ron`: a trailer configuration as a content asset (v2, ADR-3281/4590).
//!
//! The file holds a [`TrailerConfig`] under a `format_version` and a display `name`. Its
//! `chassis` section is a `.vehicle.ron` v4 vehicle configuration (no wheel may drive or steer).
//! Load it with [`TrailerAsset::from_ron`] and spawn it with `Trailer::spawn` or `spawn_trailer`;
//! the agent method `physics.trailer_config_check` validates one without spawning. See
//! `docs/formats/trailer.md`.

use serde::{Deserialize, Serialize};

use crate::vehicle_tire::SlipTire;
use crate::vehicle_trailer::{
    CouplingConfig, CouplingKind, StandConfig, TrailerBrakes, TrailerConfig, TrailerError,
};

/// Current `.trailer.ron` format version. v2: the chassis section is a `.vehicle.ron` v4
/// configuration (the tire transient and camber fields, ADR-3276).
pub const TRAILER_FORMAT_VERSION: u32 = 2;

/// Why a `.trailer.ron` did not load.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TrailerAssetError {
    /// The text is not valid RON for the format.
    #[error("trailer asset does not parse: {0}")]
    Parse(String),
    /// The file's `format_version` is not one this build reads.
    #[error("trailer asset format version {found}; this build reads {expected}")]
    Version {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// The name is empty.
    #[error("trailer asset has an empty name")]
    EmptyName,
    /// The configuration failed [`TrailerConfig::validate`].
    #[error("trailer config: {0}")]
    Config(#[from] TrailerError),
}

impl TrailerAssetError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs). A config
    /// problem reports the [`TrailerError`] code (a chassis problem the `VehicleError` code).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "trailer_asset_parse",
            Self::Version { .. } => "trailer_asset_version",
            Self::EmptyName => "trailer_asset_name",
            Self::Config(e) => e.code(),
        }
    }
}

/// A `.trailer.ron`: a named [`TrailerConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrailerAsset {
    /// [`TRAILER_FORMAT_VERSION`].
    pub format_version: u32,
    /// Display name (not empty).
    pub name: String,
    /// The trailer: chassis, coupling, brakes, parking brake, stand and rear hitch.
    pub config: TrailerConfig,
}

impl TrailerAsset {
    /// An asset of the current version.
    #[must_use]
    pub fn new(name: impl Into<String>, config: TrailerConfig) -> Self {
        Self {
            format_version: TRAILER_FORMAT_VERSION,
            name: name.into(),
            config,
        }
    }

    /// Parses and validates a `.trailer.ron` text.
    ///
    /// # Errors
    /// [`TrailerAssetError`]: a parse failure, a version this build cannot read, an empty name,
    /// or a configuration [`TrailerConfig::validate`] rejects.
    pub fn from_ron(text: &str) -> Result<Self, TrailerAssetError> {
        let found = regia_core::version::peek_ron_version(text).map_err(TrailerAssetError::Parse)?;
        if found != TRAILER_FORMAT_VERSION && !trailer_migration_sources().contains(&found) {
            return Err(TrailerAssetError::Version {
                found,
                expected: TRAILER_FORMAT_VERSION,
            });
        }
        let mut asset: Self = ron::from_str(text).map_err(|e| TrailerAssetError::Parse(e.to_string()))?;
        if found == 1
            && asset
                .config
                .chassis
                .wheels
                .iter()
                .any(|wheel| wheel.tire_model.slip().is_some_and(SlipTire::has_transients))
        {
            // The chassis section of a v1 file predates the `.vehicle.ron` v4 tire transient
            // fields; `.vehicle.ron` refuses them in older versions, and so does the trailer.
            return Err(TrailerAssetError::Parse(
                "a v1 file cannot hold the tire fields relaxation_length, pneumatic_trail, \
                 camber_stiffness, static_camber or combined_slip of v4"
                    .to_string(),
            ));
        }
        asset.format_version = TRAILER_FORMAT_VERSION;
        asset.validate()?;
        Ok(asset)
    }

    /// The asset as pretty RON text.
    ///
    /// # Errors
    /// [`TrailerAssetError::Parse`] when RON cannot write the value (never for a finite config).
    pub fn to_ron(&self) -> Result<String, TrailerAssetError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| TrailerAssetError::Parse(e.to_string()))
    }

    /// Checks the name and the configuration.
    ///
    /// # Errors
    /// [`TrailerAssetError::EmptyName`] or [`TrailerAssetError::Config`].
    pub fn validate(&self) -> Result<(), TrailerAssetError> {
        if self.name.trim().is_empty() {
            return Err(TrailerAssetError::EmptyName);
        }
        self.config.validate()?;
        Ok(())
    }

    /// The configuration to hand to `Trailer::spawn` or `spawn_trailer`.
    #[must_use]
    pub fn into_config(self) -> TrailerConfig {
        self.config
    }
}

/// `.trailer.ron` text at any readable version to text at the current version. A current file comes
/// back unchanged, comments and all; a v1 file (its chassis without the v4 vehicle tire fields,
/// which default) is rewritten at the current version.
///
/// # Errors
/// What [`TrailerAsset::from_ron`] refuses.
pub fn migrate_trailer(text: &str) -> Result<String, TrailerAssetError> {
    let found = regia_core::version::peek_ron_version(text).map_err(TrailerAssetError::Parse)?;
    if found == TRAILER_FORMAT_VERSION {
        TrailerAsset::from_ron(text)?;
        return Ok(text.to_string());
    }
    TrailerAsset::from_ron(text)?.to_ron()
}

/// Versions [`migrate_trailer`] accepts below the current one: v1, whose chassis section predates
/// the `.vehicle.ron` v4 tire fields.
#[must_use]
pub fn trailer_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Serde field-list fingerprint of `.trailer.ron` (for `cargo xtask check-formats`). It includes
/// the `.vehicle.ron` fingerprint: the chassis section is a vehicle configuration, so a vehicle
/// format change is a trailer format change too.
#[must_use]
pub fn trailer_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<TrailerAsset>(),
        serde_field_names::<TrailerConfig>(),
        serde_field_names::<CouplingConfig>(),
        serde_field_names::<StandConfig>(),
        serde_variant_names::<CouplingKind>(),
        serde_variant_names::<TrailerBrakes>(),
        vec![crate::vehicle_asset::vehicle_schema_fingerprint()],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_presets_round_trip() {
        for (name, config) in [
            ("car trailer", TrailerConfig::car_trailer()),
            ("semi", TrailerConfig::semi_trailer()),
            ("dolly", TrailerConfig::dolly()),
        ] {
            let asset = TrailerAsset::new(name, config);
            let text = asset.to_ron().expect("ron");
            assert_eq!(TrailerAsset::from_ron(&text), Ok(asset));
        }
    }

    #[test]
    fn missing_sections_take_the_car_trailer_defaults() {
        let asset = TrailerAsset::from_ron(
            r#"(format_version: 1, name: "box", config: (brakes: Surge(threshold: 300.0, full_force: 3000.0, response: 0.2)))"#,
        )
        .expect("parse");
        assert_eq!(asset.config.chassis, TrailerConfig::car_trailer().chassis);
        assert_eq!(asset.config.coupling, CouplingConfig::default());
        assert_eq!(asset.config.brakes.name(), "surge");
    }

    /// corr-low: `.vehicle.ron` refuses a v1-v3 file carrying the v4 tire transient fields; the
    /// trailer's v1 gate accepted them in the chassis section, silently loading values a v1 file
    /// predates (its chassis predates `.vehicle.ron` v4).
    #[test]
    fn a_v1_file_cannot_hold_the_v4_tire_transient_fields() {
        let with = |field: &str| {
            format!(
                r#"(format_version: 1, name: "t", config: (chassis: (chassis_mass: 750.0, wheels: [
                    (position: (-0.8, -0.2, -1.3), tire_model: Slip(({field}))),
                    (position: (0.8, -0.2, -1.3)),
                ])))"#
            )
        };
        // The same wheels without the transient fields load fine at v1.
        let plain = r#"(format_version: 1, name: "t", config: (chassis: (chassis_mass: 750.0, wheels: [
            (position: (-0.8, -0.2, -1.3), tire_model: Slip(())),
            (position: (0.8, -0.2, -1.3)),
        ])))"#;
        let plain_result = TrailerAsset::from_ron(plain);
        assert!(
            plain_result.is_ok(),
            "the plain v1 file must load: {:?}",
            plain_result.err()
        );
        for field in [
            "relaxation_length: 0.5",
            "pneumatic_trail: 0.03",
            "camber_stiffness: 4000.0",
            "static_camber: -0.02",
            "combined_slip: SlipWeighted(shape: 1.0, stiffness: 10.0)",
        ] {
            assert_eq!(
                TrailerAsset::from_ron(&with(field)).map_err(|e| e.code()),
                Err("trailer_asset_parse"),
                "{field}"
            );
        }
    }

    #[test]
    fn problems_have_stable_codes() {
        let code = |text: &str| TrailerAsset::from_ron(text).expect_err("rejected").code();
        assert_eq!(code("(("), "trailer_asset_parse");
        for version in [0, TRAILER_FORMAT_VERSION + 1] {
            assert_eq!(
                code(&format!(r#"(format_version: {version}, name: "x", config: ())"#)),
                "trailer_asset_version"
            );
        }
        assert_eq!(
            code(r#"(format_version: 1, name: " ", config: ())"#),
            "trailer_asset_name"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (parking_brake: 2.0))"#),
            "trailer_invalid_config"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (chassis: (wheels: [(drive: true)])))"#),
            "trailer_driven_wheel"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (chassis: (wheels: [])))"#),
            "vehicle_no_wheels"
        );
    }

    #[test]
    fn the_migrator_keeps_a_current_file_byte_for_byte() {
        let text = TrailerAsset::new("t", TrailerConfig::car_trailer())
            .to_ron()
            .expect("ron");
        let commented = format!("// hand tuned\n{text}");
        assert_eq!(migrate_trailer(&commented).expect("current"), commented);
        assert_eq!(trailer_migration_sources(), vec![1]);
        assert_eq!(
            migrate_trailer(r#"(format_version: 9, name: "x", config: ())"#).map_err(|e| e.code()),
            Err("trailer_asset_version")
        );
    }

    /// A v1 file (written before the `.vehicle.ron` v4 tire fields) loads, and migrates to the
    /// current version with the same configuration.
    #[test]
    fn a_v1_file_migrates_to_the_current_version() {
        let current = TrailerAsset::new("t", TrailerConfig::car_trailer())
            .to_ron()
            .expect("ron");
        let v1 = current.replacen(
            &format!("format_version: {TRAILER_FORMAT_VERSION}"),
            "format_version: 1",
            1,
        );
        assert_ne!(v1, current);
        let loaded = TrailerAsset::from_ron(&v1).expect("a v1 trailer loads");
        assert_eq!(loaded.format_version, TRAILER_FORMAT_VERSION);
        let migrated = migrate_trailer(&v1).expect("migrates");
        assert_eq!(
            regia_core::version::peek_ron_version(&migrated).expect("version"),
            TRAILER_FORMAT_VERSION
        );
        assert_eq!(TrailerAsset::from_ron(&migrated), Ok(loaded));
    }

    #[test]
    fn the_fingerprint_names_the_trailer_and_vehicle_fields() {
        let f = trailer_schema_fingerprint();
        for field in [
            "format_version",
            "coupling",
            "capture_radius",
            "jackknife_warning_angle",
            "parking_brake",
            "rear_hitch",
            "half_size",
            "TowBall",
            "FifthWheel",
            "Follow",
            "Surge",
            "chassis_mass",
            "tire_model",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
    }
}
