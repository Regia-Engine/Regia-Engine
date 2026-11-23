// SPDX-License-Identifier: MPL-2.0
//! Signed content manifests (ADR-2710, spec 40 § 11): a game that trusts Ed25519 keys checks the
//! detached `content-manifest.ron.sig` a host serves next to a manifest before it adopts the manifest.
//! Every failure keeps the running manifest, its chunk states and its sources. The host is the
//! loopback static server (`serve_dir_loopback`, plain `http://` to this machine), the same
//! harness as the content client tests; the `https://` transport is `content_https.rs`.

#![cfg(feature = "content-signing")]

use std::path::{Path, PathBuf};

use regia_asset::content::{
    CONTENT_MANIFEST_FILE, CONTENT_SIGNATURE_FILE, CONTENT_TRUST_FILE, ChunkEntry, ChunkState,
    ContentManager, ContentManifest, ContentSettings, ContentTrust, Delivery, HttpClient, HttpSource,
    RequestOptions, SignatureCheck, SignaturePolicy, SigningKey, hash_file, serve_dir_loopback,
};

const RUNNING: &str = "1.0-1-run";
const OFFERED: &str = "1.0-2-new";

struct Fixture {
    root: PathBuf,
    content: PathBuf,
    install: PathBuf,
    host: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir(tag);
        let fx = Self {
            content: root.join("content"),
            install: root.join("install"),
            host: root.join("host"),
            root,
        };
        for dir in [&fx.content, &fx.install, &fx.host.join(OFFERED)] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        std::fs::write(fx.content.join("base.fpak"), b"base pak").expect("base");
        fx
    }

    fn entry(dir: &Path, name: &str, delivery: Delivery) -> ChunkEntry {
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
    }

    /// The running manifest: base plus an `arena` chunk at version 1. It accepts [`OFFERED`].
    fn running(&self) -> ContentManifest {
        let served = self.host.join(RUNNING);
        std::fs::create_dir_all(&served).expect("mkdir");
        std::fs::write(served.join("arena.fpak"), b"arena v1").expect("arena");
        let mut m = ContentManifest::new(RUNNING, "linux");
        m.compatible_with.push(OFFERED.into());
        m.chunks
            .push(Self::entry(&self.content, "base", Delivery::Install));
        m.chunks.push(Self::entry(&served, "arena", Delivery::OnDemand));
        m
    }

    /// Publishes the offered build (arena at version 2) and returns the manifest bytes as served.
    fn publish(&self) -> Vec<u8> {
        let served = self.host.join(OFFERED);
        std::fs::write(served.join("arena.fpak"), b"arena v2, longer").expect("arena");
        let mut m = ContentManifest::new(OFFERED, "linux");
        m.compatible_with.push(RUNNING.into());
        m.chunks
            .push(Self::entry(&self.content, "base", Delivery::Install));
        m.chunks.push(Self::entry(&served, "arena", Delivery::OnDemand));
        m.write_to_dir(&served).expect("manifest");
        std::fs::write(self.host.join("latest.ron"), format!("\"{OFFERED}\"\n")).expect("latest");
        std::fs::read(served.join(CONTENT_MANIFEST_FILE)).expect("read back")
    }

    fn write_signature(&self, key: &SigningKey, manifest: &[u8]) {
        let sig = key.sign_manifest(manifest).to_ron_string().expect("ron");
        std::fs::write(self.host.join(OFFERED).join(CONTENT_SIGNATURE_FILE), sig).expect("sig");
    }

    fn manager(&self, trust: Option<ContentTrust>) -> ContentManager {
        let mut m = ContentManager::from_manifest(self.running(), &self.content, &self.install);
        m.set_trust(trust);
        m
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn key() -> SigningKey {
    SigningKey::generate().expect("keygen").0
}

fn trust(policy: SignaturePolicy, keys: &[&SigningKey]) -> Option<ContentTrust> {
    Some(ContentTrust::new(policy, keys.iter().map(|k| k.public_key()).collect()).expect("trust"))
}

/// The running manifest and every chunk state are what they were before a refused update.
fn assert_kept(manager: &ContentManager, before: &ContentManifest, states: &[(String, Option<ChunkState>)]) {
    assert_eq!(manager.manifest(), before, "the running manifest is kept");
    for (name, state) in states {
        assert_eq!(&manager.state(name), state, "{name} state is kept");
    }
}

fn states(m: &ContentManager) -> Vec<(String, Option<ChunkState>)> {
    ["base", "arena"]
        .iter()
        .map(|n| ((*n).to_owned(), m.state(n)))
        .collect()
}

#[test]
fn a_signed_manifest_round_trips_and_is_adopted() {
    let fx = Fixture::new("content-trust-roundtrip");
    let key = key();
    // The key file text reads back to the same key.
    let (again, text) = SigningKey::generate().expect("keygen");
    assert_eq!(
        SigningKey::from_key_file(&text).expect("reads").public_key(),
        again.public_key()
    );
    let bytes = fx.publish();
    fx.write_signature(&key, &bytes);
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let mut manager = fx.manager(trust(SignaturePolicy::Required, &[&key]));
    manager.add_source(Box::new(HttpSource::new(&url, RUNNING).expect("source")));
    let update = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect("signed update");
    assert_eq!(manager.source_names(), vec!["http", "local"]);
    assert_eq!(
        update.signature,
        SignatureCheck::Verified { key_id: key.key_id() }
    );
    assert_eq!(
        (update.previous_build_id.as_str(), update.build_id.as_str()),
        (RUNNING, OFFERED)
    );
    assert_eq!(update.changed, vec!["arena".to_owned()]);
    assert_eq!(manager.manifest().build_id, OFFERED);
    // The http source now points at the new build's folder: the new arena file comes from there
    // and verifies against the adopted hash.
    let id = manager
        .request(&["arena"], RequestOptions::default())
        .expect("request");
    manager.advance(0, None);
    manager.wait_for_transfers();
    manager.advance(0, None);
    assert_eq!(
        manager.state("arena"),
        Some(ChunkState::Installed),
        "{:?}",
        manager.progress(id)
    );
    assert_eq!(
        std::fs::read(fx.install.join("arena.fpak")).expect("installed"),
        b"arena v2, longer"
    );
}

#[test]
fn a_tampered_manifest_is_refused_and_the_old_one_kept() {
    let fx = Fixture::new("content-trust-tampered-manifest");
    let key = key();
    let bytes = fx.publish();
    fx.write_signature(&key, &bytes);
    // The host (or a CDN in the middle) changes one chunk hash after signing.
    let mut text = String::from_utf8(bytes).expect("utf8");
    let arena = text.find("hash: \"").expect("a hash") + 7;
    let flipped = if &text[arena..=arena] == "0" { "1" } else { "0" };
    text.replace_range(arena..=arena, flipped);
    std::fs::write(fx.host.join(OFFERED).join(CONTENT_MANIFEST_FILE), text).expect("tamper");
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let mut manager = fx.manager(trust(SignaturePolicy::Required, &[&key]));
    let before = manager.manifest().clone();
    let before_states = states(&manager);
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("tampered");
    assert_eq!(err.reason(), "content_signature_invalid", "{err}");
    assert_kept(&manager, &before, &before_states);
}

#[test]
fn a_tampered_signature_is_refused_and_the_old_one_kept() {
    let fx = Fixture::new("content-trust-tampered-sig");
    let key = key();
    let bytes = fx.publish();
    let mut sig = key.sign_manifest(&bytes);
    let first = if sig.signature.starts_with('0') { "1" } else { "0" };
    sig.signature.replace_range(0..1, first);
    std::fs::write(
        fx.host.join(OFFERED).join(CONTENT_SIGNATURE_FILE),
        sig.to_ron_string().expect("ron"),
    )
    .expect("sig");
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let mut manager = fx.manager(trust(SignaturePolicy::Optional, &[&key]));
    let before = manager.manifest().clone();
    let before_states = states(&manager);
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("tampered signature");
    assert_eq!(err.reason(), "content_signature_invalid", "{err}");
    assert_kept(&manager, &before, &before_states);
    // Garbage in the signature file is the same error, never a panic or an adoption.
    std::fs::write(
        fx.host.join(OFFERED).join(CONTENT_SIGNATURE_FILE),
        b"\xff\x00 not ron",
    )
    .expect("junk");
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("junk signature");
    assert_eq!(err.reason(), "content_signature_invalid", "{err}");
    assert_kept(&manager, &before, &before_states);
}

#[test]
fn a_signature_by_an_untrusted_key_is_refused_and_rotation_accepts_the_next_key() {
    let fx = Fixture::new("content-trust-wrong-key");
    let (trusted, other) = (key(), key());
    let bytes = fx.publish();
    fx.write_signature(&other, &bytes);
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let mut manager = fx.manager(trust(SignaturePolicy::Required, &[&trusted]));
    let before = manager.manifest().clone();
    let before_states = states(&manager);
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("wrong key");
    assert_eq!(err.reason(), "content_signature_invalid", "{err}");
    assert!(err.to_string().contains(&other.key_id()), "{err}");
    assert_kept(&manager, &before, &before_states);
    // Rotation: the game trusts the old and the new key; the host signs with the new one.
    manager.set_trust(trust(SignaturePolicy::Required, &[&trusted, &other]));
    let update = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect("rotated key");
    assert_eq!(
        update.signature,
        SignatureCheck::Verified {
            key_id: other.key_id()
        }
    );
}

#[test]
fn a_missing_signature_follows_the_policy() {
    let fx = Fixture::new("content-trust-missing");
    let key = key();
    fx.publish();
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let mut manager = fx.manager(trust(SignaturePolicy::Required, &[&key]));
    let before = manager.manifest().clone();
    let before_states = states(&manager);
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("unsigned");
    assert_eq!(err.reason(), "content_signature_missing", "{err}");
    assert_kept(&manager, &before, &before_states);
    // Optional: an unsigned manifest is adopted and reported as such.
    manager.set_trust(trust(SignaturePolicy::Optional, &[&key]));
    let update = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect("optional");
    assert_eq!(update.signature, SignatureCheck::Unsigned);
    // A game that trusts no key keeps today's behaviour (TLS only).
    let mut plain = fx.manager(None);
    let update = plain
        .update_from_host(&HttpClient::new(), &url)
        .expect("not configured");
    assert_eq!(update.signature, SignatureCheck::NotConfigured);
}

#[test]
fn a_broken_trust_record_refuses_every_update() {
    let fx = Fixture::new("content-trust-broken-record");
    let key = key();
    let bytes = fx.publish();
    fx.write_signature(&key, &bytes);
    fx.running().write_to_dir(&fx.content).expect("packaged manifest");
    std::fs::write(
        fx.content.join(CONTENT_TRUST_FILE),
        "ContentTrust(format_version: 1, policy: Required, keys: [\"ed25519:00\"])",
    )
    .expect("broken");
    let mut settings = ContentSettings::for_content_dir(&fx.content);
    settings.install_dir = fx.install.clone();
    settings.background_verify = false;
    let mut manager = ContentManager::open(&settings).expect("opens");
    assert!(manager.trust_error().is_some());
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("fails closed");
    assert_eq!(err.reason(), "content_signature_invalid", "{err}");
    assert_eq!(manager.manifest().build_id, RUNNING);
    // A good record is read at open.
    ContentTrust::new(SignaturePolicy::Required, vec![key.public_key()])
        .expect("trust")
        .write_to_dir(&fx.content)
        .expect("write");
    let mut manager = ContentManager::open(&settings).expect("opens");
    assert_eq!(manager.trust().map(|t| t.key_ids()), Some(vec![key.key_id()]));
    manager
        .update_from_host(&HttpClient::new(), &url)
        .expect("verified");
    assert_eq!(manager.manifest().build_id, OFFERED);
}

#[test]
fn an_update_waits_for_chunks_in_flight() {
    let fx = Fixture::new("content-trust-in-flight");
    let bytes = fx.publish();
    let key = key();
    fx.write_signature(&key, &bytes);
    let (url, _) = serve_dir_loopback(fx.host.clone()).expect("server");
    let mut manager = fx.manager(trust(SignaturePolicy::Required, &[&key]));
    manager
        .request(&["arena"], RequestOptions::default())
        .expect("request");
    let before = manager.manifest().clone();
    let err = manager
        .update_from_host(&HttpClient::new(), &url)
        .expect_err("in flight");
    assert_eq!(err.reason(), "chunk_in_use", "{err}");
    assert_eq!(manager.manifest(), &before);
}

/// The signature envelope is untrusted bytes from a host: no mutation of a real signature file
/// panics or hangs, and a signature check over a mutated file never verifies unless the mutation
/// left the record's fields unchanged (a reformatting). Same for the trust record.
#[test]
fn signature_and_trust_parsers_never_panic_on_mutations() {
    use regia_asset::content::{ContentSignature, check_manifest_signature};
    let key = key();
    let manifest =
        b"ContentManifest(format_version: 1, build_id: \"x\", platform: \"linux\", chunks: [])".to_vec();
    let good = key.sign_manifest(&manifest);
    let fixture = good.to_ron_string().expect("ron").into_bytes();
    let rejected = regia_test_utils::fuzz::fuzz_loader(&fixture, 0x2710, 3_000, ContentSignature::from_bytes);
    assert!(rejected > 0);
    // Every single-byte change of the record fails to parse or verify, or leaves the record's
    // fields unchanged.
    let trust = ContentTrust::new(SignaturePolicy::Required, vec![key.public_key()]).expect("trust");
    for i in 0..fixture.len() {
        for flip in [1u8, 0x20, 0x80] {
            let mut mutated = fixture.clone();
            mutated[i] ^= flip;
            match check_manifest_signature(Some(&trust), &manifest, Some(&mutated)) {
                Ok(_) => assert_eq!(
                    ContentSignature::from_bytes(&mutated).ok().as_ref(),
                    Some(&good),
                    "byte {i} flip {flip:#x} verified"
                ),
                Err(e) => assert_eq!(e.reason(), "content_signature_invalid", "byte {i}"),
            }
        }
    }
    let trust_fixture = ContentTrust::new(SignaturePolicy::Optional, vec![key.public_key()])
        .expect("trust")
        .to_ron_string()
        .expect("ron")
        .into_bytes();
    regia_test_utils::fuzz::fuzz_loader(&trust_fixture, 0x2712, 3_000, |b: &[u8]| {
        std::str::from_utf8(b)
            .map_err(|e| e.to_string())
            .and_then(|t| ContentTrust::from_ron_str(t).map_err(|e| e.to_string()))
    });
}
