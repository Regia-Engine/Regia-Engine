// SPDX-License-Identifier: MIT-0
//! A real TLS server on localhost with a self-signed certificate, serving a real cooked package: a pinned
//! download installs, a wrong pin and the public roots do not, and a server that lies about a file is refused.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

use product_viewer::fetch::{HttpsSource, PackageUrl};
use product_viewer::store::{PackageSource, PackageStore};
use regia_test_utils::unique_temp_dir;
use regia_viewer_pack::standin;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Serves `files` (path → bytes) on a new port until the process ends; returns the port and the
/// certificate's DER.
fn serve(files: HashMap<String, Vec<u8>>) -> (u16, Vec<u8>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("test certificate");
    let der = cert.der().to_vec();
    let key = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).expect("test key");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_no_client_auth()
        .with_single_cert(vec![rustls::pki_types::CertificateDer::from(der.clone())], key)
        .expect("server config");
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let Ok(connection) = rustls::ServerConnection::new(config.clone()) else {
                continue;
            };
            let mut tls = rustls::StreamOwned::new(connection, stream);
            let mut request = String::new();
            let mut reader = BufReader::new(&mut tls);
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {}
                }
            }
            let path = request.split(' ').nth(1).unwrap_or("").to_owned();
            let head = match files.get(&path) {
                Some(body) => {
                    let mut out = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    out.extend_from_slice(body);
                    out
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            };
            let _ = tls.write_all(&head);
            let _ = tls.flush();
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    (port, der)
}

fn sha256_hex(der: &[u8]) -> String {
    let suite = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256;
    let digest = suite.tls13().expect("tls13").common.hash_provider.hash(der);
    hex(digest.as_ref())
}

/// The first stand-in package, cooked for real, as a map of URL path → bytes.
fn package(work: &Path) -> HashMap<String, Vec<u8>> {
    let set = standin::write(work).expect("stand-ins");
    let out = work.join("cooked");
    let options = regia_viewer_pack::cook::CookOptions {
        materials: Some(set.library.clone()),
        option_graph: set.option_graphs.first().cloned(),
        ..Default::default()
    };
    regia_viewer_pack::cook::pack(&set.kit_dir, &set.recipes[0], &out, &options).expect("cook");
    let mut files = HashMap::new();
    for entry in std::fs::read_dir(&out).expect("read cooked").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        files.insert(
            format!("/p/piece/{name}"),
            std::fs::read(entry.path()).expect("read file"),
        );
    }
    files
}

#[test]
fn a_pinned_download_installs_the_package_and_it_opens_offline() {
    let dir = unique_temp_dir("product-viewer-https");
    let (port, der) = serve(package(&dir.join("work")));
    let url = PackageUrl::parse(&format!(
        "https://localhost:{port}/p/piece#sha256={}",
        sha256_hex(&der)
    ))
    .expect("url");
    let store = PackageStore::new(vec![dir.join("store")]);
    let mut source = HttpsSource::new(url).expect("source");
    let mut last = (0, 1);
    let name = store
        .install(&mut source, &mut |done, total| last = (done, total))
        .expect("install");
    assert_eq!(last.0, last.1, "progress reached the total");
    assert!(last.1 > 0);
    assert_eq!(store.list().len(), 1);
    // No network from here on: the store reads the folder.
    let package = store.open(&name).expect("open");
    assert!(!package.meshes.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_wrong_pin_downloads_nothing() {
    let dir = unique_temp_dir("product-viewer-https-pin");
    let (port, _) = serve(package(&dir));
    let wrong = "ab".repeat(32);
    let url = PackageUrl::parse(&format!("https://localhost:{port}/p/piece#sha256={wrong}")).expect("url");
    let mut source = HttpsSource::new(url).expect("source");
    assert_eq!(
        source.get("manifest.json", 1024 * 1024).err().map(|e| e.code),
        Some("package_fetch_failed")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_self_signed_server_without_a_pin_is_refused() {
    let dir = unique_temp_dir("product-viewer-https-roots");
    let (port, _) = serve(package(&dir));
    let url = PackageUrl::parse(&format!("https://localhost:{port}/p/piece")).expect("url");
    let mut source = HttpsSource::new(url).expect("source");
    assert_eq!(
        source.get("manifest.json", 1024 * 1024).err().map(|e| e.code),
        Some("package_fetch_failed")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_server_that_lies_about_a_file_installs_nothing_and_keeps_the_old_package() {
    let dir = unique_temp_dir("product-viewer-https-lie");
    let good = package(&dir.join("work"));
    let (good_port, der) = serve(good.clone());
    let store = PackageStore::new(vec![dir.join("store")]);
    let url = PackageUrl::parse(&format!(
        "https://localhost:{good_port}/p/piece#sha256={}",
        sha256_hex(&der)
    ))
    .expect("url");
    let name = store
        .install(&mut HttpsSource::new(url).expect("source"), &mut |_, _| {})
        .expect("install");

    let mut lying = good;
    let mesh = lying.get_mut("/p/piece/mesh.rvm").expect("mesh");
    mesh[0] ^= 0xff;
    let (port, der) = serve(lying);
    let url = PackageUrl::parse(&format!(
        "https://localhost:{port}/p/piece#sha256={}",
        sha256_hex(&der)
    ))
    .expect("url");
    let err = store
        .install(&mut HttpsSource::new(url).expect("source"), &mut |_, _| {})
        .err();
    assert_eq!(err.map(|e| e.code), Some("pack_hash_mismatch"));
    assert!(store.open(&name).is_ok(), "the old package is untouched");
    let leftovers: Vec<_> = std::fs::read_dir(&store.roots()[0])
        .expect("read store")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".install-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_missing_file_is_a_fetch_failure() {
    let (port, der) = serve(HashMap::new());
    let url = PackageUrl::parse(&format!(
        "https://localhost:{port}/nothing/#sha256={}",
        sha256_hex(&der)
    ))
    .expect("url");
    let mut source = HttpsSource::new(url).expect("source");
    assert_eq!(
        source.get("manifest.json", 1024).err().map(|e| e.code),
        Some("package_fetch_failed")
    );
}
