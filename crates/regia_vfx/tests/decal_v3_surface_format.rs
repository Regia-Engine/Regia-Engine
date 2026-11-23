// SPDX-License-Identifier: MPL-2.0
//! ADR-8829: the decal v3 `surface` field (format batch 2 fold into the unreleased v3 step).
//!
//! Uses only `DecalPrefab::from_ron` / `to_ron` (no field access), so on a build without the
//! field it fails at run time (`unknown field surface`), not at compile time.

use regia_vfx::{DECAL_FORMAT_VERSION, DecalPrefab};

const CORPUS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../regia_agent/tests/fixtures/format-corpus/decal"
);

#[test]
fn v3_surface_field_parses_round_trips_and_validates() {
    assert_eq!(DECAL_FORMAT_VERSION, 3, "the surface field rides the v3 step");
    let text = std::fs::read_to_string(format!("{CORPUS}/v3-surface.ron")).expect("v3-surface fixture");
    let prefab = DecalPrefab::from_ron(&text).expect("a v3 prefab with a surface loads");
    let written = prefab.to_ron().expect("write");
    assert!(written.contains("surface: Some(\"puddle\")"), "{written}");
    let again = DecalPrefab::from_ron(&prefab.to_ron().expect("write")).expect("round trip");
    assert_eq!(again, prefab);

    let too_long = "a".repeat(33);
    for bad in ["", "Puddle", "wet sand", "mud-1", "../x", too_long.as_str()] {
        let ron = format!("(version: 3, name: \"bad\", half_size: (1.0, 1.0, 0.2), surface: Some({bad:?}))");
        assert!(
            DecalPrefab::from_ron(&ron).is_err(),
            "surface {bad:?} must be refused"
        );
    }
    let longest = "a".repeat(32);
    let ron = format!("(version: 3, name: \"ok\", half_size: (1.0, 1.0, 0.2), surface: Some({longest:?}))");
    DecalPrefab::from_ron(&ron).expect("a 32-byte surface name is allowed");
}

#[test]
fn prefabs_without_a_surface_keep_their_bytes() {
    // The frozen v3 fixture has no `surface`: it loads, has none, and writing it emits no
    // `surface` key, so every v3 file written before the fold keeps its bytes on rewrite.
    let text = std::fs::read_to_string(format!("{CORPUS}/v3.ron")).expect("v3 fixture");
    let prefab = DecalPrefab::from_ron(&text).expect("the frozen v3 fixture loads");
    assert!(!prefab.to_ron().expect("write").contains("surface"));
    // v1 and v2 migrate to v3 with no surface.
    for v in [1, 2] {
        let text = std::fs::read_to_string(format!("{CORPUS}/v{v}.ron")).expect("old fixture");
        let prefab = DecalPrefab::from_ron(&text).expect("old fixture migrates");
        assert_eq!(prefab.version, 3);
        assert!(
            !prefab.to_ron().expect("write").contains("surface"),
            "v{v} gains no surface"
        );
    }
}
