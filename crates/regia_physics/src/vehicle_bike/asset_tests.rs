// SPDX-License-Identifier: MPL-2.0
//! `.bike.ron` v1 load, write and refusal tests (ADR-8630).

use super::*;
use crate::vehicle_bike::BikeConfig;

/// The frozen corpus file (`format-corpus/bike/v1.ron`), through its crate-local copy
/// (`tests/bike_fixture_copy.rs` keeps it byte-identical): written by [`BikeAsset::to_ron`].
const CORPUS_V1: &str = include_str!("../../tests/fixtures/formats/bike/v1.ron");

#[test]
fn bike_v1_round_trips_canonical_text() {
    let asset = BikeAsset::from_ron(CORPUS_V1).expect("corpus v1 loads");
    assert_eq!(asset.format_version, BIKE_FORMAT_VERSION);
    assert_eq!(asset.name, "city");
    assert_eq!(asset.config.wheelbase, 1.4);
    assert_eq!(asset.notes.as_deref(), Some("a light commuter: quick to lean"));
    let text = asset.to_ron().expect("write");
    assert_eq!(text, CORPUS_V1, "the corpus file is the canonical text");
    assert!(!text.contains('\r'), "LF only");
    assert_eq!(BikeAsset::from_ron(&text).expect("reload"), asset);
    assert_eq!(migrate_bike(CORPUS_V1).expect("migrate"), CORPUS_V1);
    let default = BikeAsset::default();
    assert_eq!(
        BikeAsset::from_ron(&default.to_ron().expect("write")).expect("read"),
        default
    );
}

#[test]
fn missing_bike_fields_take_the_default_config() {
    let asset = BikeAsset::from_ron("(format_version: 1, name: \"bare\")").expect("bare file");
    assert_eq!(asset.config, BikeConfig::default());
    assert_eq!(asset.notes, None);
    let asset = BikeAsset::from_ron("(format_version: 1, name: \"long\", config: (wheelbase: 2.0))")
        .expect("sparse file");
    assert_eq!(
        asset.config,
        BikeConfig {
            wheelbase: 2.0,
            ..BikeConfig::default()
        }
    );
    // A bare string is a note too (implicit_some), and an explicit `Some` reads the same.
    let a = BikeAsset::from_ron("(format_version: 1, name: \"n\", notes: \"hi\")").expect("bare note");
    let b = BikeAsset::from_ron("(format_version: 1, name: \"n\", notes: Some(\"hi\"))").expect("some note");
    assert_eq!(a, b);
}

#[test]
fn a_bad_bike_field_names_it_on_load() {
    let error = BikeAsset::from_ron("(format_version: 1, name: \"x\", config: (max_lean: 9.0))")
        .expect_err("max_lean out of range");
    assert_eq!(error.code(), "bike_bad_input");
    assert_eq!(error.field_name(), Some("max_lean"));
    assert!(error.to_string().contains("max_lean"), "{error}");
    // A misspelt name is refused and named, not ignored.
    let error = BikeAsset::from_ron("(format_version: 1, name: \"x\", config: (wheelbse: 1.0))")
        .expect_err("unknown field");
    assert_eq!(error.code(), "bike_parse");
    assert!(error.to_string().contains("wheelbse"), "{error}");
    let error =
        BikeAsset::from_ron("(format_version: 1, name: \"x\", colour: 1)").expect_err("unknown top field");
    assert_eq!(error.code(), "bike_parse");
    assert!(error.to_string().contains("colour"), "{error}");
    // A positional tuple is not a bike.
    assert_eq!(
        BikeAsset::from_ron("(format_version: 1, name: \"x\", config: (1.5, 9.81))").map_err(|e| e.code()),
        Err("bike_parse")
    );
    let error = BikeAsset::from_ron("(format_version: 1, name: \"  \")").expect_err("blank name");
    assert_eq!(error.code(), "bike_bad_name");
    assert_eq!(error.field_name(), Some("name"));
    // The same codes from a value built in code.
    let mut asset = BikeAsset::default();
    asset.config.contact_reach = f32::NAN;
    let error = asset.validate().expect_err("NaN reach");
    assert_eq!(error.code(), "bike_bad_input");
    assert_eq!(error.field_name(), Some("contact_reach"));
}

#[test]
fn a_newer_bike_file_names_its_version() {
    // A v2 body this build cannot read: the version is named before the body is parsed.
    let text = "(format_version: 2, name: \"x\", config: (frame: \"carbon\", wheels: [(radius: 0.3)]))";
    let error = BikeAsset::from_ron(text).expect_err("v2");
    assert_eq!(error.code(), "bike_version");
    assert_eq!(
        error,
        BikeAssetError::Version {
            found: 2,
            expected: 1
        }
    );
    assert!(error.to_string().contains('2'), "{error}");
    assert_eq!(migrate_bike(text).map_err(|e| e.code()), Err("bike_version"));
    assert_eq!(BikeAsset::from_ron("((").map_err(|e| e.code()), Err("bike_parse"));
    assert_eq!(
        BikeAsset::from_ron("(name: \"no version\")").map_err(|e| e.code()),
        Err("bike_parse")
    );
}

#[test]
fn the_version_table_and_fingerprint() {
    assert_eq!(BIKE_VERSION_TABLE, &[(1, 1)]);
    assert!(bike_migration_sources().is_empty());
    let f = bike_schema_fingerprint();
    for name in [
        "format_version",
        "notes",
        "wheelbase",
        "contact_reach",
        "lean_response",
    ] {
        assert!(f.contains(name), "{name} missing from {f}");
    }
}
