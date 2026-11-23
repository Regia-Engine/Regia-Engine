// SPDX-License-Identifier: MPL-2.0
//! `.vehicle.ron`: a vehicle configuration as a content asset (v4, ADR-2301, ADR-2598, ADR-2719,
//! ADR-3276).
//!
//! The file holds a [`VehicleConfig`] under a `format_version` and a display `name`, so a car
//! is tuned in data and spawned with [`VehicleAsset::into_config`] (or the agent method
//! `physics.vehicle_config_check`, which validates one without spawning). See
//! `docs/formats/vehicle.md`.
//!
//! v2 adds the `drivetrain`, `anti_roll`, `aero` and `aids` sections. A v1 file loads as the
//! `Simple` drivetrain with none of the others, which reproduces `max_engine_force` bit for bit;
//! [`migrate_vehicle`] writes it back as the current version.
//!
//! v3 adds the wheel field `tire_model` (ADR-2719). A v1 or v2 file loads with every wheel's tire
//! as `Damper`, rapier's model, so it drives bit for bit as before.
//!
//! v4 adds the slip tire's transient fields `relaxation_length`, `pneumatic_trail`,
//! `camber_stiffness`, `static_camber` and `combined_slip` (ADR-3276). Each defaults to off, so a
//! v1 to v3 file loads with none of them and drives bit for bit as before.

use serde::{Deserialize, Serialize};

use crate::vehicle::{VehicleConfig, VehicleError, WheelConfig};
use crate::vehicle_drivetrain::{
    AbsConfig, AeroConfig, AidsConfig, AntiRollBar, ClutchConfig, Differential, DriveLayout, Drivetrain,
    DrivetrainConfig, EngineConfig, GearboxConfig, TireConfig, TractionControlConfig,
};
use crate::vehicle_tire::{CombinedSlip, LateralCurve, SlipTire, TireModel};

/// Current `.vehicle.ron` format version.
pub const VEHICLE_FORMAT_VERSION: u32 = 4;

/// Why a `.vehicle.ron` did not load.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VehicleAssetError {
    /// The text is not valid RON for the format.
    #[error("vehicle asset does not parse: {0}")]
    Parse(String),
    /// The file's `format_version` is neither the current one nor one that migrates.
    #[error("vehicle asset format version {found}; this build reads {expected}")]
    Version {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// The name is empty.
    #[error("vehicle asset has an empty name")]
    EmptyName,
    /// The configuration failed [`VehicleConfig::validate`].
    #[error("vehicle config: {0}")]
    Config(#[from] VehicleError),
}

impl VehicleAssetError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs). A config
    /// problem reports the [`VehicleError`] code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "vehicle_asset_parse",
            Self::Version { .. } => "vehicle_asset_version",
            Self::EmptyName => "vehicle_asset_name",
            Self::Config(e) => e.code(),
        }
    }
}

/// A `.vehicle.ron`: a named [`VehicleConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VehicleAsset {
    /// [`VEHICLE_FORMAT_VERSION`] once loaded; a file may hold an older version that migrates
    /// ([`vehicle_migration_sources`]).
    pub format_version: u32,
    /// Display name (not empty).
    pub name: String,
    /// The vehicle: chassis, drive limits, ray layers, wheels, drivetrain, anti-roll, aero and aids.
    pub config: VehicleConfig,
}

impl VehicleAsset {
    /// An asset of the current version.
    #[must_use]
    pub fn new(name: impl Into<String>, config: VehicleConfig) -> Self {
        Self {
            format_version: VEHICLE_FORMAT_VERSION,
            name: name.into(),
            config,
        }
    }

    /// Parses and validates a `.vehicle.ron` text; a v1 or v2 file migrates on the way in, so the
    /// asset's `format_version` is always the current one.
    ///
    /// # Errors
    /// [`VehicleAssetError`]: a parse failure, a version this build cannot read, a v1 file with a v2
    /// section, a v1 or v2 file with a v3 `tire_model`, a v1 to v3 file with a v4 tire transient
    /// field, an empty name, or a configuration
    /// [`VehicleConfig::validate`] rejects.
    pub fn from_ron(text: &str) -> Result<Self, VehicleAssetError> {
        let found = regia_core::version::peek_ron_version(text).map_err(VehicleAssetError::Parse)?;
        if found != VEHICLE_FORMAT_VERSION && !vehicle_migration_sources().contains(&found) {
            return Err(VehicleAssetError::Version {
                found,
                expected: VEHICLE_FORMAT_VERSION,
            });
        }
        let mut asset: Self = ron::from_str(text).map_err(|e| VehicleAssetError::Parse(e.to_string()))?;
        if found == 1 {
            let config = &asset.config;
            if config.drivetrain != Drivetrain::Simple
                || !config.anti_roll.is_empty()
                || config.aero.is_some()
                || config.aids != AidsConfig::default()
            {
                return Err(VehicleAssetError::Parse(
                    "a v1 file cannot hold the drivetrain, anti_roll, aero or aids sections of v2"
                        .to_string(),
                ));
            }
        }
        if found < 3
            && asset
                .config
                .wheels
                .iter()
                .any(|wheel| wheel.tire_model != TireModel::Damper)
        {
            return Err(VehicleAssetError::Parse(format!(
                "a v{found} file cannot hold the wheel field tire_model of v3"
            )));
        }
        if found < 4
            && asset
                .config
                .wheels
                .iter()
                .any(|wheel| wheel.tire_model.slip().is_some_and(SlipTire::has_transients))
        {
            return Err(VehicleAssetError::Parse(format!(
                "a v{found} file cannot hold the tire fields relaxation_length, pneumatic_trail, \
                 camber_stiffness, static_camber or combined_slip of v4"
            )));
        }
        asset.format_version = VEHICLE_FORMAT_VERSION;
        asset.validate()?;
        Ok(asset)
    }

    /// The asset as pretty RON text.
    ///
    /// # Errors
    /// [`VehicleAssetError::Parse`] when RON cannot write the value (never for a finite config).
    pub fn to_ron(&self) -> Result<String, VehicleAssetError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| VehicleAssetError::Parse(e.to_string()))
    }

    /// Checks the name and the configuration.
    ///
    /// # Errors
    /// [`VehicleAssetError::EmptyName`] or [`VehicleAssetError::Config`].
    pub fn validate(&self) -> Result<(), VehicleAssetError> {
        if self.name.trim().is_empty() {
            return Err(VehicleAssetError::EmptyName);
        }
        self.config.validate()?;
        Ok(())
    }

    /// The configuration to hand to `Vehicle::spawn` or `spawn_vehicle`.
    #[must_use]
    pub fn into_config(self) -> VehicleConfig {
        self.config
    }
}

/// `.vehicle.ron` text at any readable version to text at the current version. v1 becomes the
/// `Simple` drivetrain with no anti-roll, aero or aids; v1 and v2 wheels get the `Damper` tire;
/// v1 to v3 slip tires get no transients; every older field keeps its value. A file
/// already at the current version comes back unchanged, comments and all.
///
/// # Errors
/// What [`VehicleAsset::from_ron`] refuses.
pub fn migrate_vehicle(text: &str) -> Result<String, VehicleAssetError> {
    let found = regia_core::version::peek_ron_version(text).map_err(VehicleAssetError::Parse)?;
    if found == VEHICLE_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    VehicleAsset::from_ron(text)?.to_ron()
}

/// Versions [`migrate_vehicle`] accepts below the current one.
#[must_use]
pub fn vehicle_migration_sources() -> Vec<u32> {
    vec![1, 2, 3]
}

/// Serde field-list fingerprint of `.vehicle.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn vehicle_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<VehicleAsset>(),
        serde_field_names::<VehicleConfig>(),
        serde_field_names::<WheelConfig>(),
        serde_field_names::<DrivetrainConfig>(),
        serde_field_names::<EngineConfig>(),
        serde_field_names::<GearboxConfig>(),
        serde_field_names::<ClutchConfig>(),
        serde_field_names::<TireConfig>(),
        serde_field_names::<AntiRollBar>(),
        serde_field_names::<AeroConfig>(),
        serde_field_names::<AidsConfig>(),
        serde_field_names::<AbsConfig>(),
        serde_field_names::<TractionControlConfig>(),
        serde_field_names::<SlipTire>(),
        serde_variant_names::<Drivetrain>(),
        serde_variant_names::<TireModel>(),
        serde_variant_names::<LateralCurve>(),
        serde_variant_names::<CombinedSlip>(),
        serde_variant_names::<Differential>(),
        serde_variant_names::<DriveLayout>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_car_round_trips() {
        let asset = VehicleAsset::new("car", VehicleConfig::default());
        let text = asset.to_ron().expect("ron");
        assert_eq!(VehicleAsset::from_ron(&text), Ok(asset));
    }

    #[test]
    fn missing_fields_take_the_defaults() {
        let asset = VehicleAsset::from_ron(
            r#"(format_version: 1, name: "tuned", config: (chassis_mass: 900.0, max_engine_force: 5000.0))"#,
        )
        .expect("parse");
        assert_eq!(asset.config.chassis_mass, 900.0);
        assert_eq!(asset.config.wheels, VehicleConfig::default().wheels);
        assert_eq!(asset.config.ground_layers, None);
    }

    #[test]
    fn problems_have_stable_codes() {
        let code = |text: &str| VehicleAsset::from_ron(text).expect_err("rejected").code();
        assert_eq!(code("(("), "vehicle_asset_parse");
        assert_eq!(
            code(r#"(format_version: 5, name: "x", config: ())"#),
            "vehicle_asset_version"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: " ", config: ())"#),
            "vehicle_asset_name"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (chassis_mass: -1.0))"#),
            "vehicle_invalid_config"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", config: (wheels: []))"#),
            "vehicle_no_wheels"
        );
    }

    #[test]
    fn the_fingerprint_names_the_asset_config_and_wheel_fields() {
        let f = vehicle_schema_fingerprint();
        for field in [
            "format_version",
            "config",
            "chassis_mass",
            "ground_layers",
            "suspension_stiffness",
            "drivetrain",
            "anti_roll",
            "aero",
            "aids",
            "torque_curve",
            "forward_ratios",
            "peak_friction",
            "LimitedSlip",
            "Geared",
            "tire_model",
            "cornering_stiffness",
            "load_exponent",
            "roll_influence",
            "Slip",
            "MagicFormula",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
    }

    #[test]
    fn a_v1_file_loads_as_the_simple_drivetrain_and_keeps_its_values() {
        let text =
            r#"(format_version: 1, name: "tuned", config: (chassis_mass: 900.0, max_engine_force: 5000.25))"#;
        let asset = VehicleAsset::from_ron(text).expect("v1 loads");
        assert_eq!(asset.format_version, VEHICLE_FORMAT_VERSION);
        assert_eq!(asset.config.drivetrain, Drivetrain::Simple);
        assert_eq!(asset.config.max_engine_force, 5000.25);
        assert!(asset.config.anti_roll.is_empty());
        assert_eq!(asset.config.aero, None);
        assert_eq!(asset.config.aids, AidsConfig::default());
        assert_eq!(asset.config.wheels, VehicleConfig::default().wheels);
    }

    #[test]
    fn the_migrator_writes_v2_exactly_once() {
        let text = r#"(format_version: 1, name: "tuned", config: (max_engine_force: 5000.25, max_steer_angle: 0.123456))"#;
        let migrated = migrate_vehicle(text).expect("migrates");
        assert_eq!(
            regia_core::version::peek_ron_version(&migrated).expect("version"),
            VEHICLE_FORMAT_VERSION
        );
        let asset = VehicleAsset::from_ron(&migrated).expect("v2 loads");
        assert_eq!(Ok(asset.clone()), VehicleAsset::from_ron(text));
        assert_eq!(asset.config.max_steer_angle.to_bits(), 0.123456_f32.to_bits());
        // Already current: byte for byte, comments included.
        let commented = format!("// tuned by hand\n{migrated}");
        assert_eq!(migrate_vehicle(&commented).expect("current"), commented);
        assert_eq!(vehicle_migration_sources(), vec![1, 2, 3]);
        assert_eq!(
            migrate_vehicle(r#"(format_version: 9, name: "x", config: ())"#).map_err(|e| e.code()),
            Err("vehicle_asset_version")
        );
    }

    #[test]
    fn a_v1_file_with_a_v2_section_is_refused() {
        for section in [
            "drivetrain: Geared(())",
            "anti_roll: [(left: 0, right: 1, stiffness: 1.0)]",
            "aero: Some(())",
        ] {
            let text = format!(r#"(format_version: 1, name: "x", config: ({section}))"#);
            assert_eq!(
                VehicleAsset::from_ron(&text).map_err(|e| e.code()),
                Err("vehicle_asset_parse"),
                "{section}"
            );
        }
    }

    #[test]
    fn v1_and_v2_files_load_with_damper_tires_and_a_v3_field_is_refused_in_them() {
        for version in [1, 2] {
            let text = format!(r#"(format_version: {version}, name: "x", config: (chassis_mass: 900.0))"#);
            let asset = VehicleAsset::from_ron(&text).expect("loads");
            assert_eq!(asset.format_version, VEHICLE_FORMAT_VERSION);
            assert!(
                asset
                    .config
                    .wheels
                    .iter()
                    .all(|w| w.tire_model == TireModel::Damper)
            );
            assert!(!asset.config.has_slip_tires());
            let migrated = migrate_vehicle(&text).expect("migrates");
            assert_eq!(
                regia_core::version::peek_ron_version(&migrated).expect("version"),
                VEHICLE_FORMAT_VERSION
            );
            assert_eq!(VehicleAsset::from_ron(&migrated), Ok(asset));
            let with_slip = format!(
                r#"(format_version: {version}, name: "x", config: (wheels: [(tire_model: Slip(()))]))"#
            );
            assert_eq!(
                VehicleAsset::from_ron(&with_slip).map_err(|e| e.code()),
                Err("vehicle_asset_parse"),
                "v{version}"
            );
        }
    }

    #[test]
    fn v3_slip_tires_parse_round_trip_and_name_bad_fields() {
        let text = r#"(
            format_version: 3,
            name: "slip car",
            config: (
                wheels: [
                    (position: (-0.8, -0.2, -1.3), steer: true,
                     tire_model: Slip((cornering_stiffness: 50000.0, load_exponent: 0.15))),
                    (position: (0.8, -0.2, -1.3), steer: true,
                     tire_model: Slip((curve: MagicFormula(shape: 1.3, curvature: -0.4)))),
                    (position: (-0.8, -0.2, 1.3), drive: true),
                    (position: (0.8, -0.2, 1.3), drive: true, tire_model: Damper),
                ],
            ),
        )"#;
        let asset = VehicleAsset::from_ron(text).expect("v3 loads");
        let slip = asset.config.wheels[0].tire_model.slip().expect("slip");
        assert_eq!(slip.cornering_stiffness, 50000.0);
        assert_eq!(slip.load_exponent, 0.15);
        assert_eq!(slip.roll_influence, SlipTire::default().roll_influence);
        assert_eq!(
            asset.config.wheels[1].tire_model.slip().map(|t| t.curve),
            Some(LateralCurve::MagicFormula {
                shape: 1.3,
                curvature: -0.4
            })
        );
        assert_eq!(asset.config.wheels[2].tire_model, TireModel::Damper);
        let again = VehicleAsset::from_ron(&asset.to_ron().expect("ron")).expect("round trip");
        assert_eq!(again, asset);
        let bad = VehicleAsset::from_ron(
            r#"(format_version: 3, name: "x", config: (wheels: [(tire_model: Slip((load_exponent: 1.5)))]))"#,
        );
        assert_eq!(
            bad,
            Err(VehicleAssetError::Config(VehicleError::InvalidWheel {
                index: 0,
                field: "tire_model.load_exponent"
            }))
        );
    }

    #[test]
    fn v2_sections_parse_and_round_trip() {
        let text = r#"(
            format_version: 2,
            name: "lab car",
            config: (
                drivetrain: Geared((
                    layout: Awd,
                    front_torque_share: 0.35,
                    rear_differential: LimitedSlip(torque_bias: 2.5),
                    centre_differential: Locked,
                    gearbox: (forward_ratios: [3.0, 2.0, 1.0], upshift_rpm: 6000.0),
                )),
                anti_roll: [(left: 0, right: 1, stiffness: 12000.0)],
                aero: Some((drag_coefficient: 0.3, rear_downforce: 0.4)),
                aids: (abs: Some(()), traction_control: Some((slip_threshold: 0.2))),
            ),
        )"#;
        let asset = VehicleAsset::from_ron(text).expect("v2 loads");
        let Drivetrain::Geared(drivetrain) = &asset.config.drivetrain else {
            panic!("geared");
        };
        assert_eq!(drivetrain.layout, DriveLayout::Awd);
        assert_eq!(
            drivetrain.rear_differential,
            Differential::LimitedSlip { torque_bias: 2.5 }
        );
        assert_eq!(drivetrain.gearbox.forward_ratios.len(), 3);
        assert_eq!(asset.config.anti_roll[0].stiffness, 12000.0);
        assert_eq!(asset.config.aero.as_ref().map(|a| a.rear_downforce), Some(0.4));
        assert!(asset.config.aids.abs.is_some());
        let again = VehicleAsset::from_ron(&asset.to_ron().expect("ron")).expect("round trip");
        assert_eq!(again, asset);
    }

    #[test]
    fn bad_v2_sections_name_their_field() {
        let field = |section: &str| {
            let text = format!(r#"(format_version: 2, name: "x", config: ({section}))"#);
            match VehicleAsset::from_ron(&text) {
                Err(VehicleAssetError::Config(VehicleError::InvalidConfig { field })) => field,
                other => panic!("{section}: {other:?}"),
            }
        };
        assert_eq!(
            field("drivetrain: Geared((wheel_inertia: -1.0))"),
            "drivetrain.wheel_inertia"
        );
        assert_eq!(
            field("anti_roll: [(left: 0, right: 0, stiffness: 1.0)]"),
            "anti_roll"
        );
        assert_eq!(field("aero: Some((frontal_area: -1.0))"), "aero.frontal_area");
        // The aids read wheel spin, which only the geared drivetrain has.
        assert_eq!(field("aids: (abs: Some(()))"), "aids");
        assert_eq!(
            field("drivetrain: Geared(()), aids: (abs: Some((slip_threshold: 2.0)))"),
            "aids.abs.slip_threshold"
        );
    }

    #[test]
    fn the_fingerprint_names_the_v4_tire_fields() {
        let f = vehicle_schema_fingerprint();
        for field in [
            "relaxation_length",
            "pneumatic_trail",
            "camber_stiffness",
            "static_camber",
            "combined_slip",
            "SlipWeighted",
            "Ellipse",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
    }

    #[test]
    fn a_v3_file_loads_without_transients_and_a_v4_field_is_refused_in_it() {
        let v3 = r#"(format_version: 3, name: "slip car", config: (wheels: [
            (position: (-0.8, -0.2, -1.3), steer: true, tire_model: Slip((cornering_stiffness: 50000.0))),
            (position: (0.8, -0.2, -1.3), steer: true, tire_model: Slip(())),
        ]))"#;
        let asset = VehicleAsset::from_ron(v3).expect("v3 loads");
        assert_eq!(asset.format_version, VEHICLE_FORMAT_VERSION);
        for wheel in &asset.config.wheels {
            let tire = wheel.tire_model.slip().expect("slip");
            assert!(!tire.has_transients());
        }
        let migrated = migrate_vehicle(v3).expect("migrates");
        assert_eq!(
            regia_core::version::peek_ron_version(&migrated).expect("version"),
            VEHICLE_FORMAT_VERSION
        );
        assert_eq!(VehicleAsset::from_ron(&migrated), Ok(asset));
        for field in [
            "relaxation_length: 0.5",
            "pneumatic_trail: 0.03",
            "camber_stiffness: 4000.0",
            "static_camber: -0.02",
            "combined_slip: SlipWeighted(shape: 1.0, stiffness: 10.0)",
        ] {
            let text = format!(
                r#"(format_version: 3, name: "x", config: (wheels: [(tire_model: Slip(({field})))]))"#
            );
            assert_eq!(
                VehicleAsset::from_ron(&text).map_err(|e| e.code()),
                Err("vehicle_asset_parse"),
                "{field}"
            );
        }
    }

    #[test]
    fn v4_transients_parse_round_trip_and_name_bad_fields() {
        let text = r#"(
            format_version: 4,
            name: "transient car",
            config: (
                wheels: [
                    (position: (-0.8, -0.2, -1.3), steer: true,
                     tire_model: Slip((relaxation_length: 0.5, pneumatic_trail: 0.035,
                                       camber_stiffness: 4500.0, static_camber: -0.02))),
                    (position: (0.8, -0.2, -1.3), steer: true,
                     tire_model: Slip((combined_slip: SlipWeighted(shape: 0.9, stiffness: 12.0)))),
                ],
            ),
        )"#;
        let asset = VehicleAsset::from_ron(text).expect("v4 loads");
        let tire = asset.config.wheels[0].tire_model.slip().expect("slip");
        assert_eq!(tire.relaxation_length, 0.5);
        assert_eq!(tire.pneumatic_trail, 0.035);
        assert_eq!(tire.camber_stiffness, 4500.0);
        assert_eq!(tire.static_camber, -0.02);
        assert_eq!(tire.combined_slip, CombinedSlip::Ellipse);
        assert_eq!(
            asset.config.wheels[1].tire_model.slip().map(|t| t.combined_slip),
            Some(CombinedSlip::SlipWeighted {
                shape: 0.9,
                stiffness: 12.0
            })
        );
        let again = VehicleAsset::from_ron(&asset.to_ron().expect("ron")).expect("round trip");
        assert_eq!(again, asset);
        let bad = VehicleAsset::from_ron(
            r#"(format_version: 4, name: "x", config: (wheels: [(tire_model: Slip((relaxation_length: 20.0)))]))"#,
        );
        assert_eq!(
            bad,
            Err(VehicleAssetError::Config(VehicleError::InvalidWheel {
                index: 0,
                field: "tire_model.relaxation_length"
            }))
        );
    }
}
