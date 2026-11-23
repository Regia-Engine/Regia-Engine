// SPDX-License-Identifier: MPL-2.0
//! Incremental cook without the import (ADR-530): an unchanged glTF or PNG is skipped without
//! running its importer, any edit to a file the importer reads cooks it again, and unchanged old
//! files are not even read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use image::ImageEncoder;
use regia_asset::{AssetManifest, MetaSidecar, cooked_cache_dir, cooked_file_name, meta_path_for};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};

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

fn options(root: &Path) -> CookOptions {
    CookOptions::for_host(root.to_path_buf())
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(options(root)).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    report
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

/// Every cooked file but the manifest (rewritten by every cook): name → (bytes, modified).
fn cooked_files(root: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(cache(root)).expect("cache dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "asset_manifest.bin" {
            continue;
        }
        let meta = entry.metadata().expect("metadata");
        let modified = meta.modified().expect("mtime");
        out.insert(name, (std::fs::read(entry.path()).expect("read"), modified));
    }
    out
}

/// The cooked file of the glTF's sub-asset whose type path ends with `type_suffix`.
fn sub_asset_file(root: &Path, type_suffix: &str) -> PathBuf {
    let manifest = AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present");
    let entry = manifest
        .entries
        .iter()
        .find(|e| {
            (e.path.starts_with("assets/crate.gltf#") || e.path == "assets/crate_albedo.png")
                && e.type_path.ends_with(type_suffix)
        })
        .unwrap_or_else(|| panic!("no {type_suffix} sub-asset in {:?}", manifest.entries));
    cache(root).join(cooked_file_name(entry.id))
}

/// Sets the modification time of every cooked file but the manifest `ago` back.
fn age_cooked(root: &Path, ago: Duration) {
    let then = SystemTime::now() - ago;
    for entry in std::fs::read_dir(cache(root)).expect("cache dir").flatten() {
        if entry.file_name() == "asset_manifest.bin" || !entry.path().is_file() {
            continue;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(entry.path())
            .expect("open");
        file.set_modified(then).expect("set mtime");
    }
}

/// Sets the modification time of every file under `assets/` an hour back.
fn age_assets(root: &Path) {
    let then = SystemTime::now() - Duration::from_secs(3600);
    for entry in std::fs::read_dir(root.join("assets")).expect("assets").flatten() {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(entry.path())
            .expect("open");
        file.set_modified(then).expect("set mtime");
    }
}

#[test]
fn an_unchanged_project_recooks_without_importing() {
    let root = regia_test_utils::unique_temp_dir("cook-skip-unchanged");
    write_project(&root);
    age_assets(&root);

    let first = cook(&root);
    assert_eq!((first.cooked, first.skipped, first.imports_skipped), (2, 0, 0));
    // Back-date the cooked files (still newer than the sources): a rewrite in the second cook then
    // always moves the mtime, even when both cooks land inside one coarse timestamp tick
    // (audit-v035-flaky M9).
    age_cooked(&root, Duration::from_secs(1800));
    let files = cooked_files(&root);
    let manifest = std::fs::read(cache(&root).join("asset_manifest.bin")).expect("manifest");
    let db = std::fs::read(regia_asset::asset_db_path(&root)).expect("db");

    let second = cook(&root);
    assert_eq!((second.cooked, second.skipped, second.imports_skipped), (0, 2, 2));
    // No importer ran: no cooked file was rewritten (the old path rewrote every glTF sub-asset),
    // and the manifest and database are the same bytes.
    assert_eq!(cooked_files(&root), files);
    assert_eq!(
        std::fs::read(cache(&root).join("asset_manifest.bin")).expect("manifest"),
        manifest
    );
    assert_eq!(std::fs::read(regia_asset::asset_db_path(&root)).expect("db"), db);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn old_unchanged_files_are_not_read_again() {
    let root = regia_test_utils::unique_temp_dir("cook-skip-stamps");
    write_project(&root);
    let first = cook(&root);
    assert!(first.hashed_bytes > 0);

    // Files written just now are hashed again on each cook (their stamp is racy). Once old,
    // the next cook stamps them and the one after reads nothing.
    age_assets(&root);
    let restamp = cook(&root);
    assert_eq!(restamp.imports_skipped, 2);
    assert!(restamp.hashed_bytes > 0);
    let quiet = cook(&root);
    assert_eq!(quiet.imports_skipped, 2);
    assert_eq!(quiet.hashed_bytes, 0, "an unchanged old project is not read");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_edit_to_any_input_cooks_its_source_again() {
    let root = regia_test_utils::unique_temp_dir("cook-skip-edits");
    write_project(&root);
    cook(&root);
    let assets = root.join("assets");

    // The image the glTF references by URI: the glTF imports again and its texture changes.
    let texture = sub_asset_file(&root, "CookedTexture");
    let before = std::fs::read(&texture).expect("texture");
    std::fs::write(assets.join("crate_albedo.png"), png(9)).expect("albedo");
    let report = cook(&root);
    assert_eq!(report.imports_skipped, 1, "only decal.png is unchanged");
    assert_ne!(std::fs::read(&texture).expect("texture"), before);

    // The referenced image's own `.meta` overrides (read by the glTF import).
    let mut meta = MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some("(max_size: Some(16))".into());
    std::fs::write(
        meta_path_for(&assets.join("crate_albedo.png")),
        meta.to_ron_string().expect("ron"),
    )
    .expect("albedo meta");
    assert_eq!(cook(&root).imports_skipped, 1);
    // Replacing the image's .meta also replaces its standalone id. Test the current output.
    let texture = sub_asset_file(&root, "CookedTexture");

    // A setting in the standalone image's `.meta`: it cooks uncompressed now.
    let decal_meta = meta_path_for(&assets.join("decal.png"));
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&decal_meta).expect("meta")).expect("parse");
    meta.settings = Some("(compress: Some(false))".into());
    std::fs::write(&decal_meta, meta.to_ron_string().expect("ron")).expect("write meta");
    let report = cook(&root);
    assert_eq!((report.cooked, report.imports_skipped), (1, 1));

    // A missing cooked output.
    std::fs::remove_file(&texture).expect("remove texture");
    assert_eq!(cook(&root).imports_skipped, 1);
    assert!(texture.is_file(), "the glTF import wrote its texture again");

    // The project's LOD class table (read by every glTF import).
    std::fs::write(root.join("lod_classes.toml"), "# no classes\n").expect("table");
    assert_eq!(cook(&root).imports_skipped, 1);

    // A full (not incremental) cook imports everything.
    let full = cook_project(CookOptions {
        incremental: false,
        ..options(&root)
    })
    .expect("full cook");
    assert_eq!(full.imports_skipped, 0);
    assert_eq!(cook(&root).imports_skipped, 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failed_import_is_never_skipped() {
    let root = regia_test_utils::unique_temp_dir("cook-skip-failed");
    write_project(&root);
    cook(&root);
    let gltf = root.join("assets").join("crate.gltf");
    let good = std::fs::read(&gltf).expect("gltf");

    std::fs::write(&gltf, b"{ not a gltf").expect("break");
    let broken = cook_project(options(&root)).expect("cook");
    assert!(broken.errors.iter().any(|(path, _)| path == "assets/crate.gltf"));

    // Restored to the bytes of its last good cook: imported again, not skipped on outputs a
    // failed import may have left half written.
    std::fs::write(&gltf, good).expect("restore");
    let report = cook(&root);
    assert_eq!(report.imports_skipped, 1, "only decal.png skips");
    assert_eq!(cook(&root).imports_skipped, 2);
    let _ = std::fs::remove_dir_all(&root);
}
