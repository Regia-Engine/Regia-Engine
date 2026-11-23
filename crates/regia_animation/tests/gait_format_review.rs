// SPDX-License-Identifier: MPL-2.0
//! Review proofs for `.gait.ron` v1 (head e2fc65919, docs/specs/12-assets.md § 4a).
//!
//! `explicit_pattern_rejects_an_unknown_field` fails on that head: `GaitPatternSpec`
//! has no `deny_unknown_fields`, so an extra field on `Explicit` is dropped. The other
//! tests lock the hostile inputs and the schema shape the loader already handles.

use regia_animation::gait_asset::{GaitAsset, gait_schema_fingerprint};

const CORPUS_V1: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/gait/v1.ron");

fn refused(text: &str) -> String {
    match GaitAsset::from_ron(text) {
        Ok(asset) => {
            let saved = asset
                .to_ron()
                .unwrap_or_else(|error| format!("<to_ron: {error}>"));
            panic!("loaded a file that must be refused\n{saved}")
        }
        Err(error) => {
            assert!(
                error.to_string().starts_with(error.code()),
                "the message must start with the code: {error}"
            );
            format!("{}: {error}", error.code())
        }
    }
}

fn assert_code(text: &str, code: &str) {
    let message = refused(text);
    assert!(message.starts_with(code), "{message}");
}

fn assert_code_names(text: &str, code: &str, name: &str) {
    let message = refused(text);
    assert!(message.starts_with(code), "{message}");
    assert!(message.contains(name), "{message}");
}

fn load(text: &str) -> GaitAsset {
    GaitAsset::from_ron(text).unwrap_or_else(|error| panic!("{error}"))
}

/// One save of a loaded file is a byte-identical fixed point, and the settings bits match.
fn assert_fixed_point(text: &str) -> GaitAsset {
    let asset = load(text);
    let saved = asset.to_ron().unwrap_or_else(|error| panic!("save: {error}"));
    let again = load(&saved);
    let resaved = again
        .to_ron()
        .unwrap_or_else(|error| panic!("second save: {error}"));
    assert_eq!(resaved, saved, "the canonical save is not a fixed point");
    let left = &again.params;
    let right = &asset.params;
    assert_eq!(format!("{left:?}"), format!("{right:?}"));
    assert_eq!(again.file, asset.file);
    asset
}

#[test]
fn explicit_pattern_rejects_an_unknown_field() {
    // Six offsets, so the pattern matches the corpus legs. `extra_phase` is not a field.
    let clean = CORPUS_V1.replacen(
        "patterns: [Tripod]",
        "patterns: [Explicit(offsets: [0.0, 0.5, 0.5, 0.0, 0.0, 0.5], duty: 0.5)]",
        1,
    );
    assert_ne!(clean, CORPUS_V1);
    let _ = load(&clean);
    let dirty = clean.replacen("duty: 0.5)", "duty: 0.5, extra_phase: 0.1)", 1);
    assert_ne!(dirty, clean);
    assert_code_names(&dirty, "gait_parse", "extra_phase");
}

#[test]
fn other_pattern_shapes_reject_extra_payload() {
    let tripod = CORPUS_V1.replacen("patterns: [Tripod]", "patterns: [Tripod(bogus: 1)]", 1);
    assert_code(&tripod, "gait_parse");
    let ripple = CORPUS_V1.replacen("patterns: [Tripod]", "patterns: [Ripple(6, 7)]", 1);
    assert_code(&ripple, "gait_parse");
}

#[test]
fn nan_is_a_named_error() {
    assert_code(
        &CORPUS_V1.replacen("step_length: 0.6", "step_length: NaN", 1),
        "gait_settings",
    );
    assert_code(
        &CORPUS_V1.replacen("step_length: 0.6", "step_length: -NaN", 1),
        "gait_settings",
    );
    assert_code(
        &CORPUS_V1.replacen("hip_offset: Some((-0.4, 0.5))", "hip_offset: Some((NaN, 0.5))", 1),
        "gait_settings",
    );
    let duty = CORPUS_V1.replacen(
        "patterns: [Tripod]",
        "patterns: [Explicit(offsets: [0.0, 0.5, 0.5, 0.0, 0.0, 0.5], duty: NaN)]",
        1,
    );
    assert_code(&duty, "gait_pattern");
}

#[test]
fn nonfinite_limits_the_stepper_allows_round_trip() {
    let reach = CORPUS_V1.replacen("max_reach: Some(5.0)", "max_reach: Some(inf)", 1);
    let asset = assert_fixed_point(&reach);
    assert!(asset.params.max_reach[0].is_infinite());
    let saved = asset.to_ron().unwrap_or_else(|error| panic!("save: {error}"));
    assert!(saved.contains("inf"), "{saved}");

    let band = CORPUS_V1.replacen("ground_band: Some(50.0)", "ground_band: Some(inf)", 1);
    let asset = assert_fixed_point(&band);
    assert!(asset.params.ground_band.is_infinite());

    let posture = CORPUS_V1.replacen(
        "    legs: [",
        "    posture: Some((swing_weight: 0.5, height_tau: 0.08, height_slew: Some(inf), \
         attitude_weight: 0.0, max_pitch: 0.0, max_roll: 0.0, attitude_tau: 0.0)),\n    legs: [",
        1,
    );
    let asset = assert_fixed_point(&posture);
    assert!(asset.params.posture.height_slew.is_infinite());
}

#[test]
fn oversize_counts_are_named_errors() {
    let five = CORPUS_V1.replacen(
        "patterns: [Tripod]",
        "patterns: [Tripod, Tripod, Tripod, Tripod, Tripod]",
        1,
    );
    assert_code(&five, "gait_pattern");

    let offsets = vec!["0.0"; 10_000].join(", ");
    let huge = CORPUS_V1.replacen(
        "patterns: [Tripod]",
        &format!("patterns: [Explicit(offsets: [{offsets}], duty: 0.5)]"),
        1,
    );
    assert_code(&huge, "gait_pattern");

    let leg = "(root: \"coxa_fl\", tip: \"foot_fl\", hip_offset: Some((-0.4, 0.5))),\n";
    let legs = leg.repeat(1_000);
    let many = CORPUS_V1.replacen(
        "(root: \"coxa_fl\", tip: \"foot_fl\", hip_offset: Some((-0.4, 0.5))),\n",
        &legs,
        1,
    );
    assert_code(&many, "gait_legs");
}

#[test]
fn deep_ron_nesting_is_gait_parse() {
    for depth in [200_usize, 4_096] {
        let mut text = String::from("(format_version: 1, patterns: ");
        text.extend(std::iter::repeat_n('[', depth));
        text.push('1');
        text.extend(std::iter::repeat_n(']', depth));
        text.push(')');
        assert_code(&text, "gait_parse");
    }
}

#[test]
fn newer_version_with_a_foreign_body_is_gait_version() {
    let text = "(\n    format_version: 2,\n    patterns: \"no\",\n)\n";
    let message = refused(text);
    assert!(message.starts_with("gait_version"), "{message}");
    assert!(message.contains('2'), "{message}");
}

#[test]
fn schema_fingerprint_includes_nested_enum_shapes() {
    let fingerprint = gait_schema_fingerprint();
    for needle in [
        "0:unit",
        "1:newtype",
        "2:newtype",
        "8:{offsets,duty}",
        "Tripod",
        "Explicit",
    ] {
        assert!(
            fingerprint.contains(needle),
            "{needle} missing from {fingerprint}"
        );
    }
}

#[test]
fn corpus_save_is_a_fixed_point() {
    let _ = assert_fixed_point(CORPUS_V1);
}
