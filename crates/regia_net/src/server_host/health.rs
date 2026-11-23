// SPDX-License-Identifier: MPL-2.0
//! The read-only HTTP health endpoint (spec 76 § 8), std networking only.
//!
//! One request per connection, a 1 KiB request limit and a 2 s read timeout (RFC 9112 message
//! syntax, minimal subset). `GET /health` answers `200` with the host's JSON while running and `503`
//! while draining; any other path is `404`, any other method `405`, an oversized request `431`.
//! No admin through HTTP.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Largest request accepted, bytes.
pub const MAX_REQUEST_BYTES: usize = 1024;
/// Read timeout per connection.
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Connections served at once, each on its own short-lived thread: one client that holds its
/// read window cannot delay the probes behind it (audit-v035-net L5). Past this many, a new
/// connection is closed unanswered until one finishes.
const MAX_CONCURRENT: usize = 8;

/// What the endpoint serves; the host replaces it every tick.
#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    /// The JSON body.
    pub json: String,
    /// True while draining (`503`).
    pub draining: bool,
}

/// A running endpoint (a background thread). Dropping it stops the thread.
#[derive(Debug)]
pub struct HealthServer {
    addr: SocketAddr,
    snapshot: Arc<Mutex<HealthSnapshot>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Answers one request text: `(status line, body)`.
pub fn respond(request: &[u8], snapshot: &HealthSnapshot) -> (&'static str, String) {
    if request.len() > MAX_REQUEST_BYTES {
        return (
            "431 Request Header Fields Too Large",
            "{\"error\":\"request too large\"}".into(),
        );
    }
    let text = String::from_utf8_lossy(request);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        return (
            "405 Method Not Allowed",
            "{\"error\":\"method not allowed\"}".into(),
        );
    }
    if path != "/health" {
        return ("404 Not Found", "{\"error\":\"not found\"}".into());
    }
    if snapshot.draining {
        ("503 Service Unavailable", snapshot.json.clone())
    } else {
        ("200 OK", snapshot.json.clone())
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    // One deadline for the whole request: a client trickling bytes cannot hold the endpoint.
    let deadline = std::time::Instant::now() + READ_TIMEOUT;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while std::time::Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > MAX_REQUEST_BYTES {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf
}

fn serve(mut stream: TcpStream, snapshot: &Mutex<HealthSnapshot>) {
    let request = read_request(&mut stream);
    let snap = snapshot.lock().map(|s| s.clone()).unwrap_or_default();
    let (status, body) = respond(&request, &snap);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Serves one connection on its own thread (bounded by the request deadline and the write
/// timeout), at most [`MAX_CONCURRENT`] at once; serves it inline when no thread can be started.
fn serve_detached(stream: TcpStream, snapshot: &Arc<Mutex<HealthSnapshot>>, busy: &Arc<AtomicUsize>) {
    if busy.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT {
        busy.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let (snap, count) = (snapshot.clone(), busy.clone());
    // The stream comes back when the spawn fails (the closure is dropped with it otherwise).
    let slot = Arc::new(Mutex::new(Some(stream)));
    let taken = slot.clone();
    let spawned = std::thread::Builder::new()
        .name("regia-health-conn".into())
        .spawn(move || {
            if let Some(stream) = taken.lock().ok().and_then(|mut s| s.take()) {
                serve(stream, &snap);
            }
            count.fetch_sub(1, Ordering::AcqRel);
        });
    if spawned.is_err() {
        if let Some(stream) = slot.lock().ok().and_then(|mut s| s.take()) {
            serve(stream, snapshot);
        }
        busy.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HealthServer {
    /// Binds and starts serving. Bind port 0 to let the OS choose (tests); read it with
    /// [`Self::addr`].
    pub fn start(bind: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let snapshot = Arc::new(Mutex::new(HealthSnapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (snap, stop_flag) = (snapshot.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("regia-health".into())
            .spawn(move || {
                let busy = Arc::new(AtomicUsize::new(0));
                while !stop_flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            serve_detached(stream, &snap, &busy);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            })?;
        Ok(Self {
            addr,
            snapshot,
            stop,
            thread: Some(thread),
        })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Replaces what the endpoint serves.
    pub fn publish(&self, snapshot: HealthSnapshot) {
        if let Ok(mut s) = self.snapshot.lock() {
            *s = snapshot;
        }
    }
}

impl Drop for HealthServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let snap = HealthSnapshot {
            json: "{\"ok\":true}".into(),
            draining: false,
        };
        assert_eq!(respond(b"GET /health HTTP/1.1\r\n\r\n", &snap).0, "200 OK");
        assert_eq!(respond(b"GET /other HTTP/1.1\r\n\r\n", &snap).0, "404 Not Found");
        assert_eq!(
            respond(b"POST /health HTTP/1.1\r\n\r\n", &snap).0,
            "405 Method Not Allowed"
        );
        let draining = HealthSnapshot {
            draining: true,
            ..snap.clone()
        };
        assert_eq!(
            respond(b"GET /health HTTP/1.1\r\n\r\n", &draining).0,
            "503 Service Unavailable"
        );
        let big = vec![b'a'; 2048];
        assert!(respond(&big, &snap).0.starts_with("431"));
    }

    /// audit-v035-net L5 / audit-v035-conc L11: the accept loop served one connection at a time,
    /// so a client that connected and sent nothing held every later probe for its whole read
    /// window. A probe is now answered while the silent connection is still open and unanswered.
    #[test]
    fn a_silent_client_does_not_hold_the_next_probe() {
        let server = HealthServer::start(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        server.publish(HealthSnapshot {
            json: "{\"ok\":true}".into(),
            draining: false,
        });
        let mut silent = TcpStream::connect(server.addr()).expect("connect the silent client");
        let mut probe = TcpStream::connect(server.addr()).expect("connect the probe");
        probe
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("send the probe");
        let mut answer = String::new();
        probe.read_to_string(&mut answer).expect("read the answer");
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        // The silent connection has had no answer yet: the probe did not wait for its window.
        silent.set_nonblocking(true).expect("nonblocking");
        let mut byte = [0u8; 1];
        match silent.read(&mut byte) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("the silent client was answered or closed before the probe: {other:?}"),
        }
    }

    /// Spec 76 § 8 (audit ADR-5650): the read window ends. A client that connects and sends
    /// nothing gets the empty-request answer and a closed connection once `READ_TIMEOUT` runs
    /// out; the client's own read timeout (far longer) never fires.
    #[test]
    fn a_silent_client_is_closed_when_its_read_window_ends() {
        let server = HealthServer::start(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        server.publish(HealthSnapshot {
            json: "{\"ok\":true}".into(),
            draining: false,
        });
        let mut silent = TcpStream::connect(server.addr()).expect("connect the silent client");
        silent
            .set_read_timeout(Some(READ_TIMEOUT * 15))
            .expect("client timeout");
        let mut answer = String::new();
        silent
            .read_to_string(&mut answer)
            .expect("the server closes the connection before the client's own timeout");
        assert!(answer.starts_with("HTTP/1.1 405"), "{answer}");
        assert!(answer.contains("Connection: close"), "{answer}");
    }
}
