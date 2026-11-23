// SPDX-License-Identifier: MPL-2.0
//! The Android store chunk source on any host (spec 40 § 2.6, ADR-1876): a scripted store stands in
//! for the store's asset-pack API, and the manager is stepped through pending, downloading, the
//! confirmation dialog, completion (the pak is linked into the install directory and verified),
//! pause (the store download is cancelled), and an unmapped status. The device half (a real AAB
//! through the store's local testing mode) is spec § 8 tests 21-22, run by the lead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use regia_asset::content::{
    ChunkEntry, ChunkState, ContentManager, ContentManifest, Delivery, FailReason, PackStore, RequestOptions,
    StorePackSource, hash_file, pack_name,
};
use regia_platform::asset_packs::{PackState, status};

/// A store whose pack states the test sets; counts fetch, cancel and confirm calls.
#[derive(Default)]
struct ScriptedStore {
    states: Mutex<BTreeMap<String, PackState>>,
    locations: Mutex<BTreeMap<String, PathBuf>>,
    calls: Mutex<Vec<String>>,
}

impl ScriptedStore {
    fn set(&self, pack: &str, status: i32, done: u64, total: u64) {
        self.states.lock().insert(
            pack.to_owned(),
            PackState {
                status,
                error_code: 0,
                bytes_downloaded: done,
                total_bytes: total,
            },
        );
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

impl PackStore for ScriptedStore {
    fn state(&self, pack: &str) -> Result<Option<PackState>, String> {
        Ok(self.states.lock().get(pack).copied())
    }

    fn fetch(&self, pack: &str) -> Result<(), String> {
        self.calls.lock().push(format!("fetch {pack}"));
        Ok(())
    }

    fn cancel(&self, pack: &str) {
        self.calls.lock().push(format!("cancel {pack}"));
    }

    fn remove(&self, pack: &str) {
        self.calls.lock().push(format!("remove {pack}"));
    }

    fn confirm(&self) {
        self.calls.lock().push("confirm".into());
    }

    fn location(&self, pack: &str) -> Result<Option<PathBuf>, String> {
        Ok(self.locations.lock().get(pack).cloned())
    }
}

/// `content/` holds base; the store's pack directory holds `open-world.fpak` (pack `open_world`).
fn fixture(tag: &str) -> (PathBuf, ContentManifest, Vec<u8>) {
    let root = regia_test_utils::unique_temp_dir(tag);
    let content = root.join("content");
    let packs = root.join("store/open_world/assets");
    std::fs::create_dir_all(&content).expect("mkdir");
    std::fs::create_dir_all(&packs).expect("mkdir");
    std::fs::write(content.join("base.fpak"), b"base").expect("base");
    let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(packs.join("open-world.fpak"), &bytes).expect("pack file");
    let entry = |name: &str, dir: &Path, delivery| {
        let file = format!("{name}.fpak");
        ChunkEntry {
            name: name.into(),
            size: std::fs::metadata(dir.join(&file)).expect("file").len(),
            hash: hash_file(&dir.join(&file)).expect("hash"),
            file,
            delivery,
            requires: Vec::new(),
            overrides: false,
            mount_order: 0,
            assets: 1,
            blocks: Vec::new(),
        }
    };
    let mut manifest = ContentManifest::new("1.0-1-store", "android");
    manifest.chunks.push(entry("base", &content, Delivery::Install));
    manifest
        .chunks
        .push(entry("open-world", &packs, Delivery::OnDemand));
    (root, manifest, bytes)
}

#[test]
fn store_pack_downloads_confirms_links_and_verifies() {
    let (root, manifest, bytes) = fixture("content-store");
    let store = Arc::new(ScriptedStore::default());
    let pack = pack_name("open-world");
    assert_eq!(pack, "open_world");
    let mut m = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install"));
    m.add_source(Box::new(StorePackSource::new(store.clone())));
    assert_eq!(m.source_names()[0], "android_pack");
    assert_eq!(m.state("open-world"), Some(ChunkState::NotInstalled));

    let req = m
        .request(&["open-world"], RequestOptions::default())
        .expect("request");
    m.advance(0, None);
    assert_eq!(store.calls(), ["fetch open_world"]);
    assert!(matches!(
        m.state("open-world"),
        Some(ChunkState::Downloading { done: 0, .. })
    ));

    // The store counts its own (compressed) bytes; progress is scaled to the chunk file size.
    store.set(&pack, status::DOWNLOADING, 500, 1_000);
    m.advance(0, None);
    assert_eq!(
        m.state("open-world"),
        Some(ChunkState::Downloading {
            done: bytes.len() as u64 / 2,
            total: bytes.len() as u64
        })
    );

    // Waiting for Wi-Fi: the store's confirmation dialog is shown once, however many steps pass.
    store.set(&pack, status::WAITING_FOR_WIFI, 500, 1_000);
    m.advance(0, None);
    m.advance(0, None);
    assert_eq!(store.calls(), ["fetch open_world", "confirm"]);

    // Completed: the pak is taken from the store's location, verified and installed.
    store
        .locations
        .lock()
        .insert(pack.clone(), root.join("store/open_world/assets"));
    store.set(&pack, status::COMPLETED, 1_000, 1_000);
    m.advance(0, None);
    assert_eq!(m.state("open-world"), Some(ChunkState::Installed));
    assert_eq!(m.progress(req).expect("progress").state, "Installed");
    assert_eq!(
        std::fs::read(root.join("install/open-world.fpak")).expect("installed"),
        bytes
    );
    assert!(
        !store.calls().iter().any(|c| c.starts_with("cancel")),
        "a finished transfer does not cancel the store download"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn store_pack_already_on_the_device_is_present() {
    let (root, manifest, _) = fixture("content-store-present");
    let store = Arc::new(ScriptedStore::default());
    store.set("open_world", status::COMPLETED, 1, 1);
    store
        .locations
        .lock()
        .insert("open_world".into(), root.join("store/open_world/assets"));
    let mut m = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install"));
    m.add_source(Box::new(StorePackSource::new(store.clone())));
    m.request(&["open-world"], RequestOptions::default())
        .expect("request");
    m.advance(0, None);
    assert_eq!(m.state("open-world"), Some(ChunkState::Installed));
    assert_eq!(
        m.chunk_path("open-world"),
        Some(root.join("store/open_world/assets/open-world.fpak").as_path()),
        "mounted from the store's own files, no copy"
    );
    assert!(store.calls().is_empty(), "nothing fetched");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pause_cancels_the_store_download_and_unknown_statuses_fail() {
    let (root, manifest, _) = fixture("content-store-pause");
    let store = Arc::new(ScriptedStore::default());
    let mut m = ContentManager::from_manifest(manifest.clone(), &root.join("content"), &root.join("install"));
    m.add_source(Box::new(StorePackSource::new(store.clone())));
    let req = m
        .request(&["open-world"], RequestOptions::default())
        .expect("request");
    m.advance(0, None);
    store.set("open_world", status::DOWNLOADING, 10, 100);
    m.advance(0, None);
    m.pause(req).expect("pause");
    assert_eq!(store.calls(), ["fetch open_world", "cancel open_world"]);
    m.resume(req).expect("resume");
    m.advance(0, None);
    assert_eq!(
        store.calls().last().map(String::as_str),
        Some("fetch open_world"),
        "resume asks the store again"
    );

    // A status this build does not map fails the chunk with store_unknown_status.
    store.set("open_world", 77, 10, 100);
    m.advance(0, None);
    assert_eq!(
        m.state("open-world"),
        Some(ChunkState::Failed(FailReason::StoreUnknownStatus))
    );
    assert_eq!(
        m.state("open-world").map(|s| s.label()).as_deref(),
        Some("Failed(store_unknown_status)")
    );

    // Install chunks never come from the store, and without a store (desktop) packs are missing.
    let mut desktop = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install2"));
    desktop.add_source(Box::new(StorePackSource::platform()));
    desktop
        .request(&["open-world"], RequestOptions::default())
        .expect("request");
    desktop.advance(0, None);
    if cfg!(not(target_os = "android")) {
        assert!(matches!(
            desktop.state("open-world"),
            Some(ChunkState::Failed(FailReason::SourceUnreachable(_)))
        ));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// `content.release` frees the store's copy too, and a pack that was used in place leaves nothing in
/// the install directory to delete. Install chunks never reach the store.
#[test]
fn release_removes_the_pack_from_the_store() {
    let (root, manifest, _) = fixture("content-store-release");
    let store = Arc::new(ScriptedStore::default());
    store.set("open_world", status::COMPLETED, 1, 1);
    store
        .locations
        .lock()
        .insert("open_world".into(), root.join("store/open_world/assets"));
    let mut m = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install"));
    m.add_source(Box::new(StorePackSource::new(store.clone())));
    m.request(&["open-world"], RequestOptions::default())
        .expect("request");
    m.advance(0, None);
    assert_eq!(m.state("open-world"), Some(ChunkState::Installed));
    assert_eq!(m.release(&["open-world"], &[]).expect("release"), ["open-world"]);
    assert_eq!(store.calls(), ["remove open_world"]);
    assert_eq!(m.state("open-world"), Some(ChunkState::NotInstalled));
    assert!(
        root.join("store/open_world/assets/open-world.fpak").is_file(),
        "the store owns its files"
    );
    assert!(m.release(&["base"], &[]).is_err());
    assert_eq!(store.calls(), ["remove open_world"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_store_pack_that_fails_its_hash_is_not_used() {
    let (root, manifest, _) = fixture("content-store-corrupt");
    let store = Arc::new(ScriptedStore::default());
    store.set("open_world", status::COMPLETED, 1, 1);
    let dir = root.join("store/open_world/assets");
    store.locations.lock().insert("open_world".into(), dir.clone());
    std::fs::write(dir.join("open-world.fpak"), b"not the pack").expect("corrupt");
    let mut m = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install"));
    m.add_source(Box::new(StorePackSource::new(store.clone())));
    m.request(&["open-world"], RequestOptions::default())
        .expect("request");
    m.advance(0, None);
    assert_eq!(
        m.state("open-world"),
        Some(ChunkState::Failed(FailReason::HashMismatch))
    );
    assert!(m.chunk_path("open-world").is_none());
    assert_eq!(
        store.calls(),
        ["remove open_world"],
        "a re-request fetches the pack instead of hashing the same file again"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_store_pack_is_hashed_once_per_file_stamp() {
    let (root, manifest, _) = fixture("content-store-stamp");
    let store = Arc::new(ScriptedStore::default());
    store.set("open_world", status::COMPLETED, 1, 1);
    store
        .locations
        .lock()
        .insert("open_world".into(), root.join("store/open_world/assets"));
    let jobs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for expected in [1, 1] {
        let mut m =
            ContentManager::from_manifest(manifest.clone(), &root.join("content"), &root.join("install"));
        m.add_source(Box::new(StorePackSource::new(store.clone())));
        let counter = Arc::clone(&jobs);
        m.set_verify_runner(Some(Arc::new(move |job| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            job();
        })));
        m.request(&["open-world"], RequestOptions::default())
            .expect("request");
        m.advance(0, None);
        m.wait_verifications(None);
        assert_eq!(m.state("open-world"), Some(ChunkState::Installed));
        assert_eq!(jobs.load(std::sync::atomic::Ordering::Relaxed), expected);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The store keeps packs on its own storage, so a full install volume does not stop a request.
#[test]
fn store_transfers_skip_the_install_space_check() {
    let (root, manifest, _) = fixture("content-store-space");
    let store = Arc::new(ScriptedStore::default());
    let mut m = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install"));
    m.add_source(Box::new(StorePackSource::new(store.clone())));
    m.set_space_probe(Arc::new(|_| Some(0)));
    m.request(&["open-world"], RequestOptions::default())
        .expect("request");
    m.advance(0, None);
    assert!(matches!(
        m.state("open-world"),
        Some(ChunkState::Downloading { .. })
    ));
    assert_eq!(store.calls(), ["fetch open_world"]);
    let _ = std::fs::remove_dir_all(&root);
}
