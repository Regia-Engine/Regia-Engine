// SPDX-License-Identifier: MPL-2.0
//! End-to-end `https://` upload through the `tls` feature against a local TLS server with a
//! self-signed certificate (trusted only in this test). Builds only with the `tls` feature.

#![cfg(feature = "tls")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use regia_crash::transport::{TransportTimeouts, build_agent, post_with_agent};

#[test]
fn https_upload_against_a_local_tls_server() {
    // A self-signed certificate for `localhost`, valid only for this test.
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("self-signed cert");
    let cert_der = certified.cert.der().clone();
    let key_der: rustls::pki_types::PrivateKeyDer<'static> =
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into();

    let provider = Arc::new(rustls::crypto::ring::default_provider());

    // Server: rustls over a plain TCP stream.
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .expect("server config");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    type Received = Arc<Mutex<Option<(String, String, Vec<u8>)>>>;
    let received: Received = Arc::new(Mutex::new(None));
    let recv = received.clone();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let conn = rustls::server::ServerConnection::new(Arc::new(server_config)).expect("server conn");
        let mut tls = rustls::StreamOwned::new(conn, stream);
        let mut reader = BufReader::new(&mut tls);
        let mut content_length = 0usize;
        let mut name = String::new();
        let mut legacy_name = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if let Some(v) = trimmed.strip_prefix("X-Regia-Report-Name:") {
                name = v.trim().to_owned();
            }
            let legacy = format!("{}:", regia_crash::transport::LEGACY_REPORT_NAME_HEADER);
            if let Some(v) = trimmed.strip_prefix(legacy.as_str()) {
                legacy_name = v.trim().to_owned();
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).expect("body");
        drop(reader);
        *recv.lock().expect("lock") = Some((name, legacy_name, body));
        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("reply");
        tls.flush().ok();
    });

    // Client trusts only the self-signed certificate (valid for this test only).
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

    let status = post_with_agent(
        &agent,
        &format!("https://localhost:{}/upload", addr.port()),
        b"tls crash body",
        "text/plain",
        Some("m.log"),
        TransportTimeouts::default(),
    )
    .expect("https post");
    assert_eq!(status, 200);

    // The server saw the report name and body over TLS.
    let got = received.lock().expect("lock").clone();
    assert_eq!(
        got,
        Some((
            "m.log".to_string(),
            "m.log".to_string(),
            b"tls crash body".to_vec()
        )),
        "the report name (both headers, ADR-8670) and body arrived over TLS"
    );
}

#[test]
fn a_redirect_is_returned_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(Mutex::new(0u32));
    let counter = hits.clone();
    std::thread::spawn(move || {
        // Two accepts: a redirect-following client would reconnect for the GET.
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept() else { break };
            *counter.lock().expect("lock") += 1;
            let mut reader = BufReader::new(stream);
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.trim_end().strip_prefix("Content-Length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let mut stream = reader.into_inner();
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{addr}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    // Plain-http through the ureq path with the crate's agent: ureq's default of 5 redirects must
    // not apply (ureq 2 has no per-request override, so the policy lives on the agent).
    let agent = build_agent(TransportTimeouts::default());
    let status = post_with_agent(
        &agent,
        &format!("http://{addr}/upload"),
        b"body",
        "text/plain",
        None,
        TransportTimeouts::default(),
    )
    .expect("post");
    assert_eq!(status, 302);
    assert_eq!(*hits.lock().expect("lock"), 1, "the redirect was not followed");
}

/// Runs a one-shot TLS server that answers any request with `reply`, and returns a guarded agent
/// that trusts only its certificate plus the server's port.
fn serve_raw_reply(reply: &'static [u8]) -> (ureq::Agent, u16) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("self-signed cert");
    let cert_der = certified.cert.der().clone();
    let key_der: rustls::pki_types::PrivateKeyDer<'static> =
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .expect("server config");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let conn = rustls::server::ServerConnection::new(Arc::new(server_config)).expect("server conn");
        let mut tls = rustls::StreamOwned::new(conn, stream);
        let mut reader = BufReader::new(&mut tls);
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim_end().is_empty() {
                break;
            }
            if let Some(v) = line.trim_end().strip_prefix("Content-Length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
        drop(reader);
        let _ = tls.write_all(reply);
        let _ = tls.flush();
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("root store");
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let agent = ureq::AgentBuilder::new()
        .tls_connector(regia_crash::tls_guard::guarded_tls_connector(Arc::new(
            client_config,
        )))
        .max_idle_connections(0)
        .redirects(0)
        .build();
    (agent, port)
}

fn post_to(agent: &ureq::Agent, port: u16) -> Result<u16, regia_crash::transport::TransportError> {
    post_with_agent(
        agent,
        &format!("https://localhost:{port}/upload"),
        b"body",
        "text/plain",
        None,
        TransportTimeouts::default(),
    )
}

/// ureq 2.12.1 panics (an abort under the shipping `panic = "abort"`) on a response header line
/// that is exactly a header name with no colon; the guarded connector repairs it first.
#[test]
fn a_colonless_content_length_header_does_not_panic_ureq() {
    for reply in [
        &b"HTTP/1.1 200 OK\r\nContent-Length\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\nConnection\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Encoding\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 204 No Content\r\nConnection\r\n\r\n",
    ] {
        let reply: &'static [u8] = Box::leak(reply.to_vec().into_boxed_slice());
        let (agent, port) = serve_raw_reply(reply);
        let status = post_to(&agent, port);
        assert!(
            matches!(status, Ok(200) | Ok(204)),
            "{:?} -> {status:?}",
            String::from_utf8_lossy(reply)
        );
    }
}

/// A TLS server that answers each request in turn with the next of `replies`, keeping one
/// connection alive across requests; returns a guarded agent that pools connections (ureq's
/// default) and trusts only the server's certificate, the port, and the count of accepted
/// connections.
fn serve_keep_alive(replies: Vec<&'static [u8]>) -> (ureq::Agent, u16, Arc<Mutex<usize>>) {
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
    let connections = Arc::new(Mutex::new(0usize));
    let counted = connections.clone();
    std::thread::spawn(move || {
        let mut next = 0usize;
        while next < replies.len() {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            *counted.lock().expect("lock") += 1;
            let conn = rustls::server::ServerConnection::new(server_config.clone()).expect("server conn");
            let mut tls = rustls::StreamOwned::new(conn, stream);
            while next < replies.len() {
                let mut reader = BufReader::new(&mut tls);
                let mut content_length = 0usize;
                let mut saw_request = false;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if line.trim_end().is_empty() {
                        break;
                    }
                    saw_request = true;
                    if let Some(v) = line.trim_end().strip_prefix("Content-Length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if !saw_request {
                    break; // the client closed this connection
                }
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
                drop(reader);
                let _ = tls.write_all(replies[next]);
                let _ = tls.flush();
                next += 1;
            }
        }
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("root store");
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocols")
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Built as `guarded_tls_connector`'s docs describe, with ureq's default connection pool.
    let agent = ureq::AgentBuilder::new()
        .tls_connector(regia_crash::tls_guard::guarded_tls_connector(Arc::new(
            client_config,
        )))
        .redirects(0)
        .build();
    (agent, port, connections)
}

/// The head guard used to be one-shot: on a pooled connection the second response's head reached
/// ureq unfiltered, and a colon-less header line there panicked it (an abort under the shipping
/// profile; audit-concurrency-1103 crash M3). Each new request on the stream re-arms the guard.
#[test]
fn the_head_guard_filters_every_response_on_a_pooled_connection() {
    let (agent, port, connections) = serve_keep_alive(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length\r\n\r\n",
    ]);
    assert!(matches!(post_to(&agent, port), Ok(200)), "first upload");
    let second = post_to(&agent, port);
    assert!(matches!(second, Ok(200)), "second upload: {second:?}");
    assert_eq!(
        *connections.lock().expect("lock"),
        1,
        "both requests went over one pooled connection"
    );
}
