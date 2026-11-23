// SPDX-License-Identifier: MPL-2.0
//! End-to-end test: cook → pak → load from pak → verify asset origin.
//!
//! Covers M16 acceptance: pak round trips, byte-identical loads vs dev cache,
//! mount order, and asset source tracking.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, AssetManifest, AssetServer, AssetServerSettings, AssetSource, AssetTypeTag,
    COOKED_FORMAT_VERSION, CookedHeader, CpuMesh, ManifestEntry, cooked_cache_dir, cooked_file_name,
    encode_cooked_file, pack_importer_version,
};
use regia_asset_pipeline::{PakWriteOptions, pack_cooked_directory, write_pak};

fn make_temp_project(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia_e2e_pak_{}_{}", prefix, uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Write a CpuMesh to the cooked cache and return (root, id, cache_dir).
fn write_cooked_mesh(root: &Path, platform: &str) -> (AssetId, PathBuf) {
    let cache = cooked_cache_dir(root, platform);
    std::fs::create_dir_all(&cache).expect("cache dir");

    let id = AssetId::new_random();
    let mesh = CpuMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
        tangents: Vec::new(),
        indices: vec![0, 1, 2],
        ..Default::default()
    };
    let payload = mesh.to_postcard().expect("postcard");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh_legacy_cpu().0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, &payload).expect("encode");
    std::fs::write(cache.join(cooked_file_name(id)), file).expect("write");

    (id, cache)
}

/// Write a manifest with the given entry.
fn write_manifest(cache_dir: &Path, entries: Vec<ManifestEntry>) {
    let manifest = AssetManifest { entries };
    manifest.write_to_cache(cache_dir).expect("manifest write");
}

#[test]
fn e2e_cook_pak_load_from_pak_and_fallback_to_cache() {
    let root = make_temp_project("e2e_r1");
    let (id, cache) = write_cooked_mesh(&root, "linux");
    write_manifest(
        &cache,
        vec![ManifestEntry {
            path: "meshes/test.mesh".to_string(),
            id,
            type_path: "regia_asset::CpuMesh".to_string(),
        }],
    );

    // Pak the cooked directory.
    let pak_dir = root.join("paks");
    std::fs::create_dir_all(&pak_dir).expect("pak dir");
    let paks = pack_cooked_directory(
        &cache,
        &pak_dir,
        &BTreeMap::new(),
        false, // no compression for test speed
    )
    .expect("pack");
    assert_eq!(paks.len(), 1, "should produce base.fpak");

    // Load from pak.
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".to_string(),
        content_dir: Some(pak_dir.clone()),
    })
    .expect("open server");

    assert!(server.has_paks(), "paks should be mounted");
    assert_eq!(server.pak_count(), 1);

    let h = server.load_mesh_path("meshes/test.mesh").expect("load from pak");
    // Use server.meshes().get to get the mesh, then clone it.
    let mesh_from_pak = server.meshes().get(&h).cloned().expect("get mesh");
    assert_eq!(mesh_from_pak.indices.len(), 3);
    assert_eq!(
        server.source_of(id),
        AssetSource::Pak("base".to_string()),
        "asset must come from the pak"
    );

    // Drop server to release the borrow.
    drop(server);

    // Remove paks and load again — should fall back to dev cache.
    let _ = std::fs::remove_dir_all(&pak_dir);

    let mut server2 = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .expect("open server without paks");

    let h2 = server2
        .load_mesh_path("meshes/test.mesh")
        .expect("load from cache");
    let mesh_from_cache = server2.meshes().get(&h2).cloned().expect("get mesh from cache");
    assert_eq!(mesh_from_cache.indices.len(), 3);
    assert_eq!(
        server2.source_of(id),
        AssetSource::DevCache,
        "should fall back to dev cache when pak is removed"
    );

    // Verify byte-identical data.
    assert_eq!(
        mesh_from_pak, mesh_from_cache,
        "pak and dev cache must produce identical mesh data"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn e2e_pak_override_order() {
    let root = make_temp_project("e2e_override");
    let (id, cache) = write_cooked_mesh(&root, "linux");

    // Create a second, different mesh with the same id for the override pak.
    let mesh2 = CpuMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
        tangents: Vec::new(),
        indices: vec![0, 1, 2, 1, 3, 2], // quad = 6 indices
        ..Default::default()
    };
    let payload2 = mesh2.to_postcard().expect("postcard");
    let header2 = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh_legacy_cpu().0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload2.len() as u64,
    };
    let cooked2 = encode_cooked_file(&header2, &payload2).expect("encode");

    let override_cache = root.join(".regia").join("cooked").join("override");
    std::fs::create_dir_all(&override_cache).expect("override cache");
    std::fs::write(override_cache.join(cooked_file_name(id)), &cooked2).expect("write override");

    let manifest_entry = ManifestEntry {
        path: "meshes/test.mesh".to_string(),
        id,
        type_path: "regia_asset::CpuMesh".to_string(),
    };
    write_manifest(&cache, vec![manifest_entry.clone()]);
    write_manifest(&override_cache, vec![manifest_entry.clone()]);

    // Write two paks: 00_base.fpak (small triangle) and 01_override.fpak (quad).
    let pak_dir = root.join("paks");
    std::fs::create_dir_all(&pak_dir).expect("pak dir");

    let _ = pack_cooked_directory(&cache, &pak_dir, &BTreeMap::new(), false).expect("pack base");

    // Rename base.fpak to 00_base.fpak.
    std::fs::rename(pak_dir.join("base.fpak"), pak_dir.join("00_base.fpak")).expect("rename");

    // Write override pak manually.
    let manifest = AssetManifest {
        entries: vec![manifest_entry],
    };
    write_pak(
        &pak_dir.join("01_override.fpak"),
        &override_cache,
        &[(id, AssetTypeTag::mesh_legacy_cpu().0)],
        &manifest,
        &PakWriteOptions {
            compress: false,
            chunk_name: "override".to_string(),
            dependencies: vec![],
            min_ratio: 0.0,
        },
    )
    .expect("write override pak");

    // Load with both paks in content dir — the override should win.
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".to_string(),
        content_dir: Some(pak_dir.clone()),
    })
    .expect("open server");

    assert_eq!(server.pak_count(), 2, "both paks mounted");
    let h = server.load_mesh_path("meshes/test.mesh").expect("load");
    let m = server.meshes().get(&h).cloned().expect("get");
    assert_eq!(
        m.indices.len(),
        6,
        "later pak (01_override) must override earlier (00_base): got {} indices",
        m.indices.len()
    );
    assert_eq!(
        server.source_of(id),
        AssetSource::Pak("01_override".to_string()),
        "source must be the override pak"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn e2e_pak_fails_when_no_asset_exists() {
    let root = make_temp_project("e2e_nodata");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache dir");

    // Server with no paks and no manifest entries.
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .expect("open");

    // Loading an unknown path should fail with a clear error.
    let result = server.load_mesh_path("meshes/nonexistent.mesh");
    assert!(result.is_err(), "loading nonexistent path must fail: {result:?}");
    let err = result.expect_err("must error");
    assert!(
        err.to_string().contains("no manifest entry"),
        "error should mention missing manifest: {err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
