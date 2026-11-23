// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixture for `.vfx.ron`: the checked-in muzzle-flash example from
//! `docs/formats/vfx-ron.md` (written to `fixtures/`) must keep loading, and load → save → load is
//! equal (`docs/specs/12-assets.md` § 4a).

use regia_vfx::{VfxFormatError, VfxSystemDef, vfx_schema_fingerprint};

/// The checked-in `.vfx.ron` v1 fixture still loads, and the loader migrates it to the current version.
#[test]
fn vfx_v1_fixture_still_loads() {
    let def = VfxSystemDef::from_ron(include_str!("fixtures/muzzle_flash.vfx.ron")).expect("v1 vfx");
    assert_eq!(
        def.version,
        regia_vfx::VFX_FORMAT_VERSION,
        "v1 migrates to the current format"
    );
    assert_eq!(def.emitters.len(), 2);
    assert_eq!(def.name, "muzzle_flash");
    assert!((def.emitters[0].render.emissive - 1.0).abs() < 1e-5);
    assert!(!def.emitters[0].render.lit);
}

/// `.vfx.ron` round trips: load → save → load equal.
#[test]
fn vfx_round_trips() {
    let def = VfxSystemDef::from_ron(include_str!("fixtures/muzzle_flash.vfx.ron")).expect("load");
    let text = def.to_ron().expect("save");
    let back = VfxSystemDef::from_ron(&text).expect("reload");
    assert_eq!(back, def);
}

/// A newer `.vfx.ron` version is rejected with a clear error.
#[test]
fn vfx_rejects_newer_version() {
    let next = regia_vfx::VFX_FORMAT_VERSION + 1;
    let newer =
        include_str!("fixtures/muzzle_flash.vfx.ron").replace("version: 1", &format!("version: {next}"));
    let err = VfxSystemDef::from_ron(&newer).expect_err("a newer version must be rejected");
    assert!(err.to_string().contains(&format!("version {next}")), "{err}");
}

/// A newer `.vfx.ron` version whose body no longer deserialises (`future_field` trips the format's
/// `deny_unknown_fields`) is still a *version* error, not a RON parse error: the version is peeked
/// before the full deserialise. A deserialise-first loader would report the unknown field instead.
#[test]
fn vfx_rejects_newer_version_even_when_body_changed() {
    let next = regia_vfx::VFX_FORMAT_VERSION + 1;
    let text = format!(r#"(version: {next}, name: "x", emitters: [], future_field: true)"#);
    let err = VfxSystemDef::from_ron(&text).expect_err("a newer version must be rejected");
    assert!(
        matches!(err, VfxFormatError::Version { found, .. } if found == next),
        "expected a version error, got: {err}"
    );
}

/// The vfx schema fingerprint hashes *nested* field names (not just the root struct's).
#[test]
fn vfx_schema_fingerprint_hashes_nested_fields() {
    let fp = vfx_schema_fingerprint();
    assert!(fp.contains("version"), "{fp}");
    assert!(fp.contains("emitters"), "{fp}");
    // `budget` lives on the nested `SystemModule`; `lifetime` on `SpawnModule`.
    assert!(fp.contains("budget"), "nested SystemModule not hashed: {fp}");
    assert!(fp.contains("lifetime"), "nested SpawnModule not hashed: {fp}");
}
