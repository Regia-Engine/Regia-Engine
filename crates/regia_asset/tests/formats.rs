// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixtures: the checked-in v1 examples from `docs/formats/` must keep loading, and
//! load → save → load must be equal (`docs/specs/12-assets.md` § 4a).

use regia_asset::{MaterialAssetRon, MetaSidecar, material_schema_fingerprint, meta_schema_fingerprint};

/// The `.meta` v1 fixture (the v1 `docs/formats/meta.example.ron`, copied verbatim) still loads, migrated
/// to the current version (v2, ADR-7819: the switch `lossless_clips` off; this file never named it).
#[test]
fn meta_v1_fixture_still_loads() {
    let meta = MetaSidecar::from_ron(include_bytes!("fixtures/meta.example.ron")).expect("v1 meta");
    assert_eq!(meta.format_version, regia_asset::META_FORMAT_VERSION);
    assert!(!meta.lossless_clips());
    assert_eq!(meta.importer, "gltf");
    assert_eq!(meta.sub_assets.len(), 1);
    // The bare-optional spelling of the published example is preserved in the parsed value.
    assert_eq!(
        meta.settings.as_deref(),
        Some("(forward: Convert, generate_tangents: true)")
    );
    let provenance = meta.provenance.as_ref().expect("provenance");
    assert_eq!(provenance.origin.as_deref(), Some("Khronos DamagedHelmet"));
}

/// `.meta` round trips: load → save → load equal.
#[test]
fn meta_round_trips() {
    let meta = MetaSidecar::from_ron(include_bytes!("fixtures/meta.example.ron")).expect("load");
    let text = meta.to_ron_string().expect("save");
    let back = MetaSidecar::from_ron(text.as_bytes()).expect("reload");
    assert_eq!(back, meta);
}

/// A newer `.meta` version is rejected with a clear error.
#[test]
fn meta_rejects_newer_version() {
    let newer = String::from_utf8_lossy(include_bytes!("fixtures/meta.example.ron"))
        .replace("format_version: 1", "format_version: 3");
    let err = MetaSidecar::from_ron(newer.as_bytes()).expect_err("v3 must be rejected");
    assert!(err.to_string().contains("version 3"), "{err}");
}

/// A newer `.meta` version whose body no longer matches the v1 struct is still a *version* error,
/// not a RON error (the version is peeked before the full deserialise).
#[test]
fn meta_rejects_newer_version_even_when_body_changed() {
    let newer = "(format_version: 3, id: \"67e55044-10b1-426f-9247-bb680e5fe0c8\", importer: \"gltf\", future_field: 1)";
    let err = MetaSidecar::from_ron(newer.as_bytes()).expect_err("v3 must be rejected");
    assert!(err.to_string().contains("version 3"), "{err}");
    assert!(!err.to_string().contains("future_field"), "{err}");
}

/// The `.material.ron` v1 fixture (copied verbatim from `docs/formats/material.example.ron`) loads.
#[test]
fn material_v1_fixture_still_loads() {
    let mat =
        MaterialAssetRon::from_ron(include_bytes!("fixtures/material.example.ron")).expect("v1 material");
    assert_eq!(
        mat.format_version,
        regia_asset::MATERIAL_FORMAT_VERSION,
        "v1 migrates to the current format"
    );
    assert_eq!(mat.layers.len(), 2);
    // The published example's bare optionals are populated.
    assert_eq!(mat.name.as_deref(), Some("LayeredPaint"));
    assert!(mat.base.base_color_map.is_some());
    assert!(mat.clearcoat.is_some());
    let hard = mat
        .hard_surface
        .expect("v1 migrator inserts hard-surface defaults");
    assert!(hard.multi_scatter && hard.specular_occlusion);
    assert!(hard.detail.is_none() && hard.parallax.is_none() && hard.edge_wear.is_none());
}

/// `.material.ron` round trips: load → save → load equal.
#[test]
fn material_round_trips() {
    let mat = MaterialAssetRon::from_ron(include_bytes!("fixtures/material.example.ron")).expect("load");
    let text = mat.to_ron_string().expect("save");
    let back = MaterialAssetRon::from_ron(text.as_bytes()).expect("reload");
    assert_eq!(back, mat);
}

/// A newer `.material.ron` version is rejected with a clear error.
#[test]
fn material_rejects_newer_version() {
    let next = regia_asset::MATERIAL_FORMAT_VERSION + 1;
    let newer = String::from_utf8_lossy(include_bytes!("fixtures/material.example.ron"))
        .replace("format_version: 1", &format!("format_version: {next}"));
    let err = MaterialAssetRon::from_ron(newer.as_bytes()).expect_err("a newer version must be rejected");
    assert!(err.to_string().contains(&format!("version {next}")), "{err}");
}

/// A newer `.material.ron` version whose body no longer matches the v1 struct is still a *version*
/// error, not a RON error.
#[test]
fn material_rejects_newer_version_even_when_body_changed() {
    let next = regia_asset::MATERIAL_FORMAT_VERSION + 1;
    let newer = format!(
        "(format_version: {next}, shader: Standard, base: (base_color: (0.0, 0.0, 0.0, 1.0)), future_field: true)"
    );
    let err = MaterialAssetRon::from_ron(newer.as_bytes()).expect_err("a newer version must be rejected");
    assert!(err.to_string().contains(&format!("version {next}")), "{err}");
    assert!(!err.to_string().contains("future_field"), "{err}");
}

/// A registered older→current step rewrites a fixture field through the *loader* (`from_ron`),
/// not a side-channel `version::migrate` on a value parsed beside it. Regression: the deleted
/// v1 → v1 identity step used to shadow an appended step and send `migrate` into the 32-step bail;
/// and round-1's `from_ron` had no way to register a step, so a v0 fixture failed outright.
#[test]
fn a_registered_step_rewrites_an_older_material_fixture() {
    let older = String::from_utf8_lossy(include_bytes!("fixtures/material.example.ron"))
        .replace("format_version: 1", "format_version: 0");
    let steps = [
        regia_core::version::VersionStep {
            from: 0,
            to: 1,
            // Rewrite a field *other* than the version number: the loader advances `format_version`
            // itself; the step proves the registered transform is applied to the content.
            migrate: |m: &mut MaterialAssetRon| {
                m.name = Some("migrated-name".to_string());
                Ok(())
            },
        },
        regia_core::version::VersionStep {
            from: 1,
            to: 2,
            migrate: regia_asset::migrate_material_v1_to_v2,
        },
        regia_core::version::VersionStep {
            from: 2,
            to: 3,
            migrate: regia_asset::migrate_material_v2_to_v3,
        },
        regia_core::version::VersionStep {
            from: 3,
            to: 4,
            migrate: regia_asset::migrate_material_v3_to_v4,
        },
        regia_core::version::VersionStep {
            from: 4,
            to: 5,
            migrate: regia_asset::migrate_material_v4_to_v5,
        },
        regia_core::version::VersionStep {
            from: 5,
            to: 6,
            migrate: regia_asset::migrate_material_v5_to_v6,
        },
        regia_core::version::VersionStep {
            from: 6,
            to: 7,
            migrate: regia_asset::migrate_material_v6_to_v7,
        },
    ];
    let mat = MaterialAssetRon::from_ron_with_steps(older.as_bytes(), &steps).expect("migrate v0 -> current");
    assert_eq!(mat.format_version, regia_asset::MATERIAL_FORMAT_VERSION);
    assert_eq!(mat.name.as_deref(), Some("migrated-name"));
    assert!(
        mat.hard_surface.is_some(),
        "the v1 -> v2 step inserts hard-surface defaults"
    );
    assert!(mat.validate().is_ok(), "the migrated material validates");
}

/// The checked-in v4 example (`docs/formats/material-v4.example.ron`) loads with its thin film, and
/// the block round trips.
#[test]
fn material_v4_fixture_carries_the_film() {
    let bytes = include_bytes!("fixtures/material-v4.example.ron");
    let mat = MaterialAssetRon::from_ron(bytes).expect("v4 material");
    assert_eq!(
        mat.format_version,
        regia_asset::MATERIAL_FORMAT_VERSION,
        "a v4 file migrates on load"
    );
    assert_eq!(mat.weather, None, "the default weather response");
    assert_eq!(mat.subsurface, None, "a v4 file has no scattering");
    let film = mat.thin_film.as_ref().expect("thin film");
    assert_eq!(film.factor, 0.9);
    assert_eq!(film.ior, 1.56);
    assert_eq!((film.thickness_min, film.thickness_max), (180.0, 520.0));
    assert!(film.factor_map.is_some() && film.thickness_map.is_some());
    let text = mat.to_ron_string().expect("ron");
    assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("reload"), mat);
    assert!(
        material_schema_fingerprint().contains("thickness_max"),
        "the film block is hashed"
    );
}

/// The checked-in v5 example (`docs/formats/material-v5.example.ron`) loads with its weather
/// response, and the block round trips (spec 99 § 7.3).
#[test]
fn material_v5_fixture_carries_the_weather() {
    let bytes = include_bytes!("fixtures/material-v5.example.ron");
    let mat = MaterialAssetRon::from_ron(bytes).expect("v5 material");
    assert_eq!(
        mat.format_version,
        regia_asset::MATERIAL_FORMAT_VERSION,
        "migrated on load"
    );
    assert_eq!(mat.subsurface, None, "a v5 file has no scattering");
    let weather = mat.weather.expect("weather");
    assert_eq!(weather.porosity, Some(0.6));
    assert_eq!((weather.wet, weather.snow, weather.puddles), (1.0, 0.8, true));
    let text = mat.to_ron_string().expect("ron");
    assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("reload"), mat);
    assert!(
        material_schema_fingerprint().contains("puddles"),
        "the weather block is hashed"
    );
}

/// The checked-in v6 example (`docs/formats/material-v6.example.ron`) loads with its subsurface
/// block, and the block round trips.
#[test]
fn material_v6_fixture_carries_the_subsurface_block() {
    let bytes = include_bytes!("fixtures/material-v6.example.ron");
    let mat = MaterialAssetRon::from_ron(bytes).expect("v6 material");
    assert_eq!(mat.format_version, regia_asset::MATERIAL_FORMAT_VERSION);
    let block = mat.subsurface.as_ref().expect("subsurface");
    assert_eq!(block.profile, regia_asset::SubsurfaceProfileKind::Skin);
    assert_eq!(block.radius, 6.0);
    assert_eq!(block.tint, [1.0, 0.95, 0.9]);
    let text = mat.to_ron_string().expect("ron");
    assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("reload"), mat);
    let fingerprint = material_schema_fingerprint();
    assert!(fingerprint.contains("tint"), "the block is hashed");
    assert!(fingerprint.contains("foliage"), "the profile names are hashed");
}

/// The checked-in v7 example (`docs/formats/material-v7.example.ron`) loads with its roughness, and
/// the field round trips.
#[test]
fn material_v7_fixture_carries_the_transmission_roughness() {
    let bytes = include_bytes!("fixtures/material-v7.example.ron");
    let mat = MaterialAssetRon::from_ron(bytes).expect("v7 material");
    assert_eq!(mat.format_version, regia_asset::MATERIAL_FORMAT_VERSION);
    assert_eq!(mat.transmission_roughness, Some(0.35));
    assert_eq!(mat.transmission.as_ref().expect("transmission").factor, 0.9);
    let text = mat.to_ron_string().expect("ron");
    assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("reload"), mat);
    assert!(
        material_schema_fingerprint().contains("transmission_roughness"),
        "the field is hashed"
    );
}

/// audit-v035-formats 13: a v7 file stamped with an older `format_version` loaded with its
/// newer blocks silently cleared by the migration steps (and `upgrade-project` wrote the loss
/// back). A mislabelled file is now an error naming the block.
#[test]
fn an_older_stamped_material_with_newer_blocks_is_refused() {
    let text = std::str::from_utf8(include_bytes!("fixtures/material-v7.example.ron")).expect("utf8");
    let v7 = format!("format_version: {}", regia_asset::MATERIAL_FORMAT_VERSION);
    assert!(text.contains(&v7), "the fixture is stamped v7");
    for older in [3, 6] {
        let relabelled = text.replace(&v7, &format!("format_version: {older}"));
        let err = MaterialAssetRon::from_ron(relabelled.as_bytes())
            .expect_err("a mislabelled material does not load with its blocks cleared");
        assert!(err.to_string().contains("block"), "v{older}: {err}");
    }
    // The genuine older fixtures still load.
    for (name, bytes) in [
        ("v4", &include_bytes!("fixtures/material-v4.example.ron")[..]),
        ("v6", &include_bytes!("fixtures/material-v6.example.ron")[..]),
    ] {
        MaterialAssetRon::from_ron(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// The material fingerprint hashes *nested* field names, so adding a field to
/// `StandardMaterialParams` (the book's `emissive_strength` example) changes the hash.
#[test]
fn material_schema_fingerprint_hashes_nested_fields() {
    let fp = material_schema_fingerprint();
    // `base_color_map` lives on `StandardMaterialParams`, not the root `MaterialAssetRon`.
    assert!(fp.contains("base_color_map"), "nested params not hashed: {fp}");
    // `mask` lives on `MaterialLayer`.
    assert!(fp.contains("mask"), "nested layer not hashed: {fp}");
    // `factor` lives on `ClearcoatParams`.
    assert!(fp.contains("factor"), "nested clearcoat not hashed: {fp}");
    // v2 hard-surface block and the edge-wear source spellings.
    assert!(fp.contains("hard_surface"), "hard surface not hashed: {fp}");
    assert!(
        fp.contains("multi_scatter"),
        "hard surface fields not hashed: {fp}"
    );
    // v3 glass fields.
    for field in [
        "emissive_strength",
        "orm_packed",
        "ior",
        "transmission",
        "attenuation_distance",
        "alpha_cutoff",
    ] {
        assert!(fp.contains(field), "{field} missing from the fingerprint: {fp}");
    }
    for spelling in ["curvature", "ao"] {
        assert!(
            fp.split(';').any(|list| list.split(',').any(|n| n == spelling)),
            "{spelling} missing from the fingerprint: {fp}"
        );
    }
}

/// The checked-in v2 fixture (`docs/formats/material-v2.example.ron`) loads as v3 with the v3
/// defaults: emissive strength 1, packed ORM, IOR 1.5, no transmission, alpha cutoff 0.5. Its v2
/// content is unchanged.
#[test]
fn material_v2_fixture_migrates_to_v3_defaults() {
    let mat =
        MaterialAssetRon::from_ron(include_bytes!("fixtures/material-v2.example.ron")).expect("v2 material");
    assert_eq!(mat.format_version, regia_asset::MATERIAL_FORMAT_VERSION);
    assert_eq!(mat.name.as_deref(), Some("HullPaint"));
    assert!((mat.base.roughness - 0.55).abs() < 1e-6);
    assert!((mat.base.emissive_strength - 1.0).abs() < 1e-6);
    assert!(mat.base.orm_packed);
    assert!((mat.base.ior - 1.5).abs() < 1e-6);
    assert!(mat.transmission.is_none());
    assert!((mat.render_state.alpha_cutoff - 0.5).abs() < 1e-6);
    assert!(mat.hard_surface.as_ref().is_some_and(|h| h.detail.is_some()));
}

/// A v3 material with glass fields round-trips through `.material.ron` and the cooked payload.
#[test]
fn material_v3_glass_round_trips_ron_and_cooked() {
    let text = r#"(
        format_version: 3,
        name: "M_Canopy",
        shader: Standard,
        base: (
            base_color: (1.0, 1.0, 1.0, 1.0),
            roughness: 0.15,
            emissive: (0.0, 0.0, 0.0),
            emissive_strength: 1.0,
            ior: 1.52,
        ),
        transmission: (
            factor: 0.92,
            thickness: 0.012,
            attenuation_color: (0.75, 0.88, 0.84),
            attenuation_distance: 0.08,
        ),
        render_state: (alpha_mode: opaque, cull_mode: none, queue_offset: 0, alpha_cutoff: 0.5),
    )"#;
    let mat = MaterialAssetRon::from_ron(text.as_bytes()).expect("v3 glass");
    let t = mat.transmission.as_ref().expect("transmission");
    assert!((t.factor - 0.92).abs() < 1e-6);
    assert!((mat.base.ior - 1.52).abs() < 1e-6);
    assert_eq!(mat.render_state.cull_mode, Some(regia_asset::CullMode::None));
    let back = MaterialAssetRon::from_ron(mat.to_ron_string().expect("save").as_bytes()).expect("reload");
    assert_eq!(back, mat);
    let cooked = regia_asset::CookedStandardMaterial::new(mat.clone());
    let bytes = cooked.to_postcard().expect("postcard");
    let loaded = regia_asset::CookedStandardMaterial::from_postcard(&bytes).expect("cooked");
    assert_eq!(loaded.authored, mat);
}

/// The meta fingerprint hashes nested field names too.
#[test]
fn meta_schema_fingerprint_hashes_nested_fields() {
    let fp = meta_schema_fingerprint();
    assert!(fp.contains("type_path"), "nested sub-asset not hashed: {fp}");
    assert!(fp.contains("origin"), "nested provenance not hashed: {fp}");
}

/// The checked-in minimal v1 `.fpak` fixture still loads.
#[test]
fn pak_v1_fixture_still_loads() {
    let bytes = include_bytes!("fixtures/empty.fpak").to_vec();
    let len = bytes.len() as u64;
    let reader = regia_asset::PakReader::from_bytes(bytes, "empty.fpak", len).expect("v1 pak");
    assert_eq!(reader.header.flags, 0);
    assert!(reader.toc.entries.is_empty());
    assert!(reader.toc.manifest.is_empty());
}
