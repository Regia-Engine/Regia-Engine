// SPDX-License-Identifier: MPL-2.0
//! Content chunks at run time (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 8, tests 9-18):
//! manifest and chunk-map formats, mount order, overrides, the request state machine stepped by
//! byte budgets (no sleeps), verification, faults, tiers, unavailable assets, unmount and updates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_asset::content::{
    BASE_CHUNK, ChunkEntry, ChunkMap, ChunkState, ContentError, ContentFault, ContentManager,
    ContentManifest, ContentState, Delivery, DirSource, FailReason, InstalledChunk, RequestOptions, Tier,
    hash_file, mount_order, pack_name, validate_chunk_name,
};
use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh,
    FPAK_BLOB_ALIGNMENT, FPAK_VERSION, LoadItem, LoadKind, LoadSet, LoadSets, LoadState, ManifestEntry,
    PakEntry, PakToc, encode_cooked_file, encode_pak_header, pack_importer_version,
};

fn mesh_file(id: AssetId, triangles: u32) -> Vec<u8> {
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for t in 0..triangles {
        let base = positions.len() as u32;
        let z = t as f32;
        positions.extend_from_slice(&[[0.0, 0.0, z], [1.0, 0.0, z], [0.0, 1.0, z]]);
        indices.extend_from_slice(&[base, base + 1, base + 2]);
    }
    let n = positions.len();
    let mesh = CpuMesh {
        positions,
        normals: vec![[0.0, 0.0, 1.0]; n],
        uvs: vec![[0.0, 0.0]; n],
        tangents: Vec::new(),
        indices,
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
    encode_cooked_file(&header, &payload).expect("encode")
}

/// An uncompressed pak of `entries` (id, path, cooked bytes).
fn write_pak(path: &Path, entries: &[(AssetId, &str, Vec<u8>)]) {
    write_pak_with(path, entries, |_| {});
}

/// [`write_pak`] with `tweak` applied to every TOC entry (to write a corrupt one).
fn write_pak_with(path: &Path, entries: &[(AssetId, &str, Vec<u8>)], tweak: impl Fn(&mut PakEntry)) {
    let mut data = vec![0u8; 60];
    let mut toc_entries = Vec::new();
    let mut manifest = Vec::new();
    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by_key(|(id, _, _)| *id);
    for (id, asset_path, payload) in sorted {
        let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
        if rem != 0 {
            data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
        }
        let offset = data.len() as u64;
        data.extend_from_slice(payload);
        toc_entries.push(PakEntry {
            id: *id,
            type_tag: regia_core::id::TypePathHash::of("regia_asset::CpuMesh"),
            offset,
            size: payload.len() as u64,
            compressed_size: payload.len() as u64,
            block_table_offset: 0,
        });
        manifest.push(ManifestEntry {
            path: (*asset_path).to_owned(),
            id: *id,
            type_path: "regia_asset::CpuMesh".into(),
        });
    }
    let toc_offset = data.len() as u64;
    toc_entries.iter_mut().for_each(&tweak);
    let toc = PakToc {
        entries: toc_entries,
        manifest,
        dependencies: Vec::new(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
    data[..60].copy_from_slice(&header);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, data).expect("write pak");
}

fn entry(dir: &Path, name: &str, delivery: Delivery, order: u32, requires: &[&str]) -> ChunkEntry {
    let file = format!("{name}.fpak");
    let path = dir.join(&file);
    ChunkEntry {
        name: name.to_owned(),
        size: std::fs::metadata(&path).expect("pak exists").len(),
        hash: hash_file(&path).expect("hash"),
        file,
        delivery,
        requires: requires.iter().map(|s| (*s).to_owned()).collect(),
        overrides: false,
        mount_order: order,
        assets: 1,
        blocks: Vec::new(),
    }
}

/// A project: `content/` (package, holds base), `cdn/` (holds the other chunks), `install/`.
struct Fixture {
    root: PathBuf,
    content: PathBuf,
    cdn: PathBuf,
    install: PathBuf,
    base_id: AssetId,
    ids: BTreeMap<String, AssetId>,
}

impl Fixture {
    /// `chunks`: (name, requires, padding bytes so transfers take several steps).
    fn new(tag: &str, chunks: &[(&str, &[&str], usize)]) -> (Self, ContentManifest) {
        let root = regia_test_utils::unique_temp_dir(tag);
        let content = root.join("content");
        let cdn = root.join("cdn");
        let install = root.join("install");
        let base_id = AssetId::new_random();
        write_pak(
            &content.join("base.fpak"),
            &[(base_id, "assets/base.mesh", mesh_file(base_id, 1))],
        );
        let mut manifest = ContentManifest::new("1.0-1-aaaa", "linux");
        manifest
            .chunks
            .push(entry(&content, BASE_CHUNK, Delivery::Install, 0, &[]));
        let mut ids = BTreeMap::new();
        for (i, (name, requires, pad)) in chunks.iter().enumerate() {
            let id = AssetId::new_random();
            let mut file = mesh_file(id, 2);
            // Pad with a second dummy entry so the pak is `pad` bytes larger.
            let filler = AssetId::new_random();
            let mut entries = vec![(id, format!("assets/{name}/a.mesh"), std::mem::take(&mut file))];
            if *pad > 0 {
                entries.push((filler, format!("assets/{name}/pad.bin"), vec![7u8; *pad]));
            }
            let refs: Vec<(AssetId, &str, Vec<u8>)> = entries
                .iter()
                .map(|(id, p, b)| (*id, p.as_str(), b.clone()))
                .collect();
            write_pak(&cdn.join(format!("{name}.fpak")), &refs);
            manifest
                .chunks
                .push(entry(&cdn, name, Delivery::OnDemand, (i + 1) as u32, requires));
            ids.insert((*name).to_owned(), id);
        }
        manifest.write_to_dir(&content).expect("manifest");
        let mut map = BTreeMap::new();
        map.insert(base_id, BASE_CHUNK.to_owned());
        for (name, id) in &ids {
            map.insert(*id, name.clone());
        }
        ChunkMap::from_assignment(&map)
            .write_to_dir(&content)
            .expect("chunk map");
        (
            Self {
                root,
                content,
                cdn,
                install,
                base_id,
                ids,
            },
            manifest,
        )
    }

    fn manager(&self, manifest: &ContentManifest) -> ContentManager {
        let map = ChunkMap::read_from_dir(&self.content)
            .expect("map")
            .expect("present");
        let mut m =
            ContentManager::from_manifest(manifest.clone(), &self.content, &self.install).with_chunk_map(map);
        m.add_source(Box::new(DirSource::new(&self.cdn)));
        m
    }

    fn server(&self) -> AssetServer {
        AssetServer::open(AssetServerSettings {
            project_root: self.root.clone(),
            platform: "linux".into(),
            content_dir: Some(self.content.clone()),
        })
        .expect("server")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run(m: &mut ContentManager, budget: u64, steps: usize, mut server: Option<&mut AssetServer>) {
    for _ in 0..steps {
        m.advance(budget, server.as_deref_mut());
    }
}

#[test]
fn manifest_round_trips_and_refuses_newer_versions() {
    let (fx, manifest) = Fixture::new("content-manifest-rt", &[("arena", &[], 0)]);
    let text = manifest.to_ron_string().expect("ron");
    assert!(text.starts_with("ContentManifest("), "{text}");
    assert_eq!(ContentManifest::from_ron_str(&text).expect("parse"), manifest);
    let newer = text.replace("format_version: 1", "format_version: 2");
    let err = ContentManifest::from_ron_str(&newer).expect_err("newer");
    assert_eq!(err.reason(), "manifest_version");
    let bad_file = text.replace("file: \"arena.fpak\"", "file: \"../arena.fpak\"");
    assert!(ContentManifest::from_ron_str(&bad_file).is_err());
    assert_eq!(
        ContentManifest::read_from_dir(&fx.root).expect("absent is fine"),
        None,
        "no manifest is not an error"
    );
}

#[test]
fn chunk_names_and_pack_names() {
    for good in ["base", "arena", "dlc-2", "a_b_c1"] {
        assert!(validate_chunk_name(good).is_ok(), "{good}");
    }
    for bad in ["", "Arena", "1st", "_x", "a/b", "a.b", &"x".repeat(41)] {
        assert!(validate_chunk_name(bad).is_err(), "{bad}");
    }
    assert_eq!(pack_name("dlc-2"), "dlc_2");
}

#[test]
fn mount_order_is_topological_base_first_ties_by_name() {
    let mut chunks = BTreeMap::new();
    chunks.insert("base".to_owned(), vec![]);
    chunks.insert("zeta".to_owned(), vec![]);
    chunks.insert("alpha".to_owned(), vec!["zeta".to_owned()]);
    chunks.insert("mid".to_owned(), vec!["base@1.0".to_owned()]);
    let order = mount_order(&chunks).expect("acyclic");
    let mut by_pos: Vec<_> = order.iter().map(|(n, p)| (*p, n.clone())).collect();
    by_pos.sort();
    let names: Vec<_> = by_pos.into_iter().map(|(_, n)| n).collect();
    assert_eq!(names, ["base", "mid", "zeta", "alpha"]);
    chunks.insert("zeta".to_owned(), vec!["alpha".to_owned()]);
    let err = mount_order(&chunks).expect_err("cycle");
    assert_eq!(err.reason(), "chunk_cycle");
}

#[test]
fn chunk_map_round_trips_and_answers_chunk_of() {
    let a = AssetId::new_random();
    let b = AssetId::new_random();
    let mut map = BTreeMap::new();
    map.insert(a, "arena".to_owned());
    map.insert(b, BASE_CHUNK.to_owned());
    let cm = ChunkMap::from_assignment(&map);
    let back = ChunkMap::from_bytes(&cm.to_bytes().expect("bytes")).expect("decode");
    assert_eq!(back, cm);
    assert_eq!(back.chunk_of(a), Some("arena"));
    assert_eq!(back.chunk_of(b), Some("base"));
    assert_eq!(back.chunk_of(AssetId::new_random()), None);
    let mut newer = cm.to_bytes().expect("bytes");
    newer[8] = 9;
    assert_eq!(
        ChunkMap::from_bytes(&newer).expect_err("newer").reason(),
        "manifest_version"
    );
}

/// Test 9: listed chunks mount in manifest order, not file-name order; an unlisted pak mounts after
/// them (backward compatible); without a manifest the old alphabetical order holds.
#[test]
fn startup_mount_order_follows_the_manifest() {
    let root = regia_test_utils::unique_temp_dir("content-mount-order");
    let content = root.join("content");
    for name in ["base", "zpatch", "aaa-dlc", "extra"] {
        let id = AssetId::new_random();
        write_pak(
            &content.join(format!("{name}.fpak")),
            &[(id, "assets/x.mesh", mesh_file(id, 1))],
        );
    }
    let open = || {
        AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: Some(content.clone()),
        })
        .expect("server")
    };
    assert_eq!(open().mounted_pak_names(), ["aaa-dlc", "base", "extra", "zpatch"]);
    let mut manifest = ContentManifest::new("b1", "linux");
    manifest
        .chunks
        .push(entry(&content, "base", Delivery::Install, 0, &[]));
    manifest
        .chunks
        .push(entry(&content, "zpatch", Delivery::External, 1, &[]));
    manifest
        .chunks
        .push(entry(&content, "aaa-dlc", Delivery::External, 2, &["zpatch"]));
    manifest.write_to_dir(&content).expect("manifest");
    assert_eq!(open().mounted_pak_names(), ["base", "zpatch", "aaa-dlc", "extra"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Test 10: a patch chunk replaces an id; the live handle reloads in place and later loads read
/// the patch.
#[test]
fn override_chunk_replaces_a_live_asset() {
    let (fx, mut manifest) = Fixture::new("content-override", &[]);
    let id = fx.base_id;
    write_pak(
        &fx.install.join("patch.fpak"),
        &[(id, "assets/base.mesh", mesh_file(id, 3))],
    );
    let mut patch = entry(&fx.install, "patch", Delivery::External, 1, &[]);
    patch.overrides = true;
    manifest.chunks.push(patch);
    let mut server = fx.server();
    let handle = server.load_mesh(id).expect("base mesh");
    assert_eq!(server.meshes().get(&handle).expect("live").indices.len(), 3);
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    assert_eq!(m.state("patch"), Some(ChunkState::Installed));
    m.sync_with_server(&mut server, false);
    assert_eq!(m.state(BASE_CHUNK), Some(ChunkState::Mounted));
    m.mount(&mut server, "patch").expect("mount");
    assert_eq!(server.mounted_pak_names(), ["base", "patch"]);
    assert_eq!(
        server.meshes().get(&handle).expect("live").indices.len(),
        9,
        "handle reloaded"
    );
    assert_eq!(server.generation(id), 1);
    let events = m.drain_events();
    assert!(
        events
            .iter()
            .any(|e| e.chunk == "patch" && e.to == ChunkState::Mounted)
    );
}

/// Test 11: the legal path NotInstalled → Queued → Downloading → Verifying → Installed → Mounted
/// and the illegal calls.
#[test]
fn request_state_machine_legal_and_illegal_calls() {
    let (fx, manifest) = Fixture::new("content-states", &[("arena", &[], 20_000)]);
    let mut server = fx.server();
    let mut m = fx.manager(&manifest);
    assert_eq!(m.state("arena"), Some(ChunkState::NotInstalled));
    assert_eq!(
        m.mount(&mut server, "arena").expect_err("not installed").reason(),
        "chunk_not_installed"
    );
    assert_eq!(
        m.request(&["nope"], RequestOptions::default())
            .expect_err("unknown")
            .reason(),
        "chunk_not_found"
    );
    let req = m
        .request(
            &["arena"],
            RequestOptions {
                mount_when_done: true,
                ..RequestOptions::default()
            },
        )
        .expect("request");
    assert_eq!(m.state("arena"), Some(ChunkState::Queued));
    assert_eq!(
        m.resume(req).expect_err("not paused").reason(),
        "request_not_paused"
    );
    m.advance(4096, Some(&mut server));
    assert!(matches!(
        m.state("arena"),
        Some(ChunkState::Downloading { done: 4096, .. })
    ));
    m.pause(req).expect("pause");
    assert_eq!(m.state("arena"), Some(ChunkState::Paused));
    run(&mut m, 4096, 3, Some(&mut server));
    assert_eq!(m.state("arena"), Some(ChunkState::Paused), "paused does not move");
    m.resume(req).expect("resume");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    assert_eq!(m.state("arena"), Some(ChunkState::Mounted));
    let p = m.progress(req).expect("progress");
    assert_eq!(p.state, "Mounted");
    assert_eq!(p.done, p.total);
    assert_eq!(m.pause(req).expect_err("finished").reason(), "request_finished");
    let labels: Vec<String> = m.drain_events().iter().map(|e| e.to.label()).collect();
    assert_eq!(
        labels,
        [
            "Queued",
            "Downloading",
            "Paused",
            "Queued",
            "Downloading",
            "Verifying",
            "Installed",
            "Mounted"
        ]
    );
    // The fetched file and the install record are in the install directory.
    assert!(fx.install.join("arena.fpak").is_file());
    let record = ContentState::read_from_dir(&fx.install);
    assert_eq!(
        record.chunks["arena"].hash,
        manifest.chunk("arena").expect("chunk").hash
    );
    assert!(
        m.request(&["arena"], RequestOptions::default()).is_ok(),
        "present chunks count as done"
    );
}

/// Test 12: requirements first, repeated names collapsed, summed progress; a cancelled request
/// keeps its `.part` and a new request fetches only the missing tail.
#[test]
fn requests_collapse_include_requirements_and_resume_after_cancel() {
    let (fx, manifest) = Fixture::new(
        "content-resume",
        &[("core", &[], 30_000), ("arena", &["core"], 50_000)],
    );
    let source = DirSource::new(&fx.cdn);
    let mut m = ContentManager::from_manifest(manifest.clone(), &fx.content, &fx.install);
    m.add_source(Box::new(source.clone()));
    m.set_max_concurrent(1);
    let req = m
        .request(&["arena", "arena", "core"], RequestOptions::default())
        .expect("request");
    let p = m.progress(req).expect("progress");
    let names: Vec<&str> = p.chunks.iter().map(|(n, ..)| n.as_str()).collect();
    assert_eq!(names, ["core", "arena"], "requirements first, collapsed");
    let total = manifest.chunk("core").expect("c").size + manifest.chunk("arena").expect("a").size;
    assert_eq!(p.total, total);
    run(&mut m, 10_000, 2, None);
    assert_eq!(m.progress(req).expect("p").done, 20_000);
    m.cancel(req).expect("cancel");
    assert_eq!(m.state("core"), Some(ChunkState::Cancelled));
    assert!(fx.install.join("core.fpak.part").is_file(), "partial data kept");
    assert_eq!(source.served(), 20_000);
    let again = m.request(&["arena"], RequestOptions::default()).expect("again");
    run(&mut m, 1 << 20, 6, None);
    assert_eq!(m.progress(again).expect("p").state, "Installed");
    assert_eq!(
        source.served(),
        total,
        "the second transfer fetched only the missing tail"
    );
}

/// Test 13: a flipped byte fails verification, deletes the file, leaves other chunks alone.
#[test]
fn corrupt_chunk_fails_verification_and_is_deleted() {
    let (fx, manifest) = Fixture::new("content-corrupt", &[("arena", &[], 0), ("forest", &[], 0)]);
    let mut m = fx.manager(&manifest);
    m.inject("arena", ContentFault::Corrupt, 0).expect("inject");
    let req = m
        .request(&["arena", "forest"], RequestOptions::default())
        .expect("request");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(
        m.state("arena"),
        Some(ChunkState::Failed(FailReason::HashMismatch))
    );
    assert_eq!(m.state("forest"), Some(ChunkState::Installed));
    assert!(!fx.install.join("arena.fpak").exists());
    assert!(!fx.install.join("arena.fpak.part").exists());
    assert_eq!(m.progress(req).expect("p").state, "Failed(hash_mismatch)");
}

/// Test 14: injected no-space fails before writing; metered waits for approval.
#[test]
fn no_space_and_metered_approval() {
    let (fx, manifest) = Fixture::new("content-metered", &[("a", &[], 0), ("b", &[], 0), ("c", &[], 0)]);
    let mut m = fx.manager(&manifest);
    m.inject("a", ContentFault::NoSpace, 0).expect("inject");
    m.request(&["a"], RequestOptions::default()).expect("a");
    m.advance(1 << 20, None);
    assert_eq!(m.state("a"), Some(ChunkState::Failed(FailReason::NoSpace)));
    assert!(!fx.install.join("a.fpak.part").exists(), "nothing written");

    m.set_metered(true);
    let rb = m.request(&["b"], RequestOptions::default()).expect("b");
    let rc = m.request(&["c"], RequestOptions::default()).expect("c");
    run(&mut m, 1 << 20, 2, None);
    assert_eq!(m.state("b"), Some(ChunkState::WaitingForUnmetered));
    assert_eq!(m.progress(rb).expect("p").state, "WaitingForUnmetered");
    m.approve_metered(rb).expect("approve");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("b"), Some(ChunkState::Installed));
    assert_eq!(m.state("c"), Some(ChunkState::WaitingForUnmetered));
    m.approve_all_metered();
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.progress(rc).expect("p").state, "Installed");
}

/// The real free-space check (spec § 2.5): a chunk needs its missing bytes plus 5 percent on top of
/// what running transfers still have to write, and fails before any byte is written.
#[test]
fn free_space_check_counts_missing_bytes_and_running_transfers() {
    let (fx, manifest) = Fixture::new(
        "content-space",
        &[("a", &[], 40_000), ("b", &[], 40_000), ("c", &[], 40_000)],
    );
    let size = manifest.chunk("a").expect("a").size;
    let need = size + size.div_ceil(20);

    // One byte short of size + 5 percent.
    let mut m = fx.manager(&manifest);
    m.set_space_probe(Arc::new(move |_| Some(need - 1)));
    m.request(&["a"], RequestOptions::default()).expect("a");
    m.advance(1 << 20, None);
    assert_eq!(m.state("a"), Some(ChunkState::Failed(FailReason::NoSpace)));
    assert!(!fx.install.join("a.fpak.part").exists(), "nothing written");

    // Exactly enough; a retry after the failure goes through.
    m.set_space_probe(Arc::new(move |_| Some(need)));
    m.request(&["a"], RequestOptions::default()).expect("again");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));

    // Partial data counts: only the tail is missing.
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    let bytes = std::fs::read(fx.cdn.join("b.fpak")).expect("b");
    std::fs::write(fx.install.join("b.fpak.part"), &bytes[..bytes.len() - 1_000]).expect("part");
    m.set_space_probe(Arc::new(|_| Some(1_050)));
    m.request(&["b"], RequestOptions::default()).expect("b");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("b"), Some(ChunkState::Installed));

    // Two transfers that each fit but not together: the second one fails.
    let (fx2, manifest2) = Fixture::new("content-space2", &[("a", &[], 40_000), ("b", &[], 40_000)]);
    let mut m = fx2.manager(&manifest2);
    // Room for one chunk with its margin, and half of another.
    let room = need + need / 2;
    m.set_space_probe(Arc::new(move |_| Some(room)));
    m.request(&["a", "b"], RequestOptions::default()).expect("both");
    m.advance(1_000, None);
    assert!(matches!(m.state("a"), Some(ChunkState::Downloading { .. })));
    assert_eq!(m.state("b"), Some(ChunkState::Failed(FailReason::NoSpace)));

    // A platform that cannot tell skips the check.
    let (fx3, manifest3) = Fixture::new("content-space3", &[("a", &[], 0)]);
    let mut m = fx3.manager(&manifest3);
    m.set_space_probe(Arc::new(|_| None));
    m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
}

type Jobs = Arc<parking_lot::Mutex<Vec<Box<dyn FnOnce() + Send>>>>;

/// A verify runner that holds jobs until the test runs them.
fn held_runner() -> (Jobs, regia_asset::content::VerifyRunner) {
    let jobs = Jobs::default();
    let queue = Arc::clone(&jobs);
    (jobs, Arc::new(move |job| queue.lock().push(job)))
}

fn run_held(jobs: &Jobs) {
    let held = std::mem::take(&mut *jobs.lock());
    for job in held {
        job();
    }
}

/// Audit v0.2.12 (content M5): off Android no store fetches `fast_follow` chunks after install,
/// and nothing asked for them, so they stayed `NotInstalled` until the game requested them.
#[test]
fn fast_follow_chunks_are_requested_and_mounted_without_a_store() {
    let (fx, mut manifest) = Fixture::new("content-fast-follow", &[("intro", &[], 0), ("late", &[], 0)]);
    for c in &mut manifest.chunks {
        if c.name == "intro" {
            c.delivery = Delivery::FastFollow;
        }
    }
    let mut server = fx.server();
    let mut m = fx.manager(&manifest);
    let id = m.request_fast_follow().expect("request").expect("one to fetch");
    run(&mut m, 1 << 20, 4, Some(&mut server));
    assert_eq!(m.progress(id).expect("progress").state, "Mounted");
    assert_eq!(m.state("intro"), Some(ChunkState::Mounted));
    assert_eq!(m.state("late"), Some(ChunkState::NotInstalled), "on-demand waits");
    assert!(
        m.request_fast_follow().expect("again").is_none(),
        "nothing left to fetch"
    );
}

/// An unmount sticks: a finished `mount_when_done` request does not mount the chunk again on the next
/// step (found on a phone, where it made `release` answer `chunk_in_use`). A new request does.
#[test]
fn unmount_sticks_after_a_mount_when_done_request() {
    let (fx, manifest) = Fixture::new("content-unmount-sticks", &[("a", &[], 0)]);
    let mut m = fx.manager(&manifest);
    let mut server = fx.server();
    let mount = RequestOptions {
        mount_when_done: true,
        ..RequestOptions::default()
    };
    let first = m.request(&["a"], mount).expect("a");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    assert_eq!(m.state("a"), Some(ChunkState::Mounted));

    m.unmount(&mut server, None, "a", false).expect("unmount");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    assert_eq!(m.state("a"), Some(ChunkState::Installed), "stays unmounted");
    assert_eq!(m.progress(first).expect("progress").state, "Installed");
    assert!(
        matches!(m.pause(first), Err(ContentError::RequestFinished(_))),
        "the request is finished, not waiting to mount"
    );

    m.request(&["a"], mount).expect("again");
    run(&mut m, 1 << 20, 2, Some(&mut server));
    assert_eq!(m.state("a"), Some(ChunkState::Mounted), "a new request mounts it");

    m.unmount(&mut server, None, "a", false).expect("unmount");
    run(&mut m, 1 << 20, 2, Some(&mut server));
    assert_eq!(m.release(&["a"], &[]).expect("release"), ["a"]);
    assert_eq!(m.state("a"), Some(ChunkState::NotInstalled));
}

/// Android layout: the install directory lies inside the content directory. A fetched chunk is not
/// a packaged one, so `release` deletes it (found on a phone: it answered `chunk_in_use`).
#[test]
fn release_works_when_the_install_dir_is_inside_the_content_dir() {
    let (fx, manifest) = Fixture::new("content-nested-install", &[("a", &[], 0)]);
    let install = fx.content.join("downloads");
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &install);
    m.add_source(Box::new(DirSource::new(&fx.cdn)));
    m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
    assert_eq!(m.release(&["a"], &[]).expect("release"), ["a"]);
    assert!(!install.join("a.fpak").exists());
    assert_eq!(m.state("a"), Some(ChunkState::NotInstalled));
    assert_eq!(
        m.release(&["base"], &[]).expect_err("base stays"),
        ContentError::ChunkInUse("base".into())
    );
}

/// A delete that fails while the file is still there must not report the chunk released.
#[test]
fn release_keeps_the_state_when_the_file_cannot_be_deleted() {
    let (fx, manifest) = Fixture::new("content-release-stuck", &[("a", &[], 0)]);
    let install = fx.content.join("downloads");
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &install);
    m.add_source(Box::new(DirSource::new(&fx.cdn)));
    m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
    // A non-empty directory in the file's place: `remove_file` fails and the path stays.
    let file = install.join("a.fpak");
    std::fs::remove_file(&file).expect("remove");
    std::fs::create_dir_all(file.join("keep")).expect("dir");
    assert!(m.release(&["a"], &[]).expect("release").is_empty());
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
    std::fs::remove_dir_all(&file).expect("cleanup");
    assert_eq!(m.release(&["a"], &[]).expect("release"), ["a"]);
    assert_eq!(m.state("a"), Some(ChunkState::NotInstalled));
}

/// Spec § 10: hashing leaves the frame. The chunk stays `Verifying` until the job reports, a pause
/// does not stop a complete file, and a corrupt file still fails through the same path.
#[test]
fn verification_runs_off_the_frame() {
    let (fx, manifest) = Fixture::new("content-verify-bg", &[("a", &[], 0), ("b", &[], 0)]);
    let mut m = fx.manager(&manifest);
    let (jobs, runner) = held_runner();
    m.set_verify_runner(Some(runner));
    let req = m
        .request(
            &["a"],
            RequestOptions {
                mount_when_done: true,
                ..RequestOptions::default()
            },
        )
        .expect("a");
    let mut server = fx.server();
    run(&mut m, 1 << 20, 3, Some(&mut server));
    assert_eq!(m.state("a"), Some(ChunkState::Verifying));
    assert_eq!(jobs.lock().len(), 1, "one job, not re-queued per step");
    assert_eq!(m.progress(req).expect("progress").state, "Downloading");
    assert!(fx.install.join("a.fpak.part").is_file());

    m.pause(req).expect("pause");
    assert_eq!(m.state("a"), Some(ChunkState::Verifying), "a whole file finishes");
    run_held(&jobs);
    m.advance(0, Some(&mut server));
    assert_eq!(m.state("a"), Some(ChunkState::Mounted));
    assert!(fx.install.join("a.fpak").is_file());

    m.inject("b", ContentFault::Corrupt, 0).expect("inject");
    m.request(&["b"], RequestOptions::default()).expect("b");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("b"), Some(ChunkState::Verifying));
    run_held(&jobs);
    m.advance(0, None);
    assert_eq!(m.state("b"), Some(ChunkState::Failed(FailReason::HashMismatch)));
    assert!(!fx.install.join("b.fpak.part").exists(), "partial file removed");
}

/// `release` during a hash job: the late result is dropped and the chunk stays released, also when
/// a new request runs meanwhile.
#[test]
fn a_released_chunks_late_verification_is_ignored() {
    let (fx, manifest) = Fixture::new("content-verify-stale", &[("a", &[], 0)]);
    let mut m = fx.manager(&manifest);
    let (jobs, runner) = held_runner();
    m.set_verify_runner(Some(runner));
    m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Verifying));
    assert_eq!(m.release(&["a"], &[]).expect("release"), ["a"]);
    assert_eq!(m.state("a"), Some(ChunkState::NotInstalled));
    run_held(&jobs);
    m.advance(0, None);
    assert_eq!(m.state("a"), Some(ChunkState::NotInstalled));

    m.request(&["a"], RequestOptions::default()).expect("again");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Verifying));
    run_held(&jobs);
    m.advance(0, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
}

/// The default runner: the engine's I/O pool, joined through `wait_verifications` (no sleeps).
#[test]
fn io_pool_verification_installs_the_chunk() {
    let (fx, manifest) = Fixture::new("content-verify-pool", &[("a", &[], 200_000)]);
    let mut m = fx.manager(&manifest);
    m.set_verify_runner(Some(regia_asset::content::io_pool_verify_runner()));
    let mut server = fx.server();
    m.request(
        &["a"],
        RequestOptions {
            mount_when_done: true,
            ..RequestOptions::default()
        },
    )
    .expect("a");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    m.wait_verifications(Some(&mut server));
    assert_eq!(m.state("a"), Some(ChunkState::Mounted));
    assert_eq!(server.unavailable_chunk(fx.ids["a"]), None);
}

/// `[content] base_url` reaches the running game: `content-host.ron` next to the manifest makes
/// `ContentPlugin` add the HTTP source, and a request fetches, verifies on the I/O pool and mounts.
#[test]
fn content_host_record_adds_the_http_source_at_startup() {
    use regia_app::prelude::*;
    use regia_asset::content::{
        CONTENT_HOST_FILE, ContentHost, ContentPlugin, ContentSettings, pick_base_url, resolve_base_url,
        serve_dir_loopback,
    };
    let (fx, _) = Fixture::new("content-host", &[("arena", &[], 0)]);
    let build_dir = fx.root.join("host/1.0-1-aaaa");
    std::fs::create_dir_all(&build_dir).expect("mkdir");
    std::fs::copy(fx.cdn.join("arena.fpak"), build_dir.join("arena.fpak")).expect("copy");
    let (url, stats) = serve_dir_loopback(fx.root.join("host")).expect("server");

    assert_eq!(pick_base_url(None, &fx.content), None, "no record, no source");
    ContentHost::new(url.as_str())
        .write_to_dir(&fx.content)
        .expect("write");
    let read = ContentHost::read_from_dir(&fx.content).expect("read");
    assert_eq!(read.map(|h| h.base_url).as_deref(), Some(url.as_str()));
    assert_eq!(resolve_base_url(&fx.content).as_deref(), Some(url.as_str()));
    assert_eq!(
        pick_base_url(Some("http://override.test".into()), &fx.content).as_deref(),
        Some("http://override.test"),
        "the environment wins for one run"
    );

    let mut settings = ContentSettings::for_content_dir(&fx.content);
    settings.install_dir = fx.install.clone();
    assert!(settings.background_verify);
    let mut app = App::new();
    app.add_plugins((
        regia_asset::AssetPlugin {
            settings: AssetServerSettings {
                project_root: fx.root.clone(),
                platform: "linux".into(),
                content_dir: Some(fx.content.clone()),
            },
        },
        ContentPlugin {
            settings: Some(settings),
        },
    ));
    app.finish_plugins();
    assert_eq!(
        app.world().resource::<ContentManager>().source_names(),
        ["http", "local"]
    );
    app.world_mut()
        .resource_mut::<ContentManager>()
        .request(
            &["arena"],
            RequestOptions {
                mount_when_done: true,
                ..RequestOptions::default()
            },
        )
        .expect("request");
    app.update();
    app.world_mut()
        .resource_mut::<ContentManager>()
        .wait_for_transfers();
    app.update();
    app.world_mut()
        .resource_scope(|world, mut m: Mut<ContentManager>| {
            let mut server = world.resource_mut::<AssetServer>();
            m.wait_verifications(Some(&mut server));
        });
    assert_eq!(
        app.world().resource::<ContentManager>().state("arena"),
        Some(ChunkState::Mounted)
    );
    assert!(stats.requests.load(std::sync::atomic::Ordering::Relaxed) >= 1);

    // A newer record is refused and logged, never fatal.
    std::fs::write(
        fx.content.join(CONTENT_HOST_FILE),
        "(format_version: 99, base_url: \"http://x\")",
    )
    .expect("write");
    assert!(ContentHost::read_from_dir(&fx.content).is_err());
    assert_eq!(resolve_base_url(&fx.content), None);
}

/// A resume keeps only the blocks that still match the manifest (spec § 2.6): a byte damaged in the
/// second block sends the transfer back to the end of the first, over both the directory source and
/// HTTP, and the whole-file hash then passes.
#[test]
fn resume_restarts_at_the_last_block_that_matches() {
    use regia_asset::content::{HttpSource, RESUME_BLOCK, block_hashes, serve_dir_loopback};
    use std::sync::atomic::Ordering;
    let block = RESUME_BLOCK as usize;
    let (fx, mut manifest) = Fixture::new("content-blocks", &[("a", &[], 3 * block)]);
    let bytes = std::fs::read(fx.cdn.join("a.fpak")).expect("pak");
    let entry = manifest.chunks.iter_mut().find(|c| c.name == "a").expect("a");
    entry.blocks = block_hashes(&mut bytes.as_slice()).expect("hashes");
    assert_eq!(entry.blocks.len(), bytes.len().div_ceil(block));
    let size = bytes.len() as u64;
    let write_part = |fx: &Fixture| {
        std::fs::create_dir_all(&fx.install).expect("mkdir");
        let mut part = bytes[..2 * block + 500].to_vec();
        part[block + 7] ^= 0xff;
        std::fs::write(fx.install.join("a.fpak.part"), part).expect("part");
    };

    write_part(&fx);
    let dir = DirSource::new(&fx.cdn);
    let mut m = ContentManager::from_manifest(manifest.clone(), &fx.content, &fx.install);
    m.add_source(Box::new(dir.clone()));
    m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 24, 4, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
    assert_eq!(dir.served(), size - RESUME_BLOCK, "only the first block was kept");
    m.release(&["a"], &[]).expect("release");

    let host = fx.root.join("host");
    std::fs::create_dir_all(host.join("1.0-1-aaaa")).expect("mkdir");
    std::fs::write(host.join("1.0-1-aaaa/a.fpak"), &bytes).expect("copy");
    let (url, stats) = serve_dir_loopback(host).expect("server");
    write_part(&fx);
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    m.add_source(Box::new(HttpSource::new(&url, "1.0-1-aaaa").expect("source")));
    m.request(&["a"], RequestOptions::default()).expect("a");
    m.advance(0, None);
    m.wait_for_transfers();
    m.advance(0, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
    assert_eq!(
        stats.bytes.load(Ordering::Relaxed),
        size - RESUME_BLOCK,
        "the range request starts after the good block"
    );
}

/// Test 15: with one slot, a High request submitted after a Low one starts first once the slot
/// frees.
#[test]
fn high_tier_starts_before_earlier_low_tier() {
    let (fx, manifest) = Fixture::new(
        "content-tiers",
        &[("busy", &[], 0), ("low", &[], 0), ("high", &[], 0)],
    );
    let mut m = fx.manager(&manifest);
    m.set_max_concurrent(1);
    m.request(&["busy"], RequestOptions::default()).expect("busy");
    m.advance(1, None);
    m.request(
        &["low"],
        RequestOptions {
            priority: Tier::Low,
            ..RequestOptions::default()
        },
    )
    .expect("low");
    m.request(
        &["high"],
        RequestOptions {
            priority: Tier::High,
            ..RequestOptions::default()
        },
    )
    .expect("high");
    m.drain_events();
    run(&mut m, 1 << 20, 2, None);
    let started: Vec<String> = m
        .drain_events()
        .into_iter()
        .filter(|e| matches!(e.to, ChunkState::Downloading { .. }))
        .map(|e| e.chunk)
        .collect();
    assert_eq!(started.first().map(String::as_str), Some("high"));
}

/// Test 16: an asset of an unmounted chunk reads as unavailable, never a panic or a placeholder.
#[test]
fn asset_in_unmounted_chunk_is_unavailable() {
    let (fx, manifest) = Fixture::new("content-unavailable", &[("arena", &[], 0)]);
    let mut server = fx.server();
    let id = fx.ids["arena"];
    assert_eq!(
        server.unavailable_chunk(id),
        Some("arena"),
        "chunk map read from the content dir"
    );
    assert!(server.load_mesh(id).is_err());
    assert_eq!(server.state(id), LoadState::Failed("unavailable:arena".into()));
    assert_eq!(server.unavailable_chunk(fx.base_id), None);
    let mut m = fx.manager(&manifest);
    m.request(
        &["arena"],
        RequestOptions {
            mount_when_done: true,
            ..RequestOptions::default()
        },
    )
    .expect("request");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    assert_eq!(server.unavailable_chunk(id), None);
    assert!(server.load_mesh(id).is_ok());
    assert_eq!(m.chunk_of(id), Some("arena"));
}

/// Test 17: unmount refuses while a load set pins the chunk; force removes the set and
/// invalidates.
#[test]
fn unmount_respects_pinned_load_sets() {
    let (fx, manifest) = Fixture::new("content-unmount", &[("arena", &[], 0)]);
    let mut server = fx.server();
    let mut m = fx.manager(&manifest);
    m.request(
        &["arena"],
        RequestOptions {
            mount_when_done: true,
            ..RequestOptions::default()
        },
    )
    .expect("request");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    let id = fx.ids["arena"];
    server.load_mesh(id).expect("loaded from the chunk");
    let sets = LoadSets::default();
    let mut set = LoadSet::new("map:arena");
    set.items.push(LoadItem {
        kind: LoadKind::Mesh,
        key: "assets/arena/a.mesh".into(),
        ram_bytes: 0,
        vram_bytes: 0,
    });
    sets.define(set);
    let err = m
        .unmount(&mut server, Some(&sets), "arena", false)
        .expect_err("in use");
    assert_eq!(err.reason(), "chunk_in_use");
    assert_eq!(
        m.release(&["arena"], &[]).expect_err("mounted").reason(),
        "chunk_in_use"
    );
    m.unmount(&mut server, Some(&sets), "arena", true).expect("force");
    assert!(sets.get("map:arena").is_none());
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    assert_eq!(server.state(id), LoadState::Failed("unavailable:arena".into()));
    assert_eq!(
        m.unmount(&mut server, None, BASE_CHUNK, true)
            .expect_err("base")
            .reason(),
        "chunk_in_use"
    );
    assert_eq!(m.release(&["arena"], &[]).expect("release"), ["arena"]);
    assert_eq!(m.state("arena"), Some(ChunkState::NotInstalled));
    assert!(!fx.install.join("arena.fpak").exists());
}

/// Test 18: a changed build marks installed chunks NeedsUpdate; a DLC cooked against another base
/// is `chunk_base_mismatch`.
#[test]
fn build_change_needs_update_and_dlc_base_mismatch() {
    let (fx, manifest) = Fixture::new("content-update", &[("arena", &[], 0)]);
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::copy(fx.cdn.join("arena.fpak"), fx.install.join("arena.fpak")).expect("copy");
    let mut record = ContentState::default();
    record.chunks.insert(
        "arena".into(),
        InstalledChunk {
            build_id: "0.9-1-old".into(),
            hash: "0".repeat(64),
            size: 0,
            modified: 0,
        },
    );
    record.write_to_dir(&fx.install).expect("record");
    let m = fx.manager(&manifest);
    assert_eq!(m.state("arena"), Some(ChunkState::NeedsUpdate));

    // A DLC built against another base.
    // Keep this fixture's manifest: each fixture generates a different base pak identity.
    let (fx2, mut manifest) = Fixture::new("content-dlc", &[]);
    let id = AssetId::new_random();
    write_pak(
        &fx2.install.join("dlc.fpak"),
        &[(id, "assets/dlc.mesh", mesh_file(id, 1))],
    );
    let mut dlc = entry(&fx2.install, "dlc", Delivery::External, 1, &["base@0.5-7-other"]);
    dlc.overrides = false;
    manifest.chunks.push(dlc);
    // The managers below read fx2's content directory: its base entry must describe fx2's base
    // pak, whose length can differ from fx's (random ids); a packaged pak of another length is
    // refused since ADR-8810.
    let base = manifest
        .chunks
        .iter_mut()
        .find(|c| c.name == BASE_CHUNK)
        .expect("base entry");
    *base = entry(&fx2.content, BASE_CHUNK, Delivery::Install, 0, &[]);
    let mut server = fx2.server();
    let mut m2 = ContentManager::from_manifest(manifest.clone(), &fx2.content, &fx2.install);
    let err = m2.mount(&mut server, "dlc").expect_err("mismatch");
    assert_eq!(err.reason(), "chunk_base_mismatch");
    manifest.compatible_with.push("0.5-7-other".into());
    let mut m3 = ContentManager::from_manifest(manifest, &fx2.content, &fx2.install);
    m3.mount(&mut server, "dlc").expect("compatible base");
}

/// Spec § 7: the same inputs and step sequence give the same states and event order.
#[test]
fn stepped_runs_are_reproducible() {
    let run_once = || {
        let (fx, manifest) = Fixture::new("content-repro", &[("a", &[], 9_000), ("b", &["a"], 3_000)]);
        let mut m = fx.manager(&manifest);
        m.set_max_concurrent(1);
        m.request(&["b"], RequestOptions::default()).expect("request");
        run(&mut m, 4_000, 8, None);
        m.drain_events()
            .into_iter()
            .map(|e| format!("{}:{}", e.chunk, e.to.label()))
            .collect::<Vec<_>>()
    };
    let first = run_once();
    assert_eq!(first, run_once());
    assert_eq!(first.last().map(String::as_str), Some("b:Installed"));
}

#[test]
fn errors_carry_documented_reasons() {
    let reasons = [
        ContentError::ManifestMissing.reason(),
        ContentError::ChunkNotFound("x".into()).reason(),
        ContentError::RequestNotFound(3).reason(),
    ];
    assert_eq!(
        reasons,
        ["manifest_missing", "chunk_not_found", "request_not_found"]
    );
    for r in reasons {
        assert!(r.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    }
}

/// Test 19: the http:// source end to end on loopback: a range resume from an existing `.part`
/// fetches only the tail, the file verifies and mounts.
#[test]
fn http_source_resumes_verifies_and_mounts_on_loopback() {
    use regia_asset::content::{HttpSource, serve_dir_loopback};
    use std::sync::atomic::Ordering;
    let (fx, manifest) = Fixture::new("content-http", &[("arena", &[], 300_000)]);
    // The static host layout: <root>/<build_id>/<file>.
    let host = fx.root.join("host");
    let served_dir = host.join(&manifest.build_id);
    std::fs::create_dir_all(&served_dir).expect("mkdir");
    let pak = std::fs::read(fx.cdn.join("arena.fpak")).expect("pak");
    std::fs::write(served_dir.join("arena.fpak"), &pak).expect("serve copy");
    // A previous, interrupted transfer left the first 100,000 bytes.
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::write(fx.install.join("arena.fpak.part"), &pak[..100_000]).expect("part");

    let (url, stats) = serve_dir_loopback(host).expect("server");
    let mut server = fx.server();
    let map = ChunkMap::read_from_dir(&fx.content)
        .expect("map")
        .expect("present");
    let mut m = ContentManager::from_manifest(manifest.clone(), &fx.content, &fx.install).with_chunk_map(map);
    m.add_source(Box::new(
        HttpSource::new(&url, &manifest.build_id).expect("source"),
    ));
    let req = m
        .request(
            &["arena"],
            RequestOptions {
                mount_when_done: true,
                ..RequestOptions::default()
            },
        )
        .expect("request");
    m.advance(0, Some(&mut server));
    assert!(matches!(m.state("arena"), Some(ChunkState::Downloading { .. })));
    m.wait_for_transfers();
    m.advance(0, Some(&mut server));
    assert_eq!(m.state("arena"), Some(ChunkState::Mounted));
    assert_eq!(m.progress(req).expect("p").state, "Mounted");
    assert_eq!(
        stats.bytes.load(Ordering::Relaxed),
        pak.len() as u64 - 100_000,
        "only the missing tail came over the wire"
    );
    assert!(server.load_mesh(fx.ids["arena"]).is_ok());

    // A file the host does not have fails with source_unreachable.
    let mut missing = ContentManager::from_manifest(manifest.clone(), &fx.content, &fx.root.join("install2"));
    missing.add_source(Box::new(HttpSource::new(&url, "other-build").expect("source")));
    missing
        .request(&["arena"], RequestOptions::default())
        .expect("request");
    missing.advance(0, None);
    missing.wait_for_transfers();
    missing.advance(0, None);
    assert!(matches!(
        missing.state("arena"),
        Some(ChunkState::Failed(FailReason::SourceUnreachable(_)))
    ));
}

/// Spec 40 § 2.6: the manifest itself comes from the host (`latest.ron`, then the build's
/// manifest) and must match the running platform and an accepted build.
#[test]
fn manifest_is_fetched_from_the_host_and_checked() {
    use regia_asset::content::{fetch_manifest, serve_dir_loopback};
    let (fx, manifest) = Fixture::new("content-fetch-manifest", &[("arena", &[], 0)]);
    let host = fx.root.join("host");
    std::fs::create_dir_all(host.join(&manifest.build_id)).expect("mkdir");
    manifest
        .write_to_dir(&host.join(&manifest.build_id))
        .expect("manifest");
    std::fs::write(host.join("latest.ron"), format!("\"{}\"\n", manifest.build_id)).expect("latest");
    let (url, _) = serve_dir_loopback(host.clone()).expect("server");
    assert_eq!(fetch_manifest(&url, &manifest).expect("fetch"), manifest);

    let mut other = manifest.clone();
    other.platform = "android".into();
    assert_eq!(
        fetch_manifest(&url, &other).expect_err("platform").reason(),
        "chunk_base_mismatch"
    );
    std::fs::write(host.join("latest.ron"), "9.9-9-newer\n").expect("latest");
    assert_eq!(
        fetch_manifest(&url, &manifest)
            .expect_err("missing build")
            .reason(),
        "source_unreachable"
    );
    // Without the content-http feature an https:// host is refused before any connection
    // (the TLS path itself is tests/content_https.rs).
    if !regia_asset::content::HttpClient::supports_https() {
        assert_eq!(
            fetch_manifest("https://cdn.example.com", &manifest)
                .expect_err("https")
                .reason(),
            "source_not_supported"
        );
    }
}

/// sec-review-assets-agent (ADR-5869): `content.check_host` (Read permission) fetched
/// `<base_url>/latest.ron` and, when it named no usable build id, echoed the whole reply in the
/// error. A base URL such as `http://127.0.0.1:<port>/admin/secret?x=` read any local HTTP service
/// through a read-only agent token. The error now says how large the reply was, not what it said.
#[test]
fn an_unusable_latest_reply_is_not_echoed() {
    use regia_asset::content::{fetch_manifest, serve_dir_loopback};
    let (fx, manifest) = Fixture::new("content-latest-no-echo", &[("arena", &[], 0)]);
    let host = fx.root.join("host");
    std::fs::create_dir_all(&host).expect("mkdir");
    std::fs::write(host.join("latest.ron"), "api_key = SECRET-7f3a91 and more\n").expect("latest");
    let (url, _) = serve_dir_loopback(host).expect("server");
    let err = fetch_manifest(&url, &manifest).expect_err("no usable build id");
    let text = err.to_string();
    assert!(
        !text.contains("SECRET"),
        "the reply leaked into the error: {text}"
    );
    assert!(text.contains("no usable build id"), "{text}");
    // One token is a usable id: the manifest request under it then fails, and its error named the
    // request path, which held the token.
    std::fs::write(host_dir(&fx).join("latest.ron"), "SECRET-7f3a91\n").expect("latest");
    let text = fetch_manifest(&url, &manifest)
        .expect_err("no manifest")
        .to_string();
    assert!(
        !text.contains("SECRET"),
        "the reply leaked into the error: {text}"
    );
}

fn host_dir(fx: &Fixture) -> PathBuf {
    fx.root.join("host")
}

/// `ContentPlugin` at startup: reads the manifest from the asset server's content directory,
/// marks the packaged base as mounted, mounts installed chunks, gives the server the chunk map,
/// and `content_tick` steps requests every frame (its events become `ChunkStateChanged`).
#[test]
fn content_plugin_wires_the_manager_into_an_app() {
    use regia_app::prelude::*;
    use regia_asset::content::{ChunkStateChanged, ContentPlugin, ContentSettings};
    let (fx, _) = Fixture::new("content-plugin", &[("arena", &[], 0), ("forest", &[], 0)]);
    // `forest` was fetched earlier: it sits in the install directory.
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::copy(fx.cdn.join("forest.fpak"), fx.install.join("forest.fpak")).expect("copy");
    let mut settings = ContentSettings::for_content_dir(&fx.content);
    settings.install_dir = fx.install.clone();
    // Hash inside the frame: this test counts frames.
    settings.background_verify = false;
    let mut app = App::new();
    app.add_plugins((
        regia_asset::AssetPlugin {
            settings: AssetServerSettings {
                project_root: fx.root.clone(),
                platform: "linux".into(),
                content_dir: Some(fx.content.clone()),
            },
        },
        ContentPlugin {
            settings: Some(settings),
        },
    ));
    app.finish_plugins();
    {
        let m = app.world().resource::<ContentManager>();
        assert_eq!(m.state(BASE_CHUNK), Some(ChunkState::Mounted));
        assert_eq!(
            m.state("forest"),
            Some(ChunkState::Mounted),
            "installed chunks mount at startup"
        );
        assert_eq!(m.state("arena"), Some(ChunkState::NotInstalled));
    }
    let server = app.world().resource::<AssetServer>();
    assert_eq!(server.mounted_pak_names(), ["base", "forest"]);
    assert_eq!(server.unavailable_chunk(fx.ids["arena"]), Some("arena"));

    // A request made by the game is stepped by the frame loop.
    app.world_mut()
        .resource_mut::<ContentManager>()
        .add_source(Box::new(DirSource::new(&fx.cdn)));
    app.world_mut()
        .resource_mut::<ContentManager>()
        .request(
            &["arena"],
            RequestOptions {
                mount_when_done: true,
                ..RequestOptions::default()
            },
        )
        .expect("request");
    app.update();
    app.update();
    assert_eq!(
        app.world().resource::<ContentManager>().state("arena"),
        Some(ChunkState::Mounted)
    );
    assert_eq!(
        app.world()
            .resource::<AssetServer>()
            .unavailable_chunk(fx.ids["arena"]),
        None
    );
    assert!(
        !app.world().resource::<Messages<ChunkStateChanged>>().is_empty(),
        "state changes written as messages"
    );
}

/// Levels audit L2: a released chunk had no way back, so its request read `Queued` forever.
#[test]
fn releasing_a_chunk_in_flight_ends_the_requests_that_list_it() {
    let (fx, manifest) = Fixture::new("content-release-live", &[("arena", &[], 50_000)]);
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    m.add_source(Box::new(DirSource::new(&fx.cdn)));
    let req = m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, 10_000, 2, None);
    m.release(&["arena"], &[]).expect("release");
    assert_eq!(m.state("arena"), Some(ChunkState::NotInstalled));
    assert_eq!(m.progress(req).expect("p").state, "Cancelled");
    let again = m
        .request(&["arena"], RequestOptions::default())
        .expect("a new request works");
    run(&mut m, 1 << 20, 6, None);
    assert_eq!(m.progress(again).expect("p").state, "Installed");
}

/// corr-asset: a request that finished (and was retired) before its chunk was released ends as
/// `Cancelled`, like one still in flight; it read `Queued` forever.
#[test]
fn releasing_a_chunk_ends_a_finished_request_that_lists_it() {
    let (fx, manifest) = Fixture::new("content-release-done", &[("arena", &[], 50_000)]);
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    m.add_source(Box::new(DirSource::new(&fx.cdn)));
    let req = m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, 1 << 20, 8, None);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    // One more advance retires the finished request.
    m.advance(0, None);
    assert_eq!(m.progress(req).expect("p").state, "Installed");
    m.release(&["arena"], &[]).expect("release");
    assert_eq!(m.state("arena"), Some(ChunkState::NotInstalled));
    assert_eq!(m.progress(req).expect("p").state, "Cancelled");
}

/// Contentdel L4: `requires` cycles are refused by `request` and the cook; a startup mount walks a
/// tampered manifest with neither in front of it, and overflowed the stack.
#[test]
fn a_requires_cycle_fails_the_mount_instead_of_the_stack() {
    let (fx, mut manifest) = Fixture::new("content-mount-cycle", &[("a", &[], 1_000), ("b", &["a"], 1_000)]);
    for name in ["a", "b"] {
        let from = fx.cdn.join(format!("{name}.fpak"));
        std::fs::copy(&from, fx.content.join(format!("{name}.fpak"))).expect("packaged");
    }
    for c in &mut manifest.chunks {
        if c.name == "a" {
            c.requires = vec!["b".to_owned()];
        }
    }
    let mut server = fx.server();
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    assert_eq!(m.state("a"), Some(ChunkState::Installed), "packaged");
    let err = m.mount(&mut server, "a").expect_err("cycle");
    assert_eq!(err.reason(), "chunk_cycle");
}

/// Contentdel L3: an empty name and a shared name collide on one `.part`, control characters reach
/// a request line, and a device name swallows every write on Windows.
#[test]
fn a_manifest_chunk_file_must_be_a_real_bare_file_name() {
    let (_fx, manifest) = Fixture::new("content-file-names", &[("a", &[], 10), ("b", &[], 10)]);
    let parse = |m: &ContentManifest| ContentManifest::from_ron_str(&m.to_ron_string().expect("ron"));
    assert!(parse(&manifest).is_ok());
    for bad in ["", "a\r\nb.fpak", "nul.fpak", "COM1", "x/y", "x\\y", "c:x", "..a"] {
        let mut m = manifest.clone();
        m.chunks[0].file = bad.to_owned();
        assert!(matches!(parse(&m), Err(ContentError::Format(_))), "{bad:?}");
    }
    let mut m = manifest.clone();
    m.chunks[1].file = m.chunks[0].file.clone();
    assert!(
        matches!(parse(&m), Err(ContentError::Format(_))),
        "two chunks on one file"
    );
    assert!(
        validate_chunk_name("nul").is_err(),
        "the cook refuses it before writing nul.fpak"
    );
    assert!(validate_chunk_name("nullable").is_ok());
}

/// Audit v0.2.12 (security): a local `content-manifest.ron` build id reached every chunk request
/// line unchecked (`latest.ron` was checked, the packaged manifest was not).
#[test]
fn a_local_manifest_build_id_that_would_split_a_request_line_is_refused() {
    let (_fx, manifest) = Fixture::new("content-local-build-id", &[("a", &[], 10)]);
    let parse = |m: &ContentManifest| ContentManifest::from_ron_str(&m.to_ron_string().expect("ron"));
    for bad in [
        "1-0-x\r\nHost: evil\r\nX: ",
        "2026/11/01-1-abc",
        "a\\b",
        "..",
        "1 0",
        "",
    ] {
        let mut m = manifest.clone();
        m.build_id = bad.to_owned();
        assert!(matches!(parse(&m), Err(ContentError::Format(_))), "{bad:?}");
        let mut m = manifest.clone();
        m.compatible_with = vec![bad.to_owned()];
        assert!(
            matches!(parse(&m), Err(ContentError::Format(_))),
            "compatible_with {bad:?}"
        );
    }
    assert!(parse(&manifest).is_ok());
    assert!(regia_asset::content::validate_build_id("1.2.0-7-0123456789ab").is_ok());
}

/// Contentdel L2: the id from a host goes into a request line.
#[test]
fn a_build_id_with_control_characters_is_refused_before_the_second_request() {
    use regia_asset::content::{fetch_manifest, serve_dir_loopback};
    let (fx, manifest) = Fixture::new("content-crlf", &[("a", &[], 10)]);
    let host = fx.root.join("host");
    std::fs::create_dir_all(&host).expect("host");
    // No space: the old check already refused those, and CR and LF went through.
    std::fs::write(host.join("latest.ron"), "x\r\nX-Evil:1").expect("latest");
    let (url, stats) = serve_dir_loopback(host).expect("server");
    let err = fetch_manifest(&url, &manifest).expect_err("control characters");
    assert!(matches!(err, ContentError::Format(_)), "{err}");
    assert_eq!(stats.requests.load(std::sync::atomic::Ordering::Relaxed), 1);
}

/// Levels audit M3: with `shared = "duplicate"` an asset sits in every claiming chunk but the map
/// names one home. The asset is loadable while any pak holding it is mounted, and only the last such
/// pak pins it.
#[test]
fn a_duplicated_asset_is_available_from_any_chunk_that_holds_it() {
    let (fx, mut manifest) = Fixture::new("content-duplicate", &[("a", &[], 0), ("b", &[], 0)]);
    let shared = fx.ids["a"];
    // `b` also carries the asset whose home is `a`.
    let b_pak = fx.cdn.join("b.fpak");
    write_pak(
        &b_pak,
        &[
            (fx.ids["b"], "assets/b/a.mesh", mesh_file(fx.ids["b"], 2)),
            (shared, "assets/a/a.mesh", mesh_file(shared, 2)),
        ],
    );
    for c in &mut manifest.chunks {
        if c.name == "b" {
            c.size = std::fs::metadata(&b_pak).expect("pak").len();
            c.hash = hash_file(&b_pak).expect("hash");
        }
    }
    let mut server = fx.server();
    let mut m = fx.manager(&manifest);
    assert_eq!(server.unavailable_chunk(shared), Some("a"));
    let options = || RequestOptions {
        mount_when_done: true,
        ..RequestOptions::default()
    };
    m.request(&["b"], options()).expect("request");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    assert_eq!(m.state("b"), Some(ChunkState::Mounted));
    assert_eq!(server.unavailable_chunk(shared), None, "`b` holds a copy");
    assert!(server.load_mesh(shared).is_ok());
    // A set pinning the asset holds `b` while `b` is its only pak, whatever the map's home is.
    let sets = LoadSets::default();
    let mut set = LoadSet::new("hold");
    set.items.push(LoadItem {
        kind: LoadKind::Mesh,
        key: "assets/a/a.mesh".into(),
        ram_bytes: 0,
        vram_bytes: 0,
    });
    sets.define(set);
    assert!(m.unmount(&mut server, Some(&sets), "b", false).is_err());
    m.request(&["a"], options()).expect("request");
    run(&mut m, 1 << 20, 3, Some(&mut server));
    m.unmount(&mut server, Some(&sets), "b", false)
        .expect("`a` holds it now, so `b` may go");
    assert_eq!(server.unavailable_chunk(shared), None);
    // Audit v0.2.12 (content H2): the asset came from `b`, and unmounting `b` marked it
    // `Failed("unavailable:b")` although `a` holds a copy. It now reloads from `a`.
    assert_eq!(server.state(shared), LoadState::Loaded);
    assert_eq!(
        server.source_of(shared),
        regia_asset::AssetSource::Pak("a".into())
    );
}

/// Review finding: `release` must not take down what it leaves alone. A paused request keeps its
/// chunk resumable, and a chunk in `keep` keeps downloading.
#[test]
fn releasing_a_chunk_leaves_kept_and_paused_work_resumable() {
    let (fx, manifest) = Fixture::new(
        "content-release-others",
        &[
            ("dep", &[], 50_000),
            ("a", &["dep"], 50_000),
            ("c", &["dep"], 50_000),
            ("b", &[], 50_000),
        ],
    );
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    m.add_source(Box::new(DirSource::new(&fx.cdn)));
    m.set_max_concurrent(4);
    let first = m.request(&["a", "b"], RequestOptions::default()).expect("first");
    let paused = m.request(&["c"], RequestOptions::default()).expect("second");
    run(&mut m, 1_000, 1, None);
    m.pause(paused).expect("pause");
    assert!(
        matches!(m.state("dep"), Some(ChunkState::Downloading { .. })),
        "`first` still wants it"
    );
    m.release(&["a", "b"], &["b"]).expect("release");
    assert_eq!(m.progress(first).expect("p").state, "Cancelled");
    assert!(
        matches!(m.state("b"), Some(ChunkState::Downloading { .. })),
        "kept: {:?}",
        m.state("b")
    );
    assert_eq!(
        m.state("dep"),
        Some(ChunkState::Paused),
        "only a paused request lists it"
    );
    m.resume(paused).expect("resume");
    run(&mut m, 1 << 20, 8, None);
    assert_eq!(m.progress(paused).expect("p").state, "Installed");
}

/// Review finding: a mounted chunk reached twice through a diamond is not a cycle.
#[test]
fn a_diamond_over_a_mounted_chunk_is_not_a_cycle() {
    let (fx, manifest) = Fixture::new(
        "content-mount-diamond",
        &[
            ("c", &[], 0),
            ("a", &["c"], 0),
            ("b", &["c"], 0),
            ("x", &["a", "b"], 0),
        ],
    );
    // Opened first: a server mounts every pak already in its content directory.
    let mut server = fx.server();
    for name in ["c", "a", "b", "x"] {
        let from = fx.cdn.join(format!("{name}.fpak"));
        std::fs::copy(&from, fx.content.join(format!("{name}.fpak"))).expect("packaged");
    }
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    m.mount(&mut server, "c").expect("c");
    m.mount(&mut server, "x").expect("the diamond mounts");
    assert_eq!(m.state("x"), Some(ChunkState::Mounted));
}

/// Audit concurrency-1103 asset L6: a hash job the runner drops (or that panics) never reported,
/// so `wait_verifications` blocked forever and the chunk stayed `Verifying`. The job reports a
/// failure from a drop guard instead.
#[test]
fn a_dropped_verify_job_reports_instead_of_hanging_the_wait() {
    let (fx, manifest) = Fixture::new("content-verify-dropped", &[("a", &[], 0)]);
    let mut m = fx.manager(&manifest);
    let dropper: regia_asset::content::VerifyRunner = Arc::new(|job: Box<dyn FnOnce() + Send>| drop(job));
    m.set_verify_runner(Some(dropper));
    m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 20, 3, None);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        m.wait_verifications(None);
        let _ = tx.send(m.state("a"));
    });
    let state = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("wait_verifications hung on a hash job that never ran");
    assert!(matches!(state, Some(ChunkState::Failed(_))), "{state:?}");
}

/// Audit concurrency-1103 asset M1: `release` dropped the transfer and the verify token before
/// deleting the files, so a failed delete left a `Verifying` (or `Downloading`) chunk with no
/// transfer, no verify job and no way to move again. It now ends as `Failed`, and a new request
/// fetches it once the file can go.
#[test]
fn a_failed_release_of_an_in_flight_chunk_does_not_wedge_it() {
    let (fx, manifest) = Fixture::new("content-release-in-flight-stuck", &[("a", &[], 0)]);
    let mut m = fx.manager(&manifest);
    let (jobs, runner) = held_runner();
    m.set_verify_runner(Some(runner));
    let req = m.request(&["a"], RequestOptions::default()).expect("a");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("a"), Some(ChunkState::Verifying));
    // A non-empty directory in the `.part` file's place: `remove_file` fails and the path stays.
    let part = fx.install.join("a.fpak.part");
    std::fs::remove_file(&part).expect("remove");
    std::fs::create_dir_all(part.join("keep")).expect("dir");
    assert!(m.release(&["a"], &[]).expect("release").is_empty());
    run_held(&jobs);
    m.advance(0, None);
    let state = m.state("a");
    assert!(
        matches!(state, Some(ChunkState::Failed(_))),
        "a chunk whose transfer was dropped must not stay in flight: {state:?}"
    );
    let request_state = m.progress(req).expect("progress").state;
    assert!(request_state.starts_with("Failed("), "{request_state}");
    std::fs::remove_dir_all(&part).expect("cleanup");
    m.request(&["a"], RequestOptions::default()).expect("again");
    run(&mut m, 1 << 20, 3, None);
    run_held(&jobs);
    m.advance(0, None);
    assert_eq!(m.state("a"), Some(ChunkState::Installed));
}

/// A running transfer does not clone the chunk record or allocate a state-map key on a progress
/// step. After the request finishes, the next advance does not walk chunk states.
#[test]
fn asset1_perf_transfer_clones_the_chunk_only_when_done() {
    use regia_asset::content::{
        chunk_entry_clones, content_queue_walks, content_state_key_allocs, reset_chunk_entry_clones,
        reset_content_queue_walks, reset_content_state_key_allocs,
    };

    let (fx, mut manifest) = Fixture::new("content-asset1-clone", &[("arena", &[], 3 << 20)]);
    let arena = manifest
        .chunks
        .iter_mut()
        .find(|chunk| chunk.name == "arena")
        .expect("arena");
    arena.blocks = vec!["ab".repeat(32); 3];
    let mut m = fx.manager(&manifest);
    let req = m.request(&["arena"], RequestOptions::default()).expect("request");
    m.advance(1 << 16, None);
    assert!(
        matches!(m.state("arena"), Some(ChunkState::Downloading { .. })),
        "the first step starts the transfer: {:?}",
        m.state("arena")
    );

    reset_chunk_entry_clones();
    reset_content_state_key_allocs();
    reset_content_queue_walks();
    m.advance(1 << 16, None);
    m.advance(1 << 16, None);
    assert!(
        matches!(m.state("arena"), Some(ChunkState::Downloading { .. })),
        "two short steps stay in progress: {:?}",
        m.state("arena")
    );
    assert_eq!(chunk_entry_clones(), 0, "a progress step cloned the chunk record");
    assert_eq!(
        content_state_key_allocs(),
        0,
        "a progress step allocated a state-map key"
    );
    assert!(
        content_queue_walks() >= 1,
        "a live request must still walk the queue"
    );

    reset_chunk_entry_clones();
    let mut finished = false;
    for _ in 0..8 {
        m.advance(8 << 20, None);
        if m.state("arena") == Some(ChunkState::Installed) {
            finished = true;
            break;
        }
    }
    assert!(finished, "the transfer did not install: {:?}", m.state("arena"));
    assert!(
        chunk_entry_clones() >= 1,
        "the done step must clone the record once it needs the hash"
    );
    assert_eq!(m.progress(req).expect("progress").state, "Installed");
    assert_eq!(m.pause(req).expect_err("finished").reason(), "request_finished");

    reset_chunk_entry_clones();
    reset_content_queue_walks();
    reset_content_state_key_allocs();
    m.advance(8 << 20, None);
    assert_eq!(content_queue_walks(), 0, "a finished request walked chunk states");
    assert_eq!(chunk_entry_clones(), 0, "an idle advance cloned a chunk record");
    assert_eq!(
        content_state_key_allocs(),
        0,
        "an idle advance allocated a state key"
    );
    assert_eq!(
        m.progress(req).expect("progress still answers").state,
        "Installed"
    );
}

// ---- conformance audit (spec 40 §§ 2.5, 2.6, 5; lane spec-17-39, ADR-5636) ----------------------

/// Spec 40 § 5 `content.inject`: `fail` fails the transfer once `after_bytes` are on disk, `stall`
/// stops it there (no further bytes however many steps run), and `metered` holds a request in
/// `WaitingForUnmetered` until it is approved.
#[test]
fn fail_stall_and_metered_faults_act_at_after_bytes() {
    let (fx, manifest) = Fixture::new(
        "content-faults",
        &[("a", &[], 20_000), ("b", &[], 20_000), ("c", &[], 0)],
    );
    let mut m = fx.manager(&manifest);
    m.inject("a", ContentFault::Fail, 5_000).expect("inject");
    m.inject("b", ContentFault::Stall, 5_000).expect("inject");
    m.request(&["a"], RequestOptions::default()).expect("a");
    m.request(&["b"], RequestOptions::default()).expect("b");
    run(&mut m, 1_000, 30, None);
    assert_eq!(m.state("a"), Some(ChunkState::Failed(FailReason::Injected)));
    match m.state("b") {
        Some(ChunkState::Downloading { done, total }) => {
            assert_eq!(done, 5_000, "a stall holds at after_bytes");
            assert!(total > done);
        }
        other => panic!("b should be stalled mid-download, is {other:?}"),
    }
    assert_eq!(
        m.inject("nope", ContentFault::Fail, 0)
            .expect_err("unknown")
            .reason(),
        "chunk_not_found"
    );

    let mut m = fx.manager(&manifest);
    m.inject("c", ContentFault::Metered, 0).expect("inject");
    let rc = m.request(&["c"], RequestOptions::default()).expect("c");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("c"), Some(ChunkState::WaitingForUnmetered));
    m.approve_metered(rc).expect("approve");
    run(&mut m, 1 << 20, 3, None);
    assert_eq!(m.state("c"), Some(ChunkState::Installed));
}

/// Spec 40 § 2.5: at most `max_concurrent` transfers run at once, default 4.
#[test]
fn four_transfers_run_at_once_by_default() {
    assert_eq!(
        regia_asset::content::ContentSettings::for_content_dir("content").max_concurrent,
        4
    );
    let names = ["c1", "c2", "c3", "c4", "c5", "c6"];
    let chunks: Vec<(&str, &[&str], usize)> = names.iter().map(|n| (*n, &[][..], 20_000)).collect();
    let (fx, manifest) = Fixture::new("content-concurrent", &chunks);
    let mut m = fx.manager(&manifest);
    m.request(&names, RequestOptions::default()).expect("request");
    m.advance(1_000, None);
    let downloading = names
        .iter()
        .filter(|n| matches!(m.state(n), Some(ChunkState::Downloading { .. })))
        .count();
    let queued = names
        .iter()
        .filter(|n| m.state(n) == Some(ChunkState::Queued))
        .count();
    assert_eq!((downloading, queued), (4, 2));
}

/// Spec 40 § 2.5: mounting a chunk of lower `mount_order` after a higher one re-sorts the mounts
/// and leaves the assets already loaded from the higher chunk alone.
#[test]
fn a_later_mount_of_a_lower_order_chunk_re_sorts_without_reloading() {
    let (fx, manifest) = Fixture::new("content-resort", &[("one", &[], 0), ("two", &[], 0)]);
    let mut server = fx.server();
    let mut m = fx.manager(&manifest);
    m.request(&["one", "two"], RequestOptions::default())
        .expect("request");
    run(&mut m, 1 << 20, 4, None);
    assert_eq!(m.state("one"), Some(ChunkState::Installed));
    assert_eq!(m.state("two"), Some(ChunkState::Installed));
    m.sync_with_server(&mut server, false);
    m.mount(&mut server, "two").expect("mount two");
    let id = fx.ids["two"];
    let handle = server.load_mesh(id).expect("loaded from two");
    let generation = server.generation(id);
    m.mount(&mut server, "one").expect("mount one");
    assert_eq!(server.mounted_pak_names(), ["base", "one", "two"]);
    assert_eq!(
        server.generation(id),
        generation,
        "no reload of an id one does not hold"
    );
    assert!(
        server.meshes().get(&handle).is_some(),
        "the live handle still reads"
    );
}

/// Spec 40 § 2.6 (ADR-2138): a build id is at most 128 bytes.
#[test]
fn a_build_id_longer_than_128_bytes_is_refused() {
    use regia_asset::content::validate_build_id;
    assert!(validate_build_id(&"x".repeat(128)).is_ok());
    assert!(validate_build_id(&"x".repeat(129)).is_err());
    let (_fx, mut manifest) = Fixture::new("content-long-build-id", &[]);
    manifest.build_id = "x".repeat(129);
    let text = manifest.to_ron_string().expect("ron");
    assert!(matches!(
        ContentManifest::from_ron_str(&text),
        Err(ContentError::Format(_))
    ));
}

/// Spec 40 § 5: every `data.reason` a content error or a failed chunk can carry is lowercase
/// ASCII and documented on the agent page (`content-chunks.md`, or `content-trust.md` for the
/// signing codes of ADR-2710).
#[test]
fn every_content_reason_is_documented_on_the_book_page() {
    let book = concat!(
        include_str!("../../../book/agents/content-chunks.md"),
        include_str!("../../../book/agents/content-trust.md")
    );
    let errors = [
        ContentError::ManifestMissing,
        ContentError::ManifestVersion {
            found: 2,
            supported: 1,
        },
        ContentError::ChunkNotFound("x".into()),
        ContentError::AssetNotFound("x".into()),
        ContentError::ChunkNotInstalled("x".into()),
        ContentError::ChunkIntegrityMismatch("x".into()),
        ContentError::ChunkInUse("x".into()),
        ContentError::ChunkBaseMismatch {
            chunk: "x".into(),
            expected: "a".into(),
            running: "b".into(),
        },
        ContentError::RequestNotFound(1),
        ContentError::RequestFinished(1),
        ContentError::RequestNotPaused(1),
        ContentError::SourceUnreachable("x".into()),
        ContentError::SourceNotSupported("x".into()),
        ContentError::ChunkCycle(vec!["x".into()]),
        ContentError::BadParams("x".into()),
        ContentError::Format("x".into()),
        ContentError::Io("x".into()),
        ContentError::SignatureInvalid("x".into()),
        ContentError::SignatureMissing("x".into()),
        ContentError::ChunkSizeMismatch {
            chunk: "x".into(),
            expected: 1,
            found: 2,
        },
    ];
    let fails = [
        FailReason::HashMismatch,
        FailReason::NoSpace,
        FailReason::SourceUnreachable("x".into()),
        FailReason::StoreUnknownStatus,
        FailReason::Injected,
        FailReason::Io("x".into()),
        FailReason::SizeMismatch,
    ];
    // Asset-load codes of the same rule: plain strings no enum lists (ADR-8810, ADR-8860).
    let load_codes = ["chunk_refused", "chunk_unreadable", "pak_entry_corrupt"];
    let codes = errors
        .iter()
        .map(ContentError::reason)
        .chain(fails.iter().map(FailReason::code))
        .chain(load_codes);
    for code in codes {
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
        assert!(
            book.contains(&format!("`{code}`")),
            "reason `{code}` is not documented on the book page"
        );
    }
}

/// Spec 40 § 2.5: a `NeedsUpdate` chunk is fetched again by `request`; the verified new file
/// replaces the stale one and the install record carries the manifest's hash.
#[test]
fn a_needs_update_chunk_is_refetched_on_request() {
    let (fx, manifest) = Fixture::new("content-refetch", &[("arena", &[], 5_000)]);
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::write(fx.install.join("arena.fpak"), b"an older build of the chunk").expect("old file");
    let mut record = ContentState::default();
    record.chunks.insert(
        "arena".into(),
        InstalledChunk {
            build_id: "0.9-1-old".into(),
            hash: "0".repeat(64),
            size: 0,
            modified: 0,
        },
    );
    record.write_to_dir(&fx.install).expect("record");
    let mut m = fx.manager(&manifest);
    assert_eq!(m.state("arena"), Some(ChunkState::NeedsUpdate));
    m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, 1 << 20, 4, None);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    let want = &manifest.chunk("arena").expect("arena").hash;
    assert_eq!(
        &hash_file(&fx.install.join("arena.fpak")).expect("hash"),
        want,
        "the stale file was replaced by the verified one"
    );
    assert!(!fx.install.join("arena.fpak.part").exists());
    let saved = ContentState::read_from_dir(&fx.install);
    assert_eq!(&saved.chunks["arena"].hash, want);
}

/// ADR-8810: a packaged pak whose length is not its manifest entry's `size` is not mounted. The
/// content manager reports it `Failed(size_mismatch)`, and the ids its table of contents lists
/// fail to load (`chunk_refused`) instead of being served by the base pak's older bytes.
#[test]
fn a_packaged_pak_of_the_wrong_size_is_refused_and_never_serves_older_bytes() {
    let (fx, mut manifest) = Fixture::new("content-size-refused", &[]);
    let id = fx.base_id;
    let patch_path = fx.content.join("patch.fpak");
    write_pak(&patch_path, &[(id, "assets/base.mesh", mesh_file(id, 3))]);
    let mut patch = entry(&fx.content, "patch", Delivery::Install, 1, &[]);
    patch.overrides = true;
    manifest.chunks.push(patch);
    manifest.write_to_dir(&fx.content).expect("manifest");
    // A torn copy: the installer appended a block twice. The header and the TOC still read.
    let mut bytes = std::fs::read(&patch_path).expect("pak");
    bytes.extend_from_slice(&[0u8; 4096]);
    std::fs::write(&patch_path, &bytes).expect("grow");

    let mut server = fx.server();
    assert_eq!(
        server.mounted_pak_names(),
        ["base"],
        "the wrong-size patch is not mounted"
    );
    let err = server
        .load_mesh(id)
        .expect_err("the refused patch's id must not load the base's bytes");
    assert!(err.to_string().contains("chunk_refused"), "{err}");
    let m = fx.manager(&manifest);
    assert_eq!(
        m.state("patch").map(|s| s.label()).as_deref(),
        Some("Failed(size_mismatch)"),
        "the manager reports the refusal with its code"
    );
    assert_eq!(
        m.chunk_path("patch"),
        None,
        "a refused file is never mounted by the manager either"
    );
}

/// ADR-8810: an entry of the newest pak that holds an id and fails to read is a load error
/// (`pak_entry_corrupt`), never a silent fall back to an earlier pak's version of the asset.
#[test]
fn a_corrupt_entry_in_an_overriding_pak_never_serves_the_older_version() {
    let (fx, mut manifest) = Fixture::new("content-corrupt-override", &[]);
    let id = fx.base_id;
    let patch_path = fx.content.join("patch.fpak");
    write_pak_with(&patch_path, &[(id, "assets/base.mesh", mesh_file(id, 3))], |e| {
        // Declares one byte more than it stores, uncompressed: the reader refuses the entry.
        e.size += 1;
    });
    let mut patch = entry(&fx.content, "patch", Delivery::Install, 1, &[]);
    patch.overrides = true;
    manifest.chunks.push(patch);
    manifest.write_to_dir(&fx.content).expect("manifest");

    let mut server = fx.server();
    assert_eq!(
        server.mounted_pak_names(),
        ["base", "patch"],
        "the size is right: it mounts"
    );
    let err = server
        .load_mesh(id)
        .expect_err("the patch's bad entry must not load the base's older mesh");
    assert!(err.to_string().contains("pak_entry_corrupt"), "{err}");
    let reader = server.payload_reader();
    let err = reader
        .read(id)
        .expect_err("the worker-thread reader follows the same rule");
    assert!(err.to_string().contains("pak_entry_corrupt"), "{err}");
}

/// ADR-8810: the second launch of an unchanged install hashes nothing and costs one stat per
/// packaged pak (two for a fetched one: the packaged miss and the fetched file).
#[test]
fn the_second_launch_of_an_unchanged_install_hashes_nothing() {
    let (fx, manifest) = Fixture::new("content-second-launch", &[("arena", &[], 0)]);
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::copy(fx.cdn.join("arena.fpak"), fx.install.join("arena.fpak")).expect("fetched copy");
    let first = fx.manager(&manifest);
    assert_eq!(first.state(BASE_CHUNK), Some(ChunkState::Installed));
    assert_eq!(first.state("arena"), Some(ChunkState::Installed));
    assert_eq!(
        first.scan_counts().hashes,
        1,
        "the first launch hashes the unrecorded fetched chunk"
    );
    let second = fx.manager(&manifest);
    assert_eq!(second.state("arena"), Some(ChunkState::Installed));
    let counts = second.scan_counts();
    assert_eq!(counts.hashes, 0, "nothing is hashed on an unchanged install");
    assert_eq!(
        counts.stats,
        1 + 2,
        "one stat for base, two for the fetched arena"
    );
}

/// ADR-8810: `mount` re-checks the file's length: a fetched chunk that changed after the scan
/// (a second writer appended to it) is refused with `chunk_size_mismatch`, reads
/// `Failed(size_mismatch)` and is not mounted.
#[test]
fn mount_refuses_a_chunk_whose_file_changed_length_after_the_scan() {
    let (fx, manifest) = Fixture::new("content-mount-size", &[("arena", &[], 0)]);
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    let fetched = fx.install.join("arena.fpak");
    std::fs::copy(fx.cdn.join("arena.fpak"), &fetched).expect("fetched copy");
    let mut m = fx.manager(&manifest);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    let mut bytes = std::fs::read(&fetched).expect("pak");
    bytes.extend_from_slice(b"appended by another writer");
    std::fs::write(&fetched, &bytes).expect("grow");
    let mut server = fx.server();
    let err = m.mount(&mut server, "arena").expect_err("the length changed");
    assert_eq!(err.reason(), "chunk_size_mismatch", "{err}");
    assert_eq!(
        m.state("arena"),
        Some(ChunkState::Failed(FailReason::SizeMismatch))
    );
    assert!(!server.mounted_pak_names().contains(&"arena".to_owned()));
}

/// ADR-8810: a refused packaged chunk recovers through a good fetched copy: the manager finds the
/// verified copy in the install directory, mounting it lifts the refusal, and its assets load
/// from it (the patch's version, not the base's).
#[test]
fn a_good_fetched_copy_lifts_the_refusal_of_a_torn_packaged_chunk() {
    let (fx, mut manifest) = Fixture::new("content-size-recover", &[]);
    let id = fx.base_id;
    let patch_path = fx.content.join("patch.fpak");
    write_pak(&patch_path, &[(id, "assets/base.mesh", mesh_file(id, 3))]);
    let mut patch = entry(&fx.content, "patch", Delivery::Install, 1, &[]);
    patch.overrides = true;
    manifest.chunks.push(patch);
    manifest.write_to_dir(&fx.content).expect("manifest");
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::copy(&patch_path, fx.install.join("patch.fpak")).expect("good copy");
    let mut torn = std::fs::read(&patch_path).expect("pak");
    torn.truncate(torn.len() - 1);
    std::fs::write(&patch_path, &torn).expect("tear");
    // The cook's chunk map homes the patch's id in the patch (an overriding chunk's own map wins).
    map_ids_to(&fx, &[(id, "patch")]);

    let mut server = fx.server();
    assert_eq!(server.mounted_pak_names(), ["base"]);
    // ADR-8860: a truncated pak's table of contents does not read, so it is not opened at all, but
    // it is listed in the manifest: it is refused and its chunk-map ids are not served by the base.
    assert_eq!(
        server.refused_chunks().get("patch").copied(),
        Some("chunk_size_mismatch"),
        "a truncated listed pak is refused even though it does not open"
    );
    let err = server
        .load_mesh(id)
        .expect_err("the torn patch's id must not load the base's older bytes");
    assert!(err.to_string().contains("chunk_refused"), "{err}");
    let mut m = fx.manager(&manifest);
    assert_eq!(
        m.state("patch"),
        Some(ChunkState::Installed),
        "the verified fetched copy"
    );
    assert_eq!(
        m.chunk_path("patch"),
        Some(fx.install.join("patch.fpak").as_path())
    );
    m.sync_with_server(&mut server, true);
    assert_eq!(server.mounted_pak_names(), ["base", "patch"]);
    let handle = server.load_mesh(id).expect("the patch's mesh");
    assert_eq!(server.meshes().get(&handle).expect("live").indices.len(), 9);
}

/// ADR-8810: a packaged pak with extra bytes (its table of contents still reads) is refused, and
/// mounting a good fetched copy under the chunk's name lifts the refusal of its ids.
#[test]
fn mounting_a_good_copy_lifts_a_refused_chunks_ids() {
    let (fx, mut manifest) = Fixture::new("content-size-lift", &[]);
    let id = fx.base_id;
    let patch_path = fx.content.join("patch.fpak");
    write_pak(&patch_path, &[(id, "assets/base.mesh", mesh_file(id, 3))]);
    let mut patch = entry(&fx.content, "patch", Delivery::Install, 1, &[]);
    patch.overrides = true;
    manifest.chunks.push(patch);
    manifest.write_to_dir(&fx.content).expect("manifest");
    std::fs::create_dir_all(&fx.install).expect("mkdir");
    std::fs::copy(&patch_path, fx.install.join("patch.fpak")).expect("good copy");
    let mut grown = std::fs::read(&patch_path).expect("pak");
    grown.extend_from_slice(&[0u8; 512]);
    std::fs::write(&patch_path, &grown).expect("grow");

    let mut server = fx.server();
    assert_eq!(
        server.refused_chunks().get("patch").copied(),
        Some("chunk_size_mismatch")
    );
    let err = server.load_mesh(id).expect_err("refused");
    assert!(err.to_string().contains("chunk_refused"), "{err}");
    let mut m = fx.manager(&manifest);
    assert_eq!(
        m.state("patch"),
        Some(ChunkState::Installed),
        "the verified fetched copy"
    );
    m.mount(&mut server, "patch").expect("mount the good copy");
    assert!(server.refused_chunks().is_empty(), "the refusal is lifted");
    let handle = server.load_mesh(id).expect("the patch's mesh");
    assert_eq!(server.meshes().get(&handle).expect("live").indices.len(), 9);
}

/// Rewrites the fixture's `chunk-map.bin` with `homes` (id, chunk) over the base's own id.
fn map_ids_to(fx: &Fixture, homes: &[(AssetId, &str)]) {
    let mut map = BTreeMap::new();
    map.insert(fx.base_id, BASE_CHUNK.to_owned());
    for (name, id) in &fx.ids {
        map.insert(*id, name.clone());
    }
    for (id, chunk) in homes {
        map.insert(*id, (*chunk).to_owned());
    }
    ChunkMap::from_assignment(&map)
        .write_to_dir(&fx.content)
        .expect("chunk map");
}

/// A packaged overriding chunk `patch` (mount order 1) holding the base's id as a 3-triangle mesh.
fn packaged_patch(fx: &Fixture, manifest: &mut ContentManifest) -> PathBuf {
    let id = fx.base_id;
    let patch_path = fx.content.join("patch.fpak");
    write_pak(&patch_path, &[(id, "assets/base.mesh", mesh_file(id, 3))]);
    let mut patch = entry(&fx.content, "patch", Delivery::Install, 1, &[]);
    patch.overrides = true;
    manifest.chunks.push(patch);
    manifest.write_to_dir(&fx.content).expect("manifest");
    patch_path
}

/// ADR-8860 (review w7 finding 1): adopting a manifest whose packaged chunk changed its size and
/// hash (an ordinary content update) reads `NeedsUpdate`, not a torn install
/// (`Failed(size_mismatch)`), when no fetched copy exists, and a request fetches the new file.
#[test]
fn adopting_an_update_that_resizes_a_packaged_chunk_reads_needs_update() {
    let (fx, mut manifest) = Fixture::new("content-adopt-resize", &[]);
    let id = fx.base_id;
    packaged_patch(&fx, &mut manifest);
    let mut m = fx.manager(&manifest);
    assert_eq!(m.state("patch"), Some(ChunkState::Installed));
    // The next build of the patch (5 triangles: another length and hash) is on the CDN.
    write_pak(
        &fx.cdn.join("patch.fpak"),
        &[(id, "assets/base.mesh", mesh_file(id, 5))],
    );
    let mut next = manifest.clone();
    next.build_id = "1.0-2-bbbb".into();
    let mut new_patch = entry(&fx.cdn, "patch", Delivery::Install, 1, &[]);
    new_patch.overrides = true;
    let old_size = manifest.chunk("patch").expect("patch").size;
    assert_ne!(new_patch.size, old_size, "the update changes the pak's length");
    for c in &mut next.chunks {
        if c.name == "patch" {
            *c = new_patch.clone();
        }
    }
    let (changed, removed) = m.adopt_manifest(next).expect("adopt");
    assert_eq!(changed, ["patch"]);
    assert!(removed.is_empty());
    assert_eq!(
        m.state("patch").map(|s| s.label()).as_deref(),
        Some("NeedsUpdate"),
        "a changed packaged chunk is an update to fetch, not a torn install"
    );
    assert_eq!(m.chunk_path("patch"), None, "the stale packaged file is not used");
    let rid = m.request(&["patch"], RequestOptions::default()).expect("request");
    let states: Vec<String> = m
        .progress(rid)
        .expect("progress")
        .chunks
        .iter()
        .map(|(_, s, _, _)| s.label())
        .collect();
    assert!(
        states.iter().all(|s| !s.starts_with("Failed")),
        "the request does not start from a failure: {states:?}"
    );
    run(&mut m, 1 << 20, 4, None);
    assert_eq!(m.state("patch"), Some(ChunkState::Installed));
    assert_eq!(
        hash_file(&fx.install.join("patch.fpak")).expect("hash"),
        new_patch.hash,
        "the new build was fetched"
    );
}

/// ADR-8860 control: a real torn install (the manifest's entry for the chunk is unchanged, the
/// file on disk is the wrong length) still reads `Failed(size_mismatch)` after a manifest update.
#[test]
fn a_torn_packaged_chunk_still_reads_size_mismatch_after_an_unrelated_update() {
    let (fx, mut manifest) = Fixture::new("content-adopt-torn", &[]);
    let patch_path = packaged_patch(&fx, &mut manifest);
    let mut m = fx.manager(&manifest);
    assert_eq!(m.state("patch"), Some(ChunkState::Installed));
    let mut bytes = std::fs::read(&patch_path).expect("pak");
    bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(&patch_path, &bytes).expect("tear");
    let mut next = manifest.clone();
    next.build_id = "1.0-2-bbbb".into();
    let (changed, _) = m.adopt_manifest(next).expect("adopt");
    assert!(changed.is_empty(), "the patch's entry did not change");
    assert_eq!(
        m.state("patch").map(|s| s.label()).as_deref(),
        Some("Failed(size_mismatch)")
    );
}

/// ADR-8860 (review w7 finding 2): a truncated chunk pak (its table of contents does not read) is
/// refused at mount, and the ids its chunk map lists fail with `chunk_refused` on the main-thread
/// loaders and the worker reader alike, instead of the base pak serving their older bytes.
#[test]
fn a_truncated_chunk_pak_never_serves_the_base_paks_older_bytes() {
    let (fx, mut manifest) = Fixture::new("content-truncated-refused", &[]);
    let id = fx.base_id;
    let patch_path = packaged_patch(&fx, &mut manifest);
    map_ids_to(&fx, &[(id, "patch")]);
    let mut torn = std::fs::read(&patch_path).expect("pak");
    torn.truncate(torn.len() / 2);
    std::fs::write(&patch_path, &torn).expect("tear");

    let mut server = fx.server();
    assert_eq!(server.mounted_pak_names(), ["base"]);
    assert_eq!(
        server.refused_chunks().get("patch").copied(),
        Some("chunk_size_mismatch")
    );
    let err = server
        .load_mesh(id)
        .expect_err("the truncated patch's id must not load the base's older mesh");
    assert!(err.to_string().contains("chunk_refused"), "{err}");
    let err = server
        .payload_reader()
        .read(id)
        .expect_err("the worker reader follows the same rule");
    assert!(err.to_string().contains("chunk_refused"), "{err}");
    let m = fx.manager(&manifest);
    assert_eq!(
        m.state("patch").map(|s| s.label()).as_deref(),
        Some("Failed(size_mismatch)"),
        "the manager and the server agree"
    );
}

/// ADR-8860: a listed pak of the manifest's length that does not open (its header is damaged) is
/// refused with `chunk_unreadable`, and its chunk-map ids are not served by the base.
#[test]
fn a_listed_pak_that_does_not_open_is_refused_as_unreadable() {
    let (fx, mut manifest) = Fixture::new("content-unreadable-refused", &[]);
    let id = fx.base_id;
    let patch_path = packaged_patch(&fx, &mut manifest);
    map_ids_to(&fx, &[(id, "patch")]);
    let mut bytes = std::fs::read(&patch_path).expect("pak");
    bytes[..8].copy_from_slice(b"NOTAPAK!");
    std::fs::write(&patch_path, &bytes).expect("damage");

    let mut server = fx.server();
    assert_eq!(server.mounted_pak_names(), ["base"]);
    assert_eq!(
        server.refused_chunks().get("patch").copied(),
        Some("chunk_unreadable")
    );
    let err = server.load_mesh(id).expect_err("refused");
    assert!(err.to_string().contains("chunk_refused"), "{err}");
    assert!(err.to_string().contains("chunk_unreadable"), "{err}");
}

/// ADR-8860 (review w7 finding 3): an id a refused chunk lists that a newer good pak also holds
/// is served by that newer pak; only an older pak's copy is refused.
#[test]
fn a_newer_good_pak_serves_an_id_a_refused_chunk_lists() {
    let (fx, mut manifest) = Fixture::new("content-refused-newer", &[]);
    let id = fx.base_id;
    let patch_path = packaged_patch(&fx, &mut manifest);
    let hotfix_path = fx.content.join("hotfix.fpak");
    write_pak(&hotfix_path, &[(id, "assets/base.mesh", mesh_file(id, 4))]);
    let mut hotfix = entry(&fx.content, "hotfix", Delivery::Install, 2, &[]);
    hotfix.overrides = true;
    manifest.chunks.push(hotfix);
    manifest.write_to_dir(&fx.content).expect("manifest");
    let mut bytes = std::fs::read(&patch_path).expect("pak");
    bytes.extend_from_slice(&[0u8; 256]);
    std::fs::write(&patch_path, &bytes).expect("grow");

    let mut server = fx.server();
    assert_eq!(server.mounted_pak_names(), ["base", "hotfix"]);
    assert_eq!(
        server.refused_chunks().get("patch").copied(),
        Some("chunk_size_mismatch")
    );
    let handle = server
        .load_mesh(id)
        .expect("the newer hotfix pak serves the id the refused patch also lists");
    assert_eq!(
        server.meshes().get(&handle).expect("live").indices.len(),
        12,
        "the hotfix's 4 triangles"
    );
    let (_, source) = server.payload_reader().read(id).expect("worker reader too");
    assert_eq!(source, regia_asset::AssetSource::Pak("hotfix".into()));
}

/// A valid installed pak may have been replaced after scan; neither its name nor its state
/// establishes that it is the manifest's bytes (ADR-7871).
#[test]
fn content_mount_refuses_a_same_size_swapped_pak() {
    let (fx, manifest) = Fixture::new("content-mount-swap", &[("arena", &[], 0)]);
    let installed = fx.content.join("arena.fpak");
    let original = std::fs::read(fx.cdn.join("arena.fpak")).expect("original");
    std::fs::write(&installed, &original).expect("install");
    let mut manager = fx.manager(&manifest);
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: fx.root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server without startup mounts");
    // Swap in another well-formed pak with a self-consistent header/index and the same size.
    // A header-only check would miss a replacement whose author rebuilt that header too.
    let id = fx.ids["arena"];
    let mut payload = mesh_file(id, 2);
    *payload.last_mut().expect("mesh bytes") ^= 1;
    let replacement = fx.root.join("replacement.fpak");
    write_pak(&replacement, &[(id, "assets/arena/a.mesh", payload)]);
    regia_asset::PakReader::open(&replacement).expect("valid replacement pak");
    let swapped = std::fs::read(&replacement).expect("replacement");
    assert_eq!(swapped.len(), original.len());
    assert_ne!(swapped, original);
    regia_test_utils::write_atomic(&installed, &swapped).expect("replace");
    let err = manager.mount(&mut server, "arena").expect_err("swapped pak");
    assert_eq!(err.reason(), "chunk_integrity_mismatch");
    assert_eq!(
        manager.state("arena"),
        Some(ChunkState::Failed(FailReason::HashMismatch))
    );
    assert!(server.mounted_pak_names().is_empty());
    assert!(
        installed.is_file(),
        "preserve the rejected installation for diagnosis"
    );
}

#[test]
fn content_mount_refuses_a_truncated_installed_pak() {
    let (fx, manifest) = Fixture::new("content-mount-truncated", &[("arena", &[], 0)]);
    let installed = fx.content.join("arena.fpak");
    let mut bytes = std::fs::read(fx.cdn.join("arena.fpak")).expect("original");
    std::fs::write(&installed, &bytes).expect("install");
    let mut manager = fx.manager(&manifest);
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: fx.root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    bytes.truncate(bytes.len() - 1);
    regia_test_utils::write_atomic(&installed, &bytes).expect("truncate");
    let err = manager.mount(&mut server, "arena").expect_err("truncated pak");
    // A wrong length is a torn install: the mount's length check (ADR-8810) runs before the full
    // BLAKE3 (ADR-7871) and names it, the same code and state the scan gives (ADR-8714).
    assert_eq!(err.reason(), "chunk_size_mismatch");
    assert_eq!(
        manager.state("arena"),
        Some(ChunkState::Failed(FailReason::SizeMismatch))
    );
    assert!(server.mounted_pak_names().is_empty());
}

#[test]
fn content_startup_refuses_a_manifest_pak_with_changed_payload() {
    let (fx, _manifest) = Fixture::new("content-startup-swap", &[]);
    let installed = fx.content.join("base.fpak");
    let mut bytes = std::fs::read(&installed).expect("original");
    bytes[FPAK_BLOB_ALIGNMENT as usize + 60] ^= 1;
    regia_test_utils::write_atomic(&installed, &bytes).expect("replace");
    // The open itself fails: an empty mount set behind an `Ok` would boot a damaged install
    // with exit code 0 (ADR-7979).
    let opened = AssetServer::open(AssetServerSettings {
        project_root: fx.root.clone(),
        platform: "linux".into(),
        content_dir: Some(fx.content.clone()),
    });
    match opened {
        Err(regia_asset::AssetLoadError::Content(error)) => {
            assert_eq!(error.reason(), "chunk_integrity_mismatch");
        }
        Err(other) => panic!("wrong error for a changed manifest pak: {other}"),
        Ok(server) => panic!(
            "a startup mount must refuse a manifest pak that fails verification (mounted {:?})",
            server.mounted_pak_names()
        ),
    }
}

#[test]
fn content_mount_checks_size_even_when_the_manifest_hash_matches() {
    let (fx, mut manifest) = Fixture::new("content-mount-wrong-size", &[("arena", &[], 0)]);
    std::fs::copy(fx.cdn.join("arena.fpak"), fx.content.join("arena.fpak")).expect("install");
    manifest
        .chunks
        .iter_mut()
        .find(|c| c.name == "arena")
        .expect("entry")
        .size += 1;
    let mut manager = fx.manager(&manifest);
    // The scan's length check (ADR-8810) already refuses the file, before any hash: the manifest
    // hash matching does not make a wrong length installed (ADR-8714).
    assert_eq!(
        manager.state("arena"),
        Some(ChunkState::Failed(FailReason::SizeMismatch))
    );
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: fx.root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    let err = manager
        .mount(&mut server, "arena")
        .expect_err("manifest size mismatch");
    assert_eq!(err.reason(), "chunk_not_installed");
    assert!(server.mounted_pak_names().is_empty());
}

#[test]
fn asset_plugin_exits_with_an_error_on_a_changed_manifest_pak() {
    use regia_app::prelude::*;
    let (fx, _manifest) = Fixture::new("content-plugin-swap", &[]);
    let installed = fx.content.join("base.fpak");
    let mut bytes = std::fs::read(&installed).expect("original");
    bytes[FPAK_BLOB_ALIGNMENT as usize + 60] ^= 1;
    regia_test_utils::write_atomic(&installed, &bytes).expect("replace");
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        regia_asset::AssetPlugin {
            settings: AssetServerSettings {
                project_root: fx.root.clone(),
                platform: "linux".into(),
                content_dir: Some(fx.content.clone()),
            },
        },
    ));
    app.update();
    assert!(app.world().get_resource::<AssetServer>().is_none());
    // A damaged install ends with an error exit code (ADR-7979), not a run without its content.
    let exit = app.should_exit();
    assert!(matches!(exit, Some(AppExit::Error(_))), "{exit:?}");
}
