// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixtures for `.level.ron` / `.prefab.ron`: the checked-in v1 examples from
//! `docs/formats/` (copied into `fixtures/`) must keep loading, and load → save → load is equal
//! (`docs/specs/12-assets.md` § 4a).

use regia_scene::format::{SCENE_FORMAT_VERSION, SceneFile, scene_schema_fingerprint};

/// The checked-in `.level.ron` and `.prefab.ron` v1 fixtures still load, migrated to the
/// current version (v2 added the level atmosphere).
#[test]
fn scene_v1_fixtures_still_load() {
    let level = SceneFile::from_ron_str(include_str!("fixtures/level.example.ron")).expect("level");
    assert_eq!(level.format_version, SCENE_FORMAT_VERSION);
    assert!(!level.entities.is_empty());

    let prefab = SceneFile::from_ron_str(include_str!("fixtures/prefab.example.ron")).expect("prefab");
    assert_eq!(prefab.format_version, SCENE_FORMAT_VERSION);
    assert_eq!(prefab.entities.len(), 4, "one root plus three children");
}

/// Scene files round trip: load → save → load equal, and the canonical form is stable.
#[test]
fn scene_round_trips() {
    for text in [
        include_str!("fixtures/level.example.ron"),
        include_str!("fixtures/prefab.example.ron"),
    ] {
        let parsed = SceneFile::from_ron_str(text).expect("load");
        let rewritten = parsed.to_ron_string().expect("save");
        let reparsed = SceneFile::from_ron_str(&rewritten).expect("reload");
        assert_eq!(reparsed, parsed);
        assert_eq!(
            reparsed.to_ron_string().expect("re-save"),
            rewritten,
            "canonical form is stable"
        );
    }
}

/// The scene schema fingerprint hashes *nested* field names (not just the root struct's).
#[test]
fn scene_schema_fingerprint_hashes_nested_fields() {
    let fp = scene_schema_fingerprint();
    assert!(fp.contains("format_version"), "{fp}");
    assert!(fp.contains("entities"), "{fp}");
    // `components` lives on the nested `SceneEntity`; `overrides` on `PrefabInstanceData`.
    assert!(fp.contains("components"), "nested SceneEntity not hashed: {fp}");
    assert!(
        fp.contains("overrides"),
        "nested PrefabInstanceData not hashed: {fp}"
    );
    // v2: the level settings and their atmosphere block are part of the frozen shape.
    assert!(
        fp.contains("atmosphere") && fp.contains("sky_body"),
        "LevelSettings not hashed: {fp}"
    );
    assert!(
        fp.contains("Airless"),
        "AtmospherePreset variants not hashed: {fp}"
    );
}
