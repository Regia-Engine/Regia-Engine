// SPDX-License-Identifier: MPL-2.0
//! The `https://` shared cook cache backend (ADR-2710): a cache served over rustls on loopback with
//! a self-signed certificate that only the test's ureq agent trusts. The backend stores, finds,
//! reads and removes entries through [`CookCacheBackend`], sends the `Authorization` value on every
//! request, never shows it, and fails (never hangs, never leaks) on a wrong token or an untrusted
//! certificate. A full cook through the backend is the `http://` test in `cook_shared_cache.rs`:
//! both schemes share every byte of the cook side; only the transport differs.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use regia_asset_pipeline::cook_cache::{CookCacheBackend, HttpBackend, sample_entry, verify_entry};

const TOKEN: &str = "Bearer test-only-token-6f1d";

/// What the server saw and holds.
#[derive(Default)]
struct Server {
    entries: BTreeMap<String, Vec<u8>>,
    /// The `Authorization` value of every request, in arrival order (`None`: no header).
    auth_seen: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Server>>;

/// Answers one request on a TLS stream: `401` without the right token, else the in-memory store.
fn answer<S: Read + Write>(stream: S, server: &Shared) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (
        parts.next().unwrap_or("").to_owned(),
        parts.next().unwrap_or("").to_owned(),
    );
    let (mut length, mut auth) = (0usize, None);
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                auth = Some(value.trim().to_owned());
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let (status, reply) = {
        let mut s = server.lock().expect("server");
        s.auth_seen.push(auth.clone());
        if auth.as_deref() != Some(TOKEN) {
            ("401 Unauthorized", Vec::new())
        } else {
            match method.as_str() {
                "GET" => match s.entries.get(&target) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", Vec::new()),
                },
                "HEAD" if s.entries.contains_key(&target) => ("200 OK", Vec::new()),
                "HEAD" => ("404 Not Found", Vec::new()),
                "PUT" => {
                    // Stored whole, after the whole body arrived: the atomic-put contract.
                    s.entries.insert(target, body);
                    ("201 Created", Vec::new())
                }
                "DELETE" => {
                    s.entries.remove(&target);
                    ("204 No Content", Vec::new())
                }
                _ => ("405 Method Not Allowed", Vec::new()),
            }
        }
    };
    let mut stream = reader.into_inner();
    let length = if method == "HEAD" { 0 } else { reply.len() };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    )?;
    if method != "HEAD" {
        stream.write_all(&reply)?;
    }
    stream.flush()
}

/// A TLS cache server on `127.0.0.1:<port>` for `localhost`, and an agent that trusts only its
/// certificate. Returns (`https://localhost:<port>/cache`, server state, trusting agent).
fn tls_cache() -> (String, Shared, ureq::Agent) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("self-signed cert");
    let cert_der = certified.cert.der().clone();
    let key_der: rustls::pki_types::PrivateKeyDer<'static> =
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("protocols")
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server config"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server: Shared = Arc::default();
    let shared = Arc::clone(&server);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let config = Arc::clone(&config);
            let server = Arc::clone(&shared);
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(config) else {
                    return;
                };
                let mut tls = rustls::StreamOwned::new(conn, stream);
                // A client that rejects the certificate closes mid-handshake: not an error here.
                if answer(&mut tls, &server).is_ok() {
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                }
            });
        }
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("root store");
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let agent = ureq::AgentBuilder::new()
        .tls_config(Arc::new(client))
        .redirects(0)
        .build();
    (format!("https://localhost:{port}/cache"), server, agent)
}

#[test]
fn an_https_cache_stores_reads_and_removes_entries_with_the_token() {
    let (url, server, agent) = tls_cache();
    let backend = HttpBackend::with_options(&url, Some(agent), Some(TOKEN.into())).expect("backend");
    assert_eq!(backend.location(), url);
    assert!(
        !format!("{backend:?}").contains(TOKEN),
        "Debug never shows the token"
    );
    let key = [0x5a; 32];
    let entry = sample_entry();
    assert!(!backend.contains(&key).expect("head"));
    assert_eq!(backend.get(&key).expect("get"), None, "a miss is not an error");
    backend.put(&key, &entry).expect("put");
    assert!(backend.contains(&key).expect("head"));
    let got = backend.get(&key).expect("get").expect("hit");
    assert_eq!(got, entry);
    assert!(verify_entry(&got).is_ok());
    backend.touch(&key).expect("touch");
    // The layout is the directory backend's, under the URL's path.
    {
        let s = server.lock().expect("server");
        let target = s.entries.keys().next().expect("stored");
        assert!(
            target.starts_with("/cache/v1/5a/") && target.ends_with(".rcc"),
            "{target}"
        );
        assert!(
            s.auth_seen.iter().all(|a| a.as_deref() == Some(TOKEN)),
            "every request carries the token"
        );
    }
    // A stored entry damaged on the server reads back and fails verification (a miss to the cook).
    {
        let mut s = server.lock().expect("server");
        let body = s.entries.values_mut().next().expect("stored");
        body.truncate(body.len() / 2);
    }
    assert!(verify_entry(&backend.get(&key).expect("get").expect("bytes")).is_err());
    backend.remove(&key).expect("delete");
    assert!(!backend.contains(&key).expect("head"));
    // Listing is the server's job, as for http://.
    assert_eq!(
        backend.list().expect_err("unsupported").kind(),
        std::io::ErrorKind::Unsupported
    );
}

#[test]
fn a_wrong_token_or_an_untrusted_certificate_fails_without_leaking_the_token() {
    let (url, server, agent) = tls_cache();
    let key = [0x11; 32];
    // No token, then a wrong one: 401 is an error (never a silent miss that would re-cook forever
    // while the cook believes the cache is empty).
    for auth in [None, Some("Bearer wrong".to_owned())] {
        let backend = HttpBackend::with_options(&url, Some(agent.clone()), auth).expect("backend");
        let err = backend.get(&key).expect_err("401");
        assert!(err.to_string().contains("401"), "{err}");
        assert!(backend.put(&key, b"x").is_err());
        assert!(backend.contains(&key).is_err());
    }
    assert!(server.lock().expect("server").entries.is_empty());
    // The default agent trusts the web PKI roots only: the self-signed host is refused.
    let untrusted = HttpBackend::with_options(&url, None, Some(TOKEN.into())).expect("backend");
    let err = untrusted.get(&key).expect_err("untrusted certificate");
    assert!(!err.to_string().contains("test-only-token"), "{err}");
    assert!(
        server
            .lock()
            .expect("server")
            .auth_seen
            .iter()
            .all(|a| a.as_deref() != Some(TOKEN)),
        "the token never reached a host the client does not trust"
    );
}
