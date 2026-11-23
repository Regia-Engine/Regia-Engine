// SPDX-License-Identifier: MPL-2.0
//! The cook writes the asset registry (`docs/specs/content-data.md` § 2): one record per manifest
//! entry with dependencies from the cooked headers and tags from `config/asset_tags.ron`; the live
//! cook updates it incrementally; packing writes one next to each pak.

use std::collections::BTreeSet;
use std::path::Path;

use image::ImageEncoder;
use regia_asset::registry::{
    AssetRegistry, RegistryQuery, TagFilter, cache_registry_path, pak_registry_path,
};
use regia_asset::{AssetManifest, cooked_cache_dir};
use regia_asset_pipeline::registry_build::{RegistryBuildReport, read_cooked_header, write_cache_registry};
use regia_asset_pipeline::{AssetDatabase, CookOptions, cook_project, cook_sources, host_platform};

/// A 64×64 RGBA PNG (large enough to block-compress) whose pixels depend on `seed`.
fn png(seed: u8) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&[((x * 4) as u8) ^ seed, (y * 4) as u8, seed, 255]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

/// A textured quad: `crate.gltf` with its buffer `crate.bin` and image `crate_albedo.png` beside
/// it (both referenced by URI), plus a standalone `decal.png`.
fn write_project(root: &Path) {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    let positions: [[f32; 3]; 4] = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ];
    let uvs: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let indices: [u16; 6] = [0, 2, 1, 0, 3, 2];
    let mut bin = Vec::new();
    for p in positions {
        p.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for _ in 0..4 {
        [0.0f32, 1.0, 0.0]
            .iter()
            .for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for uv in uvs {
        uv.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    indices
        .iter()
        .for_each(|i| bin.extend_from_slice(&i.to_le_bytes()));
    assert_eq!(bin.len(), 140);
    let json = r#"{
  "asset": {"version": "2.0"},
  "scene": 0,
  "scenes": [{"nodes": [0]}],
  "nodes": [{"mesh": 0, "name": "Crate"}],
  "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0, "mode": 4}]}],
  "materials": [{"name": "crate", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
  "textures": [{"source": 0}],
  "images": [{"uri": "crate_albedo.png"}],
  "buffers": [{"byteLength": 140, "uri": "crate.bin"}],
  "bufferViews": [
    {"buffer": 0, "byteOffset": 0, "byteLength": 48, "target": 34962},
    {"buffer": 0, "byteOffset": 48, "byteLength": 48, "target": 34962},
    {"buffer": 0, "byteOffset": 96, "byteLength": 32, "target": 34962},
    {"buffer": 0, "byteOffset": 128, "byteLength": 12, "target": 34963}
  ],
  "accessors": [
    {"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-1, 0, -1], "max": [1, 0, 1]},
    {"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3"},
    {"bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2"},
    {"bufferView": 3, "componentType": 5123, "count": 6, "type": "SCALAR"}
  ]
}"#;
    std::fs::write(assets.join("crate.gltf"), json).expect("gltf");
    std::fs::write(assets.join("crate.bin"), bin).expect("bin");
    std::fs::write(assets.join("crate_albedo.png"), png(1)).expect("albedo");
    std::fs::write(assets.join("decal.png"), png(2)).expect("decal");
}

fn cache(root: &Path) -> std::path::PathBuf {
    cooked_cache_dir(root, host_platform())
}

fn cook(root: &Path) {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    assert!(
        report.warnings.iter().all(|w| !w.contains("registry")),
        "{:?}",
        report.warnings
    );
}

fn registry(root: &Path) -> AssetRegistry {
    AssetRegistry::read_from(&cache_registry_path(&cache(root)))
        .expect("decode")
        .expect("the cook wrote a registry")
}

const RULES: &str = r#"(
    format_version: 1,
    rules: [
        (paths: ["assets/**"], tags: { "project": "crates" }),
        (paths: ["assets/crate.gltf"], tags: { "kind": "prop" }),
    ],
)"#;

#[test]
fn cook_writes_a_registry_matching_the_manifest() {
    let root = regia_test_utils::unique_temp_dir("registry-cook");
    write_project(&root);
    std::fs::create_dir_all(root.join("config")).expect("config");
    std::fs::write(root.join("config/asset_tags.ron"), RULES).expect("rules");
    cook(&root);

    let reg = registry(&root);
    let manifest = AssetManifest::read_from_cache(&cache(&root))
        .expect("manifest")
        .expect("some");
    let manifest_paths: BTreeSet<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    let registry_paths: BTreeSet<&str> = reg.entries().map(|e| e.path.as_str()).collect();
    assert_eq!(registry_paths, manifest_paths, "one record per manifest entry");

    // Dependencies come from the cooked headers.
    for entry in &manifest.entries {
        let header =
            read_cooked_header(&cache(&root).join(regia_asset::cooked_file_name(entry.id))).expect("header");
        let mut want: Vec<_> = header
            .dependencies
            .into_iter()
            .filter(|d| *d != entry.id)
            .collect();
        want.sort();
        want.dedup();
        assert_eq!(
            reg.by_id(entry.id).expect("record").dependencies,
            want,
            "{}",
            entry.path
        );
    }
    let material = reg
        .entries()
        .find(|e| e.type_path.ends_with("CookedStandardMaterial"))
        .expect("the glTF material");
    assert!(
        !material.dependencies.is_empty(),
        "the material names its albedo texture"
    );
    for dep in &material.dependencies {
        let refs = reg.referencers(*dep, false);
        assert!(refs.entries.iter().any(|e| e.id == material.id), "reverse index");
    }

    // Tags: automatic, importer from the database, authored rules (sub-assets inherit).
    let decal = reg.by_path("assets/decal.png").expect("decal");
    assert_eq!(decal.tags.get("ext").map(String::as_str), Some("png"));
    assert_eq!(decal.tags.get("project").map(String::as_str), Some("crates"));
    assert!(decal.tags.contains_key("importer"), "{:?}", decal.tags);
    assert!(decal.size > 0);
    let props = reg.query(&RegistryQuery {
        tags: vec![TagFilter::parse("kind=prop").expect("filter")],
        ..Default::default()
    });
    assert!(
        props.total >= 2,
        "the glTF and its sub-assets: {:?}",
        props.entries
    );
    assert!(
        props
            .entries
            .iter()
            .all(|e| e.path.starts_with("assets/crate.gltf"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unchanged_cook_leaves_the_registry_file_alone() {
    let root = regia_test_utils::unique_temp_dir("registry-unchanged");
    write_project(&root);
    cook(&root);
    let path = cache_registry_path(&cache(&root));
    let before = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("mtime");
    let db = AssetDatabase::open(&root).expect("db");
    let again: RegistryBuildReport = write_cache_registry(&root, &cache(&root), &db).expect("rebuild");
    assert_eq!(again.records, registry(&root).len());
    let after = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("mtime");
    assert_eq!(before, after, "same bytes, no rewrite");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn live_cook_updates_the_registry_like_a_full_rebuild() {
    let root = regia_test_utils::unique_temp_dir("registry-live");
    write_project(&root);
    cook(&root);
    // A new source and a tag rule that now applies to the decal.
    std::fs::write(root.join("assets/sticker.png"), png(9)).expect("sticker");
    std::fs::create_dir_all(root.join("config")).expect("config");
    std::fs::write(
        root.join("config/asset_tags.ron"),
        r#"(format_version: 1, rules: [(paths: ["assets/*.png"], tags: { "flat": "yes" })])"#,
    )
    .expect("rules");
    std::fs::write(root.join("assets/decal.png"), png(3)).expect("edit decal");
    let targets: BTreeSet<_> = [root.join("assets/sticker.png"), root.join("assets/decal.png")]
        .into_iter()
        .collect();
    let options = CookOptions::for_host(root.clone());
    let cooked = cook_sources(&options, &targets).expect("live cook");
    assert!(cooked.iter().all(|c| c.error.is_none()), "{cooked:?}");

    let live = registry(&root);
    assert!(
        live.by_path("assets/sticker.png").is_some(),
        "new source recorded"
    );
    assert_eq!(
        live.by_path("assets/decal.png")
            .and_then(|e| e.tags.get("flat"))
            .map(String::as_str),
        Some("yes")
    );
    assert!(
        live.by_path("assets/crate_albedo.png")
            .is_none_or(|e| !e.tags.contains_key("flat")),
        "untouched sources keep their records until they are cooked again"
    );
    // Same records as a full rebuild for every source the live cook touched.
    let db = AssetDatabase::open(&root).expect("db");
    write_cache_registry(&root, &cache(&root), &db).expect("full rebuild");
    let full = registry(&root);
    for path in ["assets/sticker.png", "assets/decal.png"] {
        assert_eq!(live.by_path(path), full.by_path(path), "{path}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn packing_writes_a_registry_next_to_the_pak() {
    let root = regia_test_utils::unique_temp_dir("registry-pak");
    write_project(&root);
    cook(&root);
    let out = root.join("build/paks");
    std::fs::create_dir_all(&out).expect("out");
    let paks = regia_asset_pipeline::pack_cooked_directory(&cache(&root), &out, &Default::default(), false)
        .expect("pack");
    assert_eq!(paks.len(), 1);
    let sibling = pak_registry_path(&paks[0]);
    assert_eq!(
        sibling.file_name().and_then(|n| n.to_str()),
        Some("base.registry.bin")
    );
    let pak_registry = AssetRegistry::read_from(&sibling)
        .expect("decode")
        .expect("written");
    assert_eq!(pak_registry, registry(&root), "one pak holds every cooked asset");
    let _ = std::fs::remove_dir_all(&root);
}
