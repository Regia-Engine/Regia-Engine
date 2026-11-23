// SPDX-License-Identifier: MPL-2.0
//! Spec 57 § 10: the warm discovery cache. Every claim is a counter of manifests parsed or
//! reused, never a time (ADR-2844).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regia_framework::extension::cache::{CACHE_FILE, CacheState, CacheStats};
use regia_framework::extension::discover::{self, MANIFEST_FILE, discover_cached};
use regia_test_utils::unique_temp_dir;

fn manifest(id: &str, extra: &str) -> String {
    format!(
        "format_version = 1\nid = \"{id}\"\nversion = \"1.2.3\"\nprovides = [\"cap_{id}\"]\n\
         capabilities = [\"content_mount\"]\n{extra}\n[[dependency]]\nid = \"base\"\nversion = \">=1\"\noptional = true\n"
    )
}

fn write(root: &Path, id: &str, text: &str) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(MANIFEST_FILE), text).unwrap();
}

struct Fixture {
    root: PathBuf,
    cache: PathBuf,
}

fn fixture(tag: &str, count: usize) -> Fixture {
    let base = unique_temp_dir(tag);
    let root = base.join("extensions");
    for i in 0..count {
        let id = format!("ext{i:03}");
        write(&root, &id, &manifest(&id, ""));
    }
    Fixture {
        root,
        cache: base.join(".regia").join(CACHE_FILE),
    }
}

fn run(f: &Fixture) -> (discover::Discovery, CacheStats) {
    discover_cached(std::slice::from_ref(&f.root), &f.cache)
}

#[test]
fn a_cold_run_parses_everything_and_a_warm_run_parses_nothing() {
    let f = fixture("ext-cache-warm", 50);
    let (cold, stats) = run(&f);
    assert_eq!(
        (stats.state, stats.parsed, stats.reused, stats.written),
        (CacheState::Cold, 50, 0, true)
    );
    let (warm, stats) = run(&f);
    assert_eq!(
        (stats.state, stats.parsed, stats.reused, stats.written),
        (CacheState::Warm, 0, 50, false)
    );
    // The same answer as discovery without a cache, in the same order.
    let plain = discover::discover(std::slice::from_ref(&f.root));
    for found in [&cold, &warm] {
        assert_eq!(found.manifests.len(), plain.manifests.len());
        for (a, b) in found.manifests.iter().zip(&plain.manifests) {
            assert_eq!((&a.manifest, &a.path, &a.hash), (&b.manifest, &b.path, &b.hash));
        }
    }
}

#[test]
fn a_changed_manifest_is_parsed_and_the_rest_reused() {
    let f = fixture("ext-cache-changed", 10);
    run(&f);
    write(
        &f.root,
        "ext004",
        &manifest("ext004", "description = \"edited\"\n"),
    );
    let (found, stats) = run(&f);
    assert_eq!((stats.parsed, stats.reused, stats.written), (1, 9, true));
    let edited = found
        .manifests
        .iter()
        .find(|m| m.manifest.id == "ext004")
        .unwrap();
    assert_eq!(edited.manifest.description, "edited");
    let (_, stats) = run(&f);
    assert_eq!((stats.parsed, stats.reused, stats.written), (0, 10, false));
}

#[test]
fn added_and_removed_extensions_update_the_file() {
    let f = fixture("ext-cache-set", 5);
    run(&f);
    write(&f.root, "extnew", &manifest("extnew", ""));
    std::fs::remove_dir_all(f.root.join("ext001")).unwrap();
    let (found, stats) = run(&f);
    assert_eq!((stats.parsed, stats.reused, stats.written), (1, 4, true));
    assert_eq!(found.manifests.len(), 5);
    let text = std::fs::read_to_string(&f.cache).unwrap();
    assert!(!text.contains("ext001"), "the removed extension left the cache");
    assert!(text.contains("extnew"));
}

#[test]
fn a_corrupt_cache_is_a_cold_start_and_is_rewritten() {
    for (tag, bytes) in [
        ("garbage", b"\x00\x01 not json".to_vec()),
        (
            "truncated",
            b"{\"format_version\":1,\"engine_version\":\"0.1.".to_vec(),
        ),
        (
            "wrong-shape",
            b"{\"format_version\":1,\"engine_version\":3,\"entries\":7}".to_vec(),
        ),
        ("empty", Vec::new()),
    ] {
        let f = fixture(&format!("ext-cache-bad-{tag}"), 6);
        std::fs::create_dir_all(f.cache.parent().unwrap()).unwrap();
        std::fs::write(&f.cache, bytes).unwrap();
        let (found, stats) = run(&f);
        assert_eq!(stats.state, CacheState::Corrupt, "{tag}");
        assert_eq!((stats.parsed, stats.reused, stats.written), (6, 0, true), "{tag}");
        assert_eq!(found.manifests.len(), 6, "{tag}");
        let (_, stats) = run(&f);
        assert_eq!(
            (stats.state, stats.parsed, stats.reused),
            (CacheState::Warm, 0, 6),
            "{tag}"
        );
    }
}

#[test]
fn a_directory_where_the_file_should_be_is_corrupt_not_a_crash() {
    let f = fixture("ext-cache-dir", 3);
    std::fs::create_dir_all(&f.cache).unwrap();
    let (found, stats) = run(&f);
    assert_eq!((stats.state, stats.parsed), (CacheState::Corrupt, 3));
    assert!(!stats.written, "a directory cannot be replaced by the rename");
    assert_eq!(found.manifests.len(), 3);
}

#[test]
fn another_format_or_engine_version_is_discarded() {
    let f = fixture("ext-cache-version", 4);
    run(&f);
    for (key, value) in [
        ("format_version", serde_json::json!(99)),
        ("engine_version", serde_json::json!("0.0.0-other")),
    ] {
        let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&f.cache).unwrap()).unwrap();
        doc[key] = value;
        std::fs::write(&f.cache, serde_json::to_vec(&doc).unwrap()).unwrap();
        let (_, stats) = run(&f);
        assert_eq!(stats.state, CacheState::Outdated, "{key}");
        assert_eq!((stats.parsed, stats.reused, stats.written), (4, 0, true), "{key}");
    }
}

#[test]
fn an_entry_that_no_longer_converts_is_parsed_from_the_file() {
    let f = fixture("ext-cache-entry", 3);
    run(&f);
    let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&f.cache).unwrap()).unwrap();
    doc["entries"][0]["manifest"]["version"] = serde_json::json!("not a version");
    doc["entries"][1]["manifest"]["dependencies"][0]["version"] = serde_json::json!("%%");
    std::fs::write(&f.cache, serde_json::to_vec(&doc).unwrap()).unwrap();
    let (found, stats) = run(&f);
    assert_eq!((stats.parsed, stats.reused), (2, 1));
    assert_eq!(found.manifests.len(), 3);
    assert!(
        found
            .manifests
            .iter()
            .all(|m| m.manifest.version.to_string() == "1.2.3")
    );
}

#[test]
fn the_hash_is_of_the_manifest_bytes_not_the_timestamp() {
    let f = fixture("ext-cache-bytes", 2);
    run(&f);
    // Same size, different content, written straight after: a modification time could not tell.
    let text = manifest("ext000", "");
    let edited = text.replace("1.2.3", "1.2.4");
    assert_eq!(text.len(), edited.len());
    write(&f.root, "ext000", &edited);
    let (found, stats) = run(&f);
    assert_eq!(stats.parsed, 1);
    assert_eq!(found.manifests[0].manifest.version.to_string(), "1.2.4");
}

#[test]
fn an_entry_is_keyed_by_directory_not_by_hash_alone() {
    let f = fixture("ext-cache-dirkey", 1);
    run(&f);
    // A copy of the same bytes in another directory is a different extension location.
    write(&f.root, "ext_copy", &manifest("ext_copy", ""));
    let (_, stats) = run(&f);
    assert_eq!((stats.parsed, stats.reused), (1, 1));
}

#[test]
fn invalid_manifests_stay_diagnostics_and_are_never_cached() {
    let f = fixture("ext-cache-invalid", 2);
    write(
        &f.root,
        "ext_bad",
        "format_version = 1\nid = \"BAD ID\"\nversion = \"1\"\n",
    );
    let (first, _) = run(&f);
    let (second, stats) = run(&f);
    assert_eq!(first.diagnostics.len(), 1);
    assert_eq!(second.diagnostics.len(), 1);
    assert_eq!(first.diagnostics[0].message, second.diagnostics[0].message);
    assert_eq!((stats.parsed, stats.reused), (0, 2));
}

#[test]
fn a_cache_path_that_cannot_be_written_changes_nothing() {
    let f = fixture("ext-cache-unwritable", 3);
    // `.regia` is a file, so the directory cannot be created.
    let blocker = f.cache.parent().unwrap().to_path_buf();
    std::fs::write(&blocker, b"in the way").unwrap();
    let (found, stats) = run(&f);
    assert_eq!(
        (stats.state, stats.parsed, stats.written),
        (CacheState::Cold, 3, false)
    );
    assert_eq!(found.manifests.len(), 3);
}
