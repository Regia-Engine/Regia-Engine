// SPDX-License-Identifier: MPL-2.0
//! Registry building from hand-written cooked files: header reading on damaged, truncated and
//! very long headers, per-record fallbacks, the incremental update's fallbacks, and the project
//! root of a cache directory. No cook, no clock.

use std::path::Path;

use regia_asset::registry::{AssetRegistry, TagRules, cache_registry_path, pak_registry_path};
use regia_asset::{
    AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry, cooked_file_name,
    encode_cooked_file,
};
use regia_asset_pipeline::AssetDatabase;
use regia_asset_pipeline::registry_build::{
    project_root_of_cache, read_cooked_header, registry_entries, update_cache_registry, write_cache_registry,
    write_pak_registry,
};
use regia_core::id::TypePathHash;
use regia_test_utils::unique_temp_dir;

fn header(id: AssetId, dependencies: Vec<AssetId>, payload_len: usize) -> CookedHeader {
    CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of("regia_asset::CookedTexture"),
        asset_id: id,
        importer_version: 1,
        dependencies,
        payload_len: payload_len as u64,
    }
}

fn write_cooked(dir: &Path, id: AssetId, dependencies: Vec<AssetId>, payload: &[u8]) -> u64 {
    let bytes = encode_cooked_file(&header(id, dependencies, payload.len()), payload).expect("encode");
    std::fs::write(dir.join(cooked_file_name(id)), &bytes).expect("write cooked");
    bytes.len() as u64
}

fn entry(path: &str, id: AssetId) -> ManifestEntry {
    ManifestEntry {
        path: path.into(),
        id,
        type_path: "regia_asset::CookedTexture".into(),
    }
}

#[test]
fn a_header_longer_than_the_read_prefix_still_decodes() {
    let dir = unique_temp_dir("reg-long");
    let id = AssetId::new_random();
    // 16 bytes per id: 2,000 dependencies is well over the 16 KiB prefix.
    let deps: Vec<AssetId> = (0..2000).map(|_| AssetId::new_random()).collect();
    write_cooked(&dir, id, deps.clone(), b"payload");
    let read = read_cooked_header(&dir.join(cooked_file_name(id))).expect("long header");
    assert_eq!(read.dependencies, deps);
    assert_eq!(read.asset_id, id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_cooked_files_have_no_header() {
    let dir = unique_temp_dir("reg-bad");
    let id = AssetId::new_random();
    let path = dir.join(cooked_file_name(id));
    assert!(read_cooked_header(&path).is_none(), "missing file");
    for (name, bytes) in [
        ("empty", Vec::new()),
        ("short", b"FC".to_vec()),
        ("magic only", b"FCKD".to_vec()),
        ("wrong magic", b"XXXXabcdefgh".to_vec()),
        ("garbage after magic", [b"FCKD".as_slice(), &[0xff; 40]].concat()),
    ] {
        std::fs::write(&path, &bytes).expect("write");
        assert!(read_cooked_header(&path).is_none(), "{name}");
    }
    // A header cut off exactly at the 16 KiB prefix with the rest of the file missing.
    let deps: Vec<AssetId> = (0..2000).map(|_| AssetId::new_random()).collect();
    let full = encode_cooked_file(&header(id, deps, 0), &[]).expect("encode");
    std::fs::write(&path, &full[..16 * 1024]).expect("write truncated");
    assert!(read_cooked_header(&path).is_none(), "truncated long header");
    // A directory where the file should be.
    std::fs::remove_file(&path).expect("rm");
    std::fs::create_dir(&path).expect("dir");
    assert!(read_cooked_header(&path).is_none(), "directory");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn records_drop_self_dependencies_and_fall_back_for_missing_files() {
    let dir = unique_temp_dir("reg-entries");
    let (a, b, missing, bad) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    let size_a = write_cooked(&dir, a, vec![a, b, a], b"aaaa");
    write_cooked(&dir, b, vec![], b"b");
    std::fs::write(dir.join(cooked_file_name(bad)), b"not a cooked file").expect("bad");
    let entries = [
        entry("assets/a.png", a),
        entry("assets/b.png", b),
        entry("assets/missing.png", missing),
        entry("assets/bad.png", bad),
    ];
    let (records, without) =
        registry_entries(&dir, &entries, &TagRules::default(), &AssetDatabase::default());
    assert_eq!(without, 2, "the missing and the damaged file");
    assert_eq!(records.len(), 4);
    assert_eq!(
        records[0].dependencies,
        vec![b],
        "the self reference is dropped, the other kept"
    );
    assert_eq!(records[0].size, size_a);
    assert_eq!(records[2].size, 0, "no file, no size");
    assert!(records[2].dependencies.is_empty());
    assert_eq!(records[3].size, "not a cooked file".len() as u64);
    assert!(records[3].dependencies.is_empty());
    // Records keep manifest order and carry the automatic tags.
    assert_eq!(records[0].tags.get("ext").map(String::as_str), Some("png"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn project_with_cache(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let cache = root.join(".regia").join("cooked").join("linux");
    std::fs::create_dir_all(&cache).expect("cache");
    (root, cache)
}

#[test]
fn project_root_of_cache_needs_the_cache_layout_and_an_assets_directory() {
    let (root, cache) = project_with_cache("reg-root");
    assert_eq!(project_root_of_cache(&cache), Some(root.as_path()));
    // A directory of the right depth without `assets/` beside `.regia` is not a project.
    std::fs::remove_dir(root.join("assets")).expect("rm assets");
    assert_eq!(project_root_of_cache(&cache), None);
    // Too shallow.
    assert_eq!(project_root_of_cache(Path::new("/")), None);
    assert_eq!(project_root_of_cache(Path::new("linux")), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_cache_without_a_manifest_builds_an_empty_registry() {
    let (root, cache) = project_with_cache("reg-nomanifest");
    let report = write_cache_registry(&root, &cache, &AssetDatabase::default()).expect("build");
    assert_eq!((report.records, report.without_header, report.changed), (0, 0, 0));
    let registry = AssetRegistry::read_from(&cache_registry_path(&cache))
        .expect("read")
        .expect("exists");
    assert!(registry.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_damaged_manifest_is_an_error_not_an_empty_registry() {
    let (root, cache) = project_with_cache("reg-badmanifest");
    std::fs::write(cache.join("asset_manifest.bin"), [0xff; 9]).expect("manifest");
    assert!(write_cache_registry(&root, &cache, &AssetDatabase::default()).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

fn manifest_of(entries: Vec<ManifestEntry>) -> AssetManifest {
    AssetManifest { entries }
}

#[test]
fn the_incremental_update_rebuilds_when_the_registry_is_missing_or_damaged() {
    let (root, cache) = project_with_cache("reg-fallback");
    let id = AssetId::new_random();
    write_cooked(&cache, id, vec![], b"x");
    manifest_of(vec![entry("assets/a.png", id)])
        .write_to_cache(&cache)
        .expect("manifest");
    let db = AssetDatabase::default();
    // No registry on disk: a full build, so `changed` is the full build's 0 and the record exists.
    let report = update_cache_registry(&root, &cache, &["assets/a.png".to_string()], &db).expect("update");
    assert_eq!((report.records, report.changed), (1, 0));
    // A registry that does not decode is rebuilt too.
    std::fs::write(cache_registry_path(&cache), [1u8, 2, 3]).expect("damage");
    let report = update_cache_registry(&root, &cache, &["assets/a.png".to_string()], &db).expect("update");
    assert_eq!(report.records, 1);
    assert!(
        AssetRegistry::read_from(&cache_registry_path(&cache))
            .expect("read")
            .expect("exists")
            .by_id(id)
            .is_some()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_incremental_update_adds_replaces_and_removes_one_source() {
    let (root, cache) = project_with_cache("reg-incremental");
    let db = AssetDatabase::default();
    let (a, b, sub) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    for id in [a, b, sub] {
        write_cooked(&cache, id, vec![], b"x");
    }
    manifest_of(vec![
        entry("assets/a.glb", a),
        entry("assets/a.glb#mesh/Body", sub),
        entry("assets/b.png", b),
    ])
    .write_to_cache(&cache)
    .expect("manifest");
    write_cache_registry(&root, &cache, &db).expect("full");

    // Nothing to do: an update for a source whose records are unchanged reports 0 and leaves the
    // file byte for byte as it was.
    let before = std::fs::read(cache_registry_path(&cache)).expect("read");
    let report = update_cache_registry(&root, &cache, &["assets/b.png".to_string()], &db).expect("noop");
    assert_eq!((report.records, report.changed), (3, 0));
    assert_eq!(std::fs::read(cache_registry_path(&cache)).expect("read"), before);

    // Source a.glb re-cooked with its sub-asset gone: both records of `a.glb` are replaced, and
    // the sub-asset record is removed. b.png is untouched.
    let a2 = AssetId::new_random();
    write_cooked(&cache, a2, vec![b], b"yy");
    manifest_of(vec![entry("assets/a.glb", a2), entry("assets/b.png", b)])
        .write_to_cache(&cache)
        .expect("manifest");
    let report = update_cache_registry(&root, &cache, &["assets/a.glb".to_string()], &db).expect("update");
    assert!(report.changed >= 2, "{report:?}");
    let registry = AssetRegistry::read_from(&cache_registry_path(&cache))
        .expect("read")
        .expect("exists");
    assert_eq!(registry.len(), 2);
    assert!(registry.by_id(a).is_none() && registry.by_id(sub).is_none());
    assert_eq!(registry.by_id(a2).expect("new a").dependencies, vec![b]);
    assert!(registry.by_id(b).is_some());

    // The source deleted entirely: its record disappears.
    manifest_of(vec![entry("assets/b.png", b)])
        .write_to_cache(&cache)
        .expect("manifest");
    let report = update_cache_registry(&root, &cache, &["assets/a.glb".to_string()], &db).expect("remove");
    assert_eq!(report.changed, 1);
    assert_eq!(report.records, 1);

    // The same source listed twice is applied once.
    let again = update_cache_registry(
        &root,
        &cache,
        &["assets/a.glb".to_string(), "assets/a.glb".to_string()],
        &db,
    )
    .expect("dup");
    assert_eq!(again.changed, 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_pak_registry_works_without_a_project() {
    let dir = unique_temp_dir("reg-pak");
    let id = AssetId::new_random();
    write_cooked(&dir, id, vec![], b"abc");
    let manifest = manifest_of(vec![entry("assets/a.png", id)]);
    let pak = dir.join("base.fpak");
    let out = write_pak_registry(None, &dir, &pak, &manifest).expect("write");
    assert_eq!(out, pak_registry_path(&pak));
    let registry = AssetRegistry::read_from(&out).expect("read").expect("exists");
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.by_id(id).expect("record").path, "assets/a.png");
    // The same call twice writes the same bytes.
    let first = std::fs::read(&out).expect("read");
    write_pak_registry(None, &dir, &pak, &manifest).expect("again");
    assert_eq!(std::fs::read(&out).expect("read"), first);
    let _ = std::fs::remove_dir_all(&dir);
}
