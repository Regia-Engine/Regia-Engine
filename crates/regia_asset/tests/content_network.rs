// SPDX-License-Identifier: MPL-2.0
//! The platform metered-network query behind `ContentManager` (spec 40 § 2.5, ADR-7236) with a
//! host-side fake probe, and the `content-state` format's text form (spec 40 § 4).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use regia_asset::content::{
    BASE_CHUNK, CONTENT_STATE_FORMAT_VERSION, ChunkEntry, ChunkState, ContentFault, ContentManager,
    ContentManifest, ContentState, Delivery, DirSource, InstalledChunk, METERED_POLL_TICKS, MeteredProbe,
    RequestOptions, chunk_map_schema_fingerprint, content_state_schema_fingerprint, hash_file,
};

/// A fake platform answer: 0 = cannot tell, 1 = unmetered, 2 = metered; counts the calls.
#[derive(Clone, Default)]
struct FakeNetwork {
    answer: Arc<AtomicU8>,
    calls: Arc<AtomicUsize>,
}

impl FakeNetwork {
    fn set(&self, metered: Option<bool>) {
        let v = match metered {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        self.answer.store(v, Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn probe(&self) -> MeteredProbe {
        let me = self.clone();
        Arc::new(move || {
            me.calls.fetch_add(1, Ordering::SeqCst);
            match me.answer.load(Ordering::SeqCst) {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            }
        })
    }
}

struct Fixture {
    root: PathBuf,
    content: PathBuf,
    cdn: PathBuf,
    install: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn chunk_file(dir: &Path, name: &str, len: usize) -> ChunkEntry {
    std::fs::create_dir_all(dir).expect("dir");
    let file = format!("{name}.fpak");
    let path = dir.join(&file);
    std::fs::write(&path, vec![name.len() as u8; len]).expect("write chunk");
    ChunkEntry {
        name: name.to_owned(),
        file,
        size: len as u64,
        hash: hash_file(&path).expect("hash"),
        delivery: Delivery::OnDemand,
        requires: Vec::new(),
        overrides: false,
        mount_order: 1,
        assets: 1,
        blocks: Vec::new(),
    }
}

/// A build with `base` in the package and `arena` (3000 bytes) on a directory host.
fn fixture(tag: &str) -> (Fixture, ContentManager) {
    let root = regia_test_utils::unique_temp_dir(tag);
    let fx = Fixture {
        content: root.join("content"),
        cdn: root.join("cdn"),
        install: root.join("install"),
        root,
    };
    let mut manifest = ContentManifest::new("1.0-1-net", "linux");
    let mut base = chunk_file(&fx.content, BASE_CHUNK, 64);
    base.delivery = Delivery::Install;
    base.mount_order = 0;
    manifest.chunks.push(base);
    manifest.chunks.push(chunk_file(&fx.cdn, "arena", 3000));
    let mut m = ContentManager::from_manifest(manifest, &fx.content, &fx.install);
    m.add_source(Box::new(DirSource::new(&fx.cdn)));
    (fx, m)
}

fn run(m: &mut ContentManager, steps: usize) {
    for _ in 0..steps {
        m.advance(1 << 20, None);
    }
}

/// A metered answer from the platform holds a new request in `WaitingForUnmetered` (the probe is
/// asked when the request is made); when the platform reports unmetered, the next poll releases it.
#[test]
fn a_metered_platform_holds_requests_and_unmetered_releases_them() {
    let (_fx, mut m) = fixture("net-metered");
    let net = FakeNetwork::default();
    net.set(Some(true));
    m.set_metered_probe(Some(net.probe()));
    let req = m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, 3);
    assert_eq!(m.state("arena"), Some(ChunkState::WaitingForUnmetered));
    assert_eq!(m.progress(req).expect("progress").state, "WaitingForUnmetered");
    let cost = m.network();
    assert_eq!(
        (cost.platform, cost.metered, cost.forced),
        (Some(true), true, false)
    );

    net.set(Some(false));
    // The probe is polled every METERED_POLL_TICKS busy advances, never more often.
    let before = net.calls();
    run(&mut m, METERED_POLL_TICKS as usize + 3);
    assert!(net.calls() > before, "the probe was polled again");
    assert!(
        net.calls() - before <= 2,
        "at most one poll per {METERED_POLL_TICKS} advances: {}",
        net.calls() - before
    );
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    assert!(!m.network().metered);
}

/// A platform that cannot tell (desktop without a network-cost API) reads as unmetered (spec 40
/// § 2.5), and an idle manager never calls the probe (the per-frame budget of an idle manager).
#[test]
fn an_unknown_network_reads_as_unmetered_and_idle_frames_never_probe() {
    let (_fx, mut m) = fixture("net-unknown");
    let net = FakeNetwork::default();
    net.set(None);
    m.set_metered_probe(Some(net.probe()));
    let after_set = net.calls();
    run(&mut m, 5 * METERED_POLL_TICKS as usize);
    assert_eq!(net.calls(), after_set, "no request, no probe call");
    m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, 3);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    let cost = m.network();
    assert_eq!((cost.platform, cost.metered), (None, false));
}

/// An injected `metered` fault (debug tool, `content.inject`) stays metered whatever the platform
/// says, and approval still lets the request through.
#[test]
fn an_injected_metered_fault_is_not_cleared_by_the_platform() {
    let (_fx, mut m) = fixture("net-forced");
    let net = FakeNetwork::default();
    net.set(Some(false));
    m.set_metered_probe(Some(net.probe()));
    m.inject("arena", ContentFault::Metered, 0).expect("inject");
    let req = m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, METERED_POLL_TICKS as usize + 2);
    assert_eq!(m.state("arena"), Some(ChunkState::WaitingForUnmetered));
    let cost = m.network();
    assert_eq!(
        (cost.platform, cost.metered, cost.forced),
        (Some(false), true, true)
    );
    m.approve_metered(req).expect("approve");
    run(&mut m, 3);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
}

/// Without a probe the manager keeps today's behaviour: `set_metered` alone decides, and the
/// report says no platform answer was read.
#[test]
fn without_a_probe_set_metered_decides() {
    let (_fx, mut m) = fixture("net-none");
    assert_eq!(m.network().platform, None);
    assert!(!m.network().probed);
    m.set_metered(true);
    m.request(&["arena"], RequestOptions::default()).expect("request");
    run(&mut m, 2);
    assert_eq!(m.state("arena"), Some(ChunkState::WaitingForUnmetered));
    m.set_metered(false);
    run(&mut m, 3);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
}

/// The platform query this build answers with: on desktop hosts there is no network-cost API, so
/// it is "cannot tell" (`Ok(None)`), never an error.
#[cfg(not(target_os = "android"))]
#[test]
fn the_desktop_platform_cannot_tell() {
    assert_eq!(regia_platform::network::active_network_metered().ok(), Some(None));
    assert_eq!((regia_asset::content::platform_metered_probe())(), None);
}

/// `content-state.ron` has its own format row: a text form that round-trips, refuses a newer
/// version, and a fingerprint of the install record alone.
#[test]
fn content_state_text_round_trips_and_refuses_newer_versions() {
    let mut state = ContentState::default();
    state.chunks.insert(
        "arena".into(),
        InstalledChunk {
            build_id: "1.0-1-net".into(),
            hash: "ab".repeat(32),
            size: 3000,
            modified: 1_700_000_000,
        },
    );
    let text = state.to_ron_string().expect("text");
    assert_eq!(ContentState::from_ron_str(&text).expect("parse"), state);
    let newer = text.replace(
        &format!("format_version: {CONTENT_STATE_FORMAT_VERSION}"),
        &format!("format_version: {}", CONTENT_STATE_FORMAT_VERSION + 1),
    );
    assert_ne!(newer, text);
    assert_eq!(
        ContentState::from_ron_str(&newer).expect_err("newer").reason(),
        "manifest_version"
    );
    assert!(ContentState::from_ron_str("not ron").is_err());
    let fp = content_state_schema_fingerprint();
    assert!(fp.contains("build_id") && fp.contains("chunks"), "{fp}");
    assert!(!fp.contains("RGCHMAP"));
    assert_ne!(fp, chunk_map_schema_fingerprint());
}
