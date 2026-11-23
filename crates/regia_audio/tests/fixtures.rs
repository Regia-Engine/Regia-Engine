// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixtures for audio: the checked-in `.cue.ron` and `.audiopatch.ron` v1 examples
//! (written to `fixtures/` from `docs/formats/audio-*-v1.md`) must keep loading, and load → save →
//! load is equal (`docs/specs/12-assets.md` § 4a).

use regia_audio::{AudioPatchDef, SoundCueDef, cue_schema_fingerprint, patch_schema_fingerprint};

/// The checked-in `.cue.ron` v1 fixture still loads (the frozen v1 cue format has no version field).
#[test]
fn cue_v1_fixture_still_loads() {
    let def = SoundCueDef::from_ron(include_str!("fixtures/impact_flesh.cue.ron")).expect("v1 cue");
    assert_eq!(def.sounds.len(), 3);
    assert_eq!(def.name, "impact_flesh");
}

/// `.cue.ron` round trips: load → save → load equal.
#[test]
fn cue_round_trips() {
    let def = SoundCueDef::from_ron(include_str!("fixtures/impact_flesh.cue.ron")).expect("load");
    let text = ron::to_string(&def).expect("save");
    let back = SoundCueDef::from_ron(&text).expect("reload");
    assert_eq!(back, def);
}

/// The checked-in `.audiopatch.ron` v1 fixture still loads (migrated to v2, ADR-7226).
#[test]
fn patch_v1_fixture_still_loads() {
    let def =
        AudioPatchDef::from_ron(include_str!("fixtures/hover_engine.audiopatch.ron")).expect("v1 patch");
    assert_eq!(def.version, regia_audio::PATCH_VERSION);
    assert_eq!(def.layers[0].keys.len(), 3);
}

/// `.audiopatch.ron` round trips: load → save → load equal.
#[test]
fn patch_round_trips() {
    let def = AudioPatchDef::from_ron(include_str!("fixtures/hover_engine.audiopatch.ron")).expect("load");
    let text = ron::to_string(&def).expect("save");
    let back = AudioPatchDef::from_ron(&text).expect("reload");
    assert_eq!(back, def);
}

/// A newer `.audiopatch.ron` version is rejected with a clear error.
#[test]
fn patch_rejects_newer_version() {
    // Version 2 is current since ADR-7226; the first unknown version is 3.
    let newer = include_str!("fixtures/hover_engine.audiopatch.ron").replace("version: 1", "version: 3");
    let err = AudioPatchDef::from_ron(&newer).expect_err("v3 must be rejected");
    assert!(err.to_string().contains("version 3"), "{err}");
}

/// The audio schema fingerprints hash *nested* field names (not just the root struct's).
#[test]
fn audio_schema_fingerprints_hash_nested_fields() {
    let cue = cue_schema_fingerprint();
    assert!(cue.contains("sounds"), "{cue}");
    // `sound`/`weight` live on the nested `CueSoundDef`.
    assert!(cue.contains("sound"), "nested CueSoundDef not hashed: {cue}");
    assert!(cue.contains("weight"), "nested CueSoundDef not hashed: {cue}");

    let patch = patch_schema_fingerprint();
    assert!(patch.contains("version"), "{patch}");
    // `smoothing_secs` lives on the nested `PatchParamDef`.
    assert!(
        patch.contains("smoothing_secs"),
        "nested PatchParamDef not hashed: {patch}"
    );
}

/// The `.loopgen.ron` v1 example still loads, round trips and has a stable-shaped fingerprint.
#[test]
fn loopgen_v1_example_loads_and_round_trips() {
    use regia_audio::{LoopGenDef, loopgen_schema_fingerprint};
    let text = include_str!("../../../docs/formats/loopgen.example.ron");
    let def = LoopGenDef::from_ron(text).expect("v1 loopgen");
    assert_eq!(def.version, 1);
    let back = LoopGenDef::from_ron(&ron::to_string(&def).expect("save")).expect("reload");
    assert_eq!(back, def);
    assert!(loopgen_schema_fingerprint().contains("crossfade_secs"));
}
