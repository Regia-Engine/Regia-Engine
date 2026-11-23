// SPDX-License-Identifier: MPL-2.0
//! The shared cook cache (ADR-2612): machine A cooks a project into a shared directory, machine B
//! (another copy of the project with its own local cache) cooks it with no import and the same
//! bytes. Each "machine" is a temp project directory; the shared cache is a temp directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::ImageEncoder;
use regia_asset::{AssetManifest, cooked_cache_dir};
use regia_asset_pipeline::cook_cache::{
    SharedCacheChoice, SharedCacheConfig, SharedCacheMode, SharedCacheSummary, last_summary, sample_entry,
    verify_entry,
};
use regia_asset_pipeline::{
    CookOptions, CookReport, cook_project, cook_project_with_shared_cache, host_platform,
};

/// The shareable sources of the fixture: a glTF (its buffer and image are read through it) and two
/// standalone PNGs.
const SOURCES: u64 = 3;

/// A 64×64 RGBA PNG whose pixels depend on `seed`.
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

/// A textured quad `crate.gltf` (buffer `crate.bin`, image `crate_albedo.png`) plus `decal.png`
/// and `rust.png`.
fn write_project(root: &Path) {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    let mut bin = Vec::new();
    for p in [
        [-1.0f32, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ] {
        p.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for _ in 0..4 {
        [0.0f32, 1.0, 0.0]
            .iter()
            .for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for uv in [[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]] {
        uv.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    [0u16, 2, 1, 0, 3, 2]
        .iter()
        .for_each(|i| bin.extend_from_slice(&i.to_le_bytes()));
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
    std::fs::write(assets.join("rust.png"), png(3)).expect("rust");
}

/// Another machine's checkout of `from`: its `assets/` with the `.meta` files (which a team
/// commits) and its `Regia.toml`, but no `.regia/` (cooked cache, asset database, input stamps).
fn checkout(from: &Path, tag: &str) -> PathBuf {
    let to = regia_test_utils::unique_temp_dir(tag);
    copy_dir(&from.join("assets"), &to.join("assets"));
    if from.join("Regia.toml").is_file() {
        std::fs::copy(from.join("Regia.toml"), to.join("Regia.toml")).expect("copy manifest");
    }
    to
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dir");
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            std::fs::copy(&path, to.join(entry.file_name())).expect("copy");
        }
    }
}

fn shared(dir: &Path, mode: SharedCacheMode) -> SharedCacheChoice {
    SharedCacheChoice::Use(SharedCacheConfig::new(dir, mode))
}

fn shared_as(dir: &Path, mode: SharedCacheMode, build: &str) -> SharedCacheChoice {
    let mut config = SharedCacheConfig::new(dir, mode);
    config.engine_build_id = Some(build.to_string());
    SharedCacheChoice::Use(config)
}

fn cook_with(root: &Path, choice: &SharedCacheChoice) -> CookReport {
    let report =
        cook_project_with_shared_cache(CookOptions::for_host(root.to_path_buf()), choice).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    report
}

fn summary(report: &CookReport) -> &SharedCacheSummary {
    report.shared_cache.as_ref().expect("a shared cache summary")
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

/// Every cooked asset file (`.fckd`) of a project: name → bytes.
fn cooked_outputs(root: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(cache(root))
        .expect("cache dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fckd"))
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("read"),
            )
        })
        .collect()
}

fn manifest(root: &Path) -> AssetManifest {
    AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present")
}

/// Every file under `dir`: path → (bytes, modified).
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(snapshot(&path));
        } else {
            let modified = entry.metadata().and_then(|m| m.modified()).expect("mtime");
            out.insert(path.clone(), (std::fs::read(&path).expect("read"), modified));
        }
    }
    out
}

/// The entry files of a shared cache.
fn entries(share: &Path) -> Vec<PathBuf> {
    snapshot(share)
        .into_keys()
        .filter(|p| p.extension().is_some_and(|e| e == "rcc"))
        .collect()
}

/// The entry file holding `rel_path`.
fn entry_of(share: &Path, rel_path: &str) -> PathBuf {
    entries(share)
        .into_iter()
        .find(|p| verify_entry(&std::fs::read(p).expect("read")).is_ok_and(|s| s.rel_path == rel_path))
        .unwrap_or_else(|| panic!("no entry for {rel_path}"))
}

fn cleanup(dirs: &[&Path]) {
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn machine_b_cooks_from_the_shared_cache_with_zero_imports_and_the_same_bytes() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-a");
    write_project(&a);
    let first = cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    assert_eq!(first.cooked as u64, SOURCES);
    let s = summary(&first);
    assert_eq!((s.hits, s.published, s.errors), (0, SOURCES, 0));
    assert!(s.bytes_written > 0);
    assert_eq!(entries(&share).len() as u64, SOURCES);

    let b = checkout(&a, "shared-cook-b");
    let report = cook_with(&b, &shared(&share, SharedCacheMode::ReadWrite));
    let s = summary(&report);
    assert_eq!((s.hits, s.misses, s.corrupt, s.published), (SOURCES, 0, 0, 0));
    // Zero imports: every source cooked on B came from the cache, none was skipped locally.
    assert_eq!((report.cooked as u64, report.imports_skipped), (SOURCES, 0));
    assert_eq!(cooked_outputs(&b), cooked_outputs(&a), "byte-identical outputs");
    assert_eq!(manifest(&b).entries, manifest(&a).entries);
    assert_eq!(
        last_summary(&cache(&b)).as_ref(),
        Some(s),
        "the summary is in the cook ledger"
    );

    // The fetched sources were recorded locally: B's next cook skips them without the cache.
    let again = cook_with(&b, &SharedCacheChoice::Off);
    assert_eq!(
        (again.skipped as u64, again.imports_skipped as u64),
        (SOURCES, SOURCES)
    );
    assert!(again.shared_cache.is_none());
    assert_eq!(
        last_summary(&cache(&b)).as_ref(),
        Some(s),
        "kept by a cook without the cache"
    );
    cleanup(&[&share, &a, &b]);
}

#[test]
fn changing_one_source_recooks_only_it() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-edit-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-edit-a");
    write_project(&a);
    cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    let b = checkout(&a, "shared-cook-edit-b");
    cook_with(&b, &shared(&share, SharedCacheMode::ReadWrite));

    // A edits one texture and cooks: only it is imported and published.
    std::fs::write(a.join("assets/decal.png"), png(9)).expect("edit");
    let edited = cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    assert_eq!((edited.cooked, edited.imports_skipped as u64), (1, SOURCES - 1));
    assert_eq!(summary(&edited).published, 1);

    // B pulls the edit: one fetch, the rest skipped locally, no import.
    std::fs::copy(a.join("assets/decal.png"), b.join("assets/decal.png")).expect("pull");
    let pulled = cook_with(&b, &shared(&share, SharedCacheMode::ReadWrite));
    let s = summary(&pulled);
    assert_eq!((s.hits, s.misses, s.published), (1, 0, 0));
    assert_eq!((pulled.cooked, pulled.imports_skipped as u64), (1, SOURCES - 1));
    assert_eq!(cooked_outputs(&b), cooked_outputs(&a));
    cleanup(&[&share, &a, &b]);
}

#[test]
fn a_truncated_entry_is_detected_and_recooked() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-corrupt-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-corrupt-a");
    write_project(&a);
    cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    let entry = entry_of(&share, "assets/decal.png");
    let len = std::fs::metadata(&entry).expect("meta").len();
    std::fs::File::options()
        .write(true)
        .open(&entry)
        .expect("open")
        .set_len(len / 2)
        .expect("truncate");

    let b = checkout(&a, "shared-cook-corrupt-b");
    let report = cook_with(&b, &shared(&share, SharedCacheMode::ReadWrite));
    let s = summary(&report);
    assert_eq!((s.hits, s.misses, s.corrupt, s.published), (SOURCES - 1, 1, 1, 1));
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("assets/decal.png") && w.contains("corrupt")),
        "{:?}",
        report.warnings
    );
    // Re-imported with the same bytes, and the entry replaced by a whole one.
    assert_eq!(cooked_outputs(&b), cooked_outputs(&a));
    assert!(verify_entry(&std::fs::read(&entry).expect("read")).is_ok());
    cleanup(&[&share, &a, &b]);
}

#[test]
fn two_concurrent_cooks_of_the_same_keys_both_succeed_with_one_entry_each() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-race-share");
    let origin = regia_test_utils::unique_temp_dir("shared-cook-race-origin");
    write_project(&origin);
    // The first cook writes the `.meta` files the team commits; it does not use the cache.
    cook_with(&origin, &SharedCacheChoice::Off);
    let machines = [
        checkout(&origin, "shared-cook-race-1"),
        checkout(&origin, "shared-cook-race-2"),
    ];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = machines
        .iter()
        .cloned()
        .map(|root| {
            let barrier = barrier.clone();
            let choice = shared(&share, SharedCacheMode::ReadWrite);
            std::thread::spawn(move || {
                barrier.wait();
                cook_project_with_shared_cache(CookOptions::for_host(root), &choice)
                    .map_err(|e| e.to_string())
            })
        })
        .collect();
    for handle in handles {
        let report = handle.join().expect("thread").expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let s = summary(&report);
        assert_eq!(s.errors, 0);
        assert_eq!(s.hits + s.misses, SOURCES);
    }
    let files = snapshot(&share);
    assert!(
        files.keys().all(|p| !p.to_string_lossy().ends_with(".tmp")),
        "no temp file left: {:?}",
        files.keys()
    );
    let found = entries(&share);
    assert_eq!(found.len() as u64, SOURCES, "one entry per key");
    for path in found {
        verify_entry(&std::fs::read(&path).expect("read")).expect("every entry is whole");
    }
    assert_eq!(cooked_outputs(&machines[0]), cooked_outputs(&origin));
    assert_eq!(cooked_outputs(&machines[1]), cooked_outputs(&origin));
    cleanup(&[&share, &origin, &machines[0], &machines[1]]);
}

#[test]
fn the_read_only_mode_never_writes() {
    let origin = regia_test_utils::unique_temp_dir("shared-cook-ro-origin");
    write_project(&origin);
    cook_with(&origin, &SharedCacheChoice::Off);

    // A path that does not exist: a warning, nothing created, the cook goes on locally.
    let missing = regia_test_utils::unique_temp_dir("shared-cook-ro-missing").join("share");
    let b = checkout(&origin, "shared-cook-ro-b");
    let report = cook_with(&b, &shared(&missing, SharedCacheMode::Read));
    assert!(report.shared_cache.is_none());
    assert!(
        report.warnings.iter().any(|w| w.contains("not reachable")),
        "{:?}",
        report.warnings
    );
    assert!(!missing.exists());
    assert_eq!(report.cooked as u64, SOURCES);

    // An empty cache: every lookup misses and nothing is published or created.
    let share = regia_test_utils::unique_temp_dir("shared-cook-ro-share");
    let c = checkout(&origin, "shared-cook-ro-c");
    let report = cook_with(&c, &shared(&share, SharedCacheMode::Read));
    let s = summary(&report);
    assert_eq!(
        (s.hits, s.misses, s.published, s.bytes_written),
        (0, SOURCES, 0, 0)
    );
    assert!(snapshot(&share).is_empty());
    assert!(
        std::fs::read_dir(&share).expect("share").next().is_none(),
        "no directory created"
    );

    // A filled cache with one corrupt entry: hits are not touched, the corrupt entry not replaced.
    cook_with(&origin, &shared(&share, SharedCacheMode::ReadWrite));
    let bad = entry_of(&share, "assets/rust.png");
    let len = std::fs::metadata(&bad).expect("meta").len();
    std::fs::File::options()
        .write(true)
        .open(&bad)
        .expect("open")
        .set_len(len - 1)
        .expect("truncate");
    let before = snapshot(&share);
    let d = checkout(&origin, "shared-cook-ro-d");
    let report = cook_with(&d, &shared(&share, SharedCacheMode::Read));
    let s = summary(&report);
    assert_eq!((s.hits, s.corrupt, s.published), (SOURCES - 1, 1, 0));
    assert_eq!(snapshot(&share), before, "bytes and modification times unchanged");
    assert_eq!(cooked_outputs(&d), cooked_outputs(&origin));
    cleanup(&[&origin, missing.parent().expect("parent"), &b, &share, &c, &d]);
}

#[test]
fn another_engine_build_never_matches() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-build-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-build-a");
    write_project(&a);
    cook_with(&a, &shared_as(&share, SharedCacheMode::ReadWrite, "build-x"));
    let b = checkout(&a, "shared-cook-build-b");
    let other = cook_with(&b, &shared_as(&share, SharedCacheMode::Read, "build-y"));
    assert_eq!((summary(&other).hits, summary(&other).misses), (0, SOURCES));
    let c = checkout(&a, "shared-cook-build-c");
    let same = cook_with(&c, &shared_as(&share, SharedCacheMode::Read, "build-x"));
    assert_eq!(summary(&same).hits, SOURCES);
    assert_eq!(summary(&same).engine_build_id, "build-x");
    cleanup(&[&share, &a, &b, &c]);
}

#[test]
fn a_warm_local_cache_seeds_an_empty_shared_cache_once() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-seed-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-seed-a");
    write_project(&a);
    cook_with(&a, &SharedCacheChoice::Off);
    let seeded = cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    assert_eq!(seeded.imports_skipped as u64, SOURCES);
    assert_eq!(summary(&seeded).published, SOURCES);
    // The keys are remembered: the next cook neither asks nor writes.
    let before = snapshot(&share);
    let quiet = cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    assert_eq!(summary(&quiet).published, 0);
    assert_eq!(snapshot(&share), before);
    let b = checkout(&a, "shared-cook-seed-b");
    assert_eq!(
        summary(&cook_with(&b, &shared(&share, SharedCacheMode::Read))).hits,
        SOURCES
    );
    cleanup(&[&share, &a, &b]);
}

/// Sets the modification time of every file under `assets/` an hour back, so the next cook's
/// stamps are trusted (ADR-530: a file modified around its hash is read again).
fn age_assets(root: &Path) {
    let then = SystemTime::now() - std::time::Duration::from_secs(3600);
    for entry in std::fs::read_dir(root.join("assets")).expect("assets").flatten() {
        std::fs::File::options()
            .write(true)
            .open(entry.path())
            .expect("open")
            .set_modified(then)
            .expect("mtime");
    }
}

/// A file rewritten with its size and modification time kept fools the local stamps (the
/// ADR-530 blind spot, one machine's problem). It must not spread through the shared cache: the
/// seed is checked by a fresh read and refused, the next cook imports the real bytes and
/// publishes them under their own key, and a machine with the old bytes never gets the new
/// outputs.
#[test]
fn a_file_that_fools_the_stamps_is_never_published_under_a_stale_key() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-stale-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-stale-a");
    write_project(&a);
    cook_with(&a, &SharedCacheChoice::Off);
    // Another machine's checkout of the old bytes, with the `.meta` files the first cook wrote.
    let old = checkout(&a, "shared-cook-stale-old");
    age_assets(&a);
    // This cook stamps the aged files; the next one trusts the stamps.
    cook_with(&a, &SharedCacheChoice::Off);
    let quiet = cook_with(&a, &SharedCacheChoice::Off);
    assert_eq!((quiet.imports_skipped as u64, quiet.hashed_bytes), (SOURCES, 0));

    // One UV of the glTF's buffer changes (1.0 -> 0.5), same size, same mtime.
    let bin = a.join("assets/crate.bin");
    let mtime = std::fs::metadata(&bin).and_then(|m| m.modified()).expect("mtime");
    let mut bytes = std::fs::read(&bin).expect("read");
    bytes[112..116].copy_from_slice(&0.5f32.to_le_bytes());
    std::fs::write(&bin, &bytes).expect("write");
    std::fs::File::options()
        .write(true)
        .open(&bin)
        .expect("open")
        .set_modified(mtime)
        .expect("mtime");

    // The local skip trusts the stamp; the seed reads the files and refuses the glTF.
    let seeding = cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    assert_eq!(seeding.imports_skipped as u64, SOURCES);
    assert_eq!(summary(&seeding).published, SOURCES - 1);
    assert!(
        seeding
            .warnings
            .iter()
            .any(|w| w.contains("assets/crate.gltf") && w.contains("without a new size")),
        "{:?}",
        seeding.warnings
    );
    // The next cook imports the real bytes and publishes them.
    let fixed = cook_with(&a, &shared(&share, SharedCacheMode::ReadWrite));
    assert_eq!((fixed.cooked, summary(&fixed).published), (1, 1));

    // A machine with the new bytes gets them from the cache, byte for byte.
    let new = checkout(&a, "shared-cook-stale-new");
    let pulled = cook_with(&new, &shared(&share, SharedCacheMode::Read));
    assert_eq!((summary(&pulled).hits, pulled.imports_skipped), (SOURCES, 0));
    assert_eq!(cooked_outputs(&new), cooked_outputs(&a));
    // A machine with the old bytes misses on the glTF and imports it.
    let behind = cook_with(&old, &shared(&share, SharedCacheMode::Read));
    let s = summary(&behind);
    assert_eq!((s.hits, s.misses), (SOURCES - 1, 1));
    let fresh_old = checkout(&old, "shared-cook-stale-old-clean");
    cook_with(&fresh_old, &SharedCacheChoice::Off);
    assert_eq!(cooked_outputs(&old), cooked_outputs(&fresh_old));
    cleanup(&[&share, &a, &old, &new, &fresh_old]);
}

#[test]
fn the_project_manifest_turns_the_shared_cache_on() {
    let share = regia_test_utils::unique_temp_dir("shared-cook-toml-share");
    let a = regia_test_utils::unique_temp_dir("shared-cook-toml-a");
    write_project(&a);
    // Default off: no table, no summary.
    let report = cook_project(CookOptions::for_host(a.clone())).expect("cook");
    assert!(report.shared_cache.is_none());
    std::fs::write(
        a.join("Regia.toml"),
        format!(
            "[cook.shared_cache]\npath = {:?}\nmode = \"read_write\"\n",
            share.to_string_lossy()
        ),
    )
    .expect("manifest");
    let report = cook_project(CookOptions::for_host(a.clone())).expect("cook");
    assert_eq!(summary(&report).mode, "read_write");
    assert_eq!(summary(&report).published, SOURCES);
    // `Off` wins over the project.
    assert!(cook_with(&a, &SharedCacheChoice::Off).shared_cache.is_none());

    // A bad table fails the cook before anything is cooked.
    std::fs::write(
        a.join("Regia.toml"),
        "[cook.shared_cache]\nmode = \"sometimes\"\n",
    )
    .expect("manifest");
    let report = cook_project(CookOptions::for_host(a.clone())).expect("cook");
    assert!(
        report
            .errors
            .iter()
            .any(|(p, e)| p == "Regia.toml" && e.contains("mode"))
    );
    cleanup(&[&share, &a]);
}

#[test]
fn the_entry_decoder_never_panics_on_damaged_bytes() {
    let fixture = sample_entry();
    assert!(verify_entry(&fixture).is_ok());
    regia_test_utils::fuzz::fuzz_loader(&fixture, 0x2496, 2_000, verify_entry);
}

/// The loopback server's entries, by request target.
type Store = std::sync::Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>;

/// A minimal loopback cache server: `GET`, `HEAD`, `PUT`, `DELETE` on an in-memory map, one
/// request per connection (what `HttpBackend` sends). Returns the base URL and the store.
fn serve_cache() -> (String, Store) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let store = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
    let shared = store.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let store = shared.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let mut parts = line.split_whitespace();
                let (method, target) = (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                );
                let mut length = 0usize;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let (status, reply) = {
                    let mut map = store.lock().expect("store");
                    match method.as_str() {
                        "GET" => match map.get(&target) {
                            Some(b) => ("200 OK", b.clone()),
                            None => ("404 Not Found", Vec::new()),
                        },
                        "HEAD" => (
                            if map.contains_key(&target) {
                                "200 OK"
                            } else {
                                "404 Not Found"
                            },
                            Vec::new(),
                        ),
                        "PUT" => {
                            map.insert(target, body);
                            ("201 Created", Vec::new())
                        }
                        "DELETE" => {
                            map.remove(&target);
                            ("204 No Content", Vec::new())
                        }
                        _ => ("405 Method Not Allowed", Vec::new()),
                    }
                };
                let mut out = stream;
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = out.write_all(head.as_bytes());
                if method != "HEAD" {
                    let _ = out.write_all(&reply);
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}/cook-cache"), store)
}

#[test]
fn an_http_cache_shares_entries_and_survives_a_corrupt_one() {
    let (url, store) = serve_cache();
    let a = regia_test_utils::unique_temp_dir("shared-cook-http-a");
    write_project(&a);
    let first = cook_with(&a, &shared(Path::new(&url), SharedCacheMode::ReadWrite));
    assert_eq!(summary(&first).published, SOURCES);
    assert_eq!(store.lock().expect("store").len() as u64, SOURCES);
    assert!(
        store
            .lock()
            .expect("store")
            .keys()
            .all(|k| k.starts_with("/cook-cache/v1/") && k.ends_with(".rcc"))
    );

    let b = checkout(&a, "shared-cook-http-b");
    let report = cook_with(&b, &shared(Path::new(&url), SharedCacheMode::Read));
    assert_eq!((summary(&report).hits, summary(&report).misses), (SOURCES, 0));
    assert_eq!(cooked_outputs(&b), cooked_outputs(&a));

    // One stored body cut short: detected, imported, replaced by a read-write cook.
    {
        let mut map = store.lock().expect("store");
        let body = map.values_mut().next().expect("an entry");
        body.truncate(body.len() / 2);
    }
    let c = checkout(&a, "shared-cook-http-c");
    let report = cook_with(&c, &shared(Path::new(&url), SharedCacheMode::ReadWrite));
    let s = summary(&report);
    assert_eq!((s.hits, s.corrupt, s.published), (SOURCES - 1, 1, 1));
    assert!(
        store
            .lock()
            .expect("store")
            .values()
            .all(|b| verify_entry(b).is_ok())
    );
    assert_eq!(cooked_outputs(&c), cooked_outputs(&a));
    cleanup(&[&a, &b, &c]);
}

#[test]
fn an_unreachable_http_cache_costs_three_errors_then_the_cook_goes_on() {
    // A port with nothing listening: every request is refused at once.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let origin = regia_test_utils::unique_temp_dir("shared-cook-http-down-origin");
    write_project(&origin);
    cook_with(&origin, &SharedCacheChoice::Off);
    let b = checkout(&origin, "shared-cook-http-down-b");
    let url = format!("http://127.0.0.1:{port}/c");
    let report = cook_with(&b, &shared(Path::new(&url), SharedCacheMode::ReadWrite));
    let s = summary(&report);
    assert_eq!(s.errors, 3, "stops after three: {:?}", report.warnings);
    assert_eq!(s.hits, 0);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cooking the rest without it"))
    );
    assert_eq!(cooked_outputs(&b), cooked_outputs(&origin));
    cleanup(&[&origin, &b]);
}
