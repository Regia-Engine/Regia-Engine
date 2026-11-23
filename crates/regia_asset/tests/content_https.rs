// SPDX-License-Identifier: MPL-2.0
//! `https://` content sources (spec 40 § 2.6, § 8 test 19 TLS half, ADR-1876): a chunks folder
//! served over rustls on loopback with a self-signed certificate that only the test's ureq agent
//! trusts. The manifest is fetched over TLS, an interrupted transfer resumes with a `Range`
//! request (only the tail crosses the wire), the file verifies, and a client that does not trust
//! the certificate fails with `source_unreachable`. Builds only with the `content-http` feature.

#![cfg(feature = "content-http")]

use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use regia_asset::content::{
    ChunkEntry, ChunkState, ContentManager, ContentManifest, Delivery, FailReason, HttpClient, HttpSource,
    RequestOptions, ServeStats, fetch_manifest_with, hash_file, serve_connection,
};

/// A TLS static host over `root` on `127.0.0.1:<port>` and a client that trusts only its
/// certificate. Returns (`https://localhost:<port>`, stats, trusting client).
fn tls_host(root: &Path) -> (String, Arc<ServeStats>, HttpClient) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("self-signed cert");
    let cert_der = certified.cert.der().clone();
    let key_der: rustls::pki_types::PrivateKeyDer<'static> =
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("protocols")
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server config"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stats = Arc::new(ServeStats::default());
    let thread_stats = Arc::clone(&stats);
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = Arc::clone(&server_config);
            let stats = Arc::clone(&thread_stats);
            let root = root.clone();
            std::thread::spawn(move || {
                let Ok(conn) = rustls::server::ServerConnection::new(config) else {
                    return;
                };
                let mut tls = rustls::StreamOwned::new(conn, stream);
                // A client that rejects the certificate closes mid-handshake: not an error here.
                let _ = serve_connection(&mut tls, &root, &stats);
                tls.conn.send_close_notify();
                let _ = std::io::Write::flush(&mut tls);
            });
        }
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("root store");
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let agent = ureq::AgentBuilder::new()
        .tls_config(Arc::new(client_config))
        .build();
    (
        format!("https://localhost:{port}"),
        stats,
        HttpClient::with_agent(agent),
    )
}

#[test]
fn https_source_fetches_manifest_resumes_and_verifies() {
    let root = regia_test_utils::unique_temp_dir("content-https");
    let content = root.join("content");
    let install = root.join("install");
    let host = root.join("host");
    std::fs::create_dir_all(&content).expect("mkdir");
    std::fs::write(content.join("base.fpak"), b"base pak bytes").expect("base");

    // The chunk: deterministic bytes, larger than one socket read.
    let chunk_bytes: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let build = "1.0-1-tls";
    let served = host.join(build);
    std::fs::create_dir_all(&served).expect("mkdir");
    std::fs::write(served.join("arena.fpak"), &chunk_bytes).expect("chunk");
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
    let mut manifest = ContentManifest::new(build, "linux");
    manifest.chunks.push(entry("base", &content, Delivery::Install));
    manifest.chunks.push(entry("arena", &served, Delivery::OnDemand));
    manifest.write_to_dir(&served).expect("host manifest");
    std::fs::write(host.join("latest.ron"), format!("\"{build}\"\n")).expect("latest");

    let (url, stats, client) = tls_host(&host);

    // 1. The manifest comes over TLS and passes the platform/build checks.
    let fetched = fetch_manifest_with(&client, &url, &manifest).expect("manifest over https");
    assert_eq!(fetched, manifest);

    // 2. An interrupted transfer left the first 150,000 bytes; the resume fetches only the tail.
    std::fs::create_dir_all(&install).expect("mkdir");
    std::fs::write(install.join("arena.fpak.part"), &chunk_bytes[..150_000]).expect("part");
    let before = stats.bytes.load(Ordering::Relaxed);
    let mut m = ContentManager::from_manifest(manifest.clone(), &content, &install);
    m.add_source(Box::new(
        HttpSource::with_client(&url, build, client.clone()).expect("https source"),
    ));
    assert!(m.source_names().contains(&"http"));
    let req = m.request(&["arena"], RequestOptions::default()).expect("request");
    m.advance(0, None);
    assert!(matches!(m.state("arena"), Some(ChunkState::Downloading { .. })));
    m.wait_for_transfers();
    m.advance(0, None);
    assert_eq!(m.state("arena"), Some(ChunkState::Installed));
    assert_eq!(m.progress(req).expect("progress").state, "Installed");
    assert_eq!(
        stats.bytes.load(Ordering::Relaxed) - before,
        chunk_bytes.len() as u64 - 150_000,
        "only the missing tail came over TLS"
    );
    assert_eq!(
        std::fs::read(install.join("arena.fpak")).expect("installed"),
        chunk_bytes
    );

    // 3. The default client trusts the web PKI roots only: the self-signed host is refused.
    let mut untrusted = ContentManager::from_manifest(manifest.clone(), &content, &root.join("install2"));
    untrusted.add_source(Box::new(HttpSource::new(&url, build).expect("https source")));
    untrusted
        .request(&["arena"], RequestOptions::default())
        .expect("request");
    untrusted.advance(0, None);
    untrusted.wait_for_transfers();
    untrusted.advance(0, None);
    assert!(
        matches!(
            untrusted.state("arena"),
            Some(ChunkState::Failed(FailReason::SourceUnreachable(_)))
        ),
        "{:?}",
        untrusted.state("arena")
    );
    assert_eq!(
        fetch_manifest_with(&HttpClient::new(), &url, &manifest)
            .expect_err("untrusted certificate")
            .reason(),
        "source_unreachable"
    );

    // 4. A file the host does not have over TLS: source_unreachable, not a hang.
    let mut missing = ContentManager::from_manifest(manifest, &content, &root.join("install3"));
    missing.add_source(Box::new(
        HttpSource::with_client(&url, "other-build", client).expect("source"),
    ));
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
    let _ = std::fs::remove_dir_all(&root);
}
