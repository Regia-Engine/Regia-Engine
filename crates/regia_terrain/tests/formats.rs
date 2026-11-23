// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixtures for terrain: the checked-in `.terrain.layers.ron` and `.scatter.ron` v1
//! examples (written to `fixtures/` from `docs/formats/terrain.md`) must keep loading, and
//! load → save → load is equal (`docs/specs/12-assets.md` § 4a).

use regia_terrain::{
    ScatterRule, TerrainError, TerrainLayers, scatter_schema_fingerprint, terrain_layers_schema_fingerprint,
};

/// The checked-in `.terrain.layers.ron` v1 fixture still loads (and migrates to v2).
#[test]
fn terrain_layers_v1_fixture_still_loads() {
    let layers =
        TerrainLayers::from_ron(include_str!("fixtures/ground.terrain.layers.ron")).expect("v1 layers");
    assert_eq!(layers.version, regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION);
    assert_eq!(layers.layers.len(), 1);
    assert_eq!(layers.layers[0].name, "ground");
    // The v1 look survives: no textures, no micro normals, soft blending.
    assert!(layers.layers[0].albedo_texture.is_none());
    assert_eq!(layers.layers[0].detail_normal_strength, 0.0);
    assert_eq!(layers.layers[0].height_blend, 0.0);
}

/// `.terrain.layers.ron` round trips: load → save → load equal.
#[test]
fn terrain_layers_round_trips() {
    let layers = TerrainLayers::from_ron(include_str!("fixtures/ground.terrain.layers.ron")).expect("load");
    let text = layers.to_ron().expect("save");
    let back = TerrainLayers::from_ron(&text).expect("reload");
    assert_eq!(back, layers);
}

/// A newer `.terrain.layers.ron` version is rejected with a clear error.
#[test]
fn terrain_layers_rejects_newer_version() {
    // One past the current version (v5 since ADR-3367).
    let newer = include_str!("fixtures/ground.terrain.layers.ron").replace("version: 1", "version: 6");
    let err = TerrainLayers::from_ron(&newer).expect_err("v6 must be rejected");
    assert!(err.to_string().contains("version 6"), "{err}");
}

/// A newer layers version whose body no longer deserialises (the required `layers` field is
/// absent) is still a *version* error, not a RON "missing field" error.
#[test]
fn terrain_layers_rejects_newer_version_even_when_body_changed() {
    let err = TerrainLayers::from_ron("(version: 6, wetness_default: 0.0)").expect_err("v6 must be rejected");
    assert!(
        matches!(err, TerrainError::InvalidLayers(_)),
        "expected a layers error, got: {err}"
    );
    assert!(err.to_string().contains("version 6"), "{err}");
}

/// A v2 file with texture slots and blend knobs loads with them intact.
#[test]
fn terrain_layers_v2_texture_slots_load() {
    let text = r#"
TerrainLayers(
    version: 2,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "cliff",
            albedo: (0.4, 0.38, 0.36),
            roughness: 0.85,
            tiling: 1.2,
            slope_min: 0.5,
            slope_max: 1.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.12,
            triplanar: true,
            albedo_texture: Some("terrain/rock_albedo.png"),
            normal_texture: Some("terrain/rock_normal.png"),
            height_texture: Some("terrain/rock_height.png"),
            detail_normal_strength: 0.9,
            height_blend: 0.7,
        ),
    ],
)
"#;
    let layers = TerrainLayers::from_ron(text).expect("v2 layers");
    assert_eq!(layers.version, regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION);
    let layer = &layers.layers[0];
    assert!(layer.orm_texture.is_none(), "v2 migrates with no ORM path");
    assert_eq!(layer.albedo_texture.as_deref(), Some("terrain/rock_albedo.png"));
    assert_eq!(layer.normal_texture.as_deref(), Some("terrain/rock_normal.png"));
    assert_eq!(layer.height_texture.as_deref(), Some("terrain/rock_height.png"));
    assert_eq!(layer.detail_normal_strength, 0.9);
    assert_eq!(layer.height_blend, 0.7);
}

/// The checked-in `.scatter.ron` v1 fixture still loads.
#[test]
fn scatter_v1_fixture_still_loads() {
    let rule = ScatterRule::from_ron(include_str!("fixtures/rocks.scatter.ron"), None).expect("v1 scatter");
    assert_eq!(rule.version, 1);
    assert_eq!(rule.seed, 2747);
}

/// `.scatter.ron` round trips: load → save → load equal.
#[test]
fn scatter_round_trips() {
    let rule = ScatterRule::from_ron(include_str!("fixtures/rocks.scatter.ron"), None).expect("load");
    let text = rule.to_ron().expect("save");
    let back = ScatterRule::from_ron(&text, None).expect("reload");
    assert_eq!(back, rule);
}

/// A newer `.scatter.ron` version is rejected with a clear error.
#[test]
fn scatter_rejects_newer_version() {
    let newer = include_str!("fixtures/rocks.scatter.ron").replace("version: 1", "version: 2");
    let err = ScatterRule::from_ron(&newer, None).expect_err("v2 must be rejected");
    assert!(err.to_string().contains("version 2"), "{err}");
}

/// A newer scatter version whose body no longer deserialises (the required `seed`/`mesh` fields are
/// absent) is still a *version* error, not a RON "missing field" error.
#[test]
fn scatter_rejects_newer_version_even_when_body_changed() {
    let err = ScatterRule::from_ron("(version: 2)", None).expect_err("v2 must be rejected");
    assert!(
        matches!(err, TerrainError::InvalidScatter(_)),
        "expected a scatter error, got: {err}"
    );
    assert!(err.to_string().contains("version 2"), "{err}");
}

/// The terrain schema fingerprints hash *nested* field names (not just the root struct's).
#[test]
fn terrain_schema_fingerprints_hash_nested_fields() {
    let layers = terrain_layers_schema_fingerprint();
    assert!(layers.contains("layers"), "{layers}");
    // `albedo`/`roughness` live on the nested `LayerDesc`, not the root `TerrainLayers`.
    assert!(layers.contains("albedo"), "nested LayerDesc not hashed: {layers}");
    assert!(
        layers.contains("roughness"),
        "nested LayerDesc not hashed: {layers}"
    );
    // Scatter v2 nests classes, mesh refs and exclusions under the root set: every level hashed.
    let scatter = scatter_schema_fingerprint();
    assert!(scatter.contains("seed"), "{scatter}");
    assert!(
        scatter.contains("density_map"),
        "nested ScatterClass not hashed: {scatter}"
    );
    assert!(
        scatter.contains("scale_bias"),
        "nested ScatterMeshRef not hashed: {scatter}"
    );
    assert!(
        scatter.contains("points"),
        "nested ScatterExclusion not hashed: {scatter}"
    );
}

/// The v1 fixture loads as a one-class v2 set (the frozen-format migrator, specs/12 § 4a).
#[test]
fn scatter_v1_fixture_migrates_to_v2() {
    let set = regia_terrain::ScatterSet::from_ron(include_str!("fixtures/rocks.scatter.ron"), None)
        .expect("v1 file loads as v2");
    assert_eq!(set.version, regia_terrain::SCATTER_SET_VERSION);
    assert_eq!(set.seed, 2747);
    assert_eq!(set.classes.len(), 1);
    assert_eq!(set.classes[0].meshes[0].mesh, "models/rock_small.gltf");
    assert_eq!(set.classes[0].density, 0.7);
    let text = set.to_ron().expect("save v2");
    assert_eq!(
        regia_terrain::ScatterSet::from_ron(&text, None).expect("reload"),
        set
    );
}
