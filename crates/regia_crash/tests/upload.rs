// SPDX-License-Identifier: MPL-2.0
//! End-to-end uploader tests against a local HTTP server (no internet, no TLS).
//!
//! The server listens on an ephemeral loopback port and records the request path, headers and body
//! it received, then answers with a configurable status code.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_crash::transport::TransportError;
use regia_crash::{CrashReporter, UploadSummary};

/// The tests share `regia_platform::crash`'s process-global crash directory, so they are
/// serialised rather than run in parallel.
static SERIAL: Mutex<()> = Mutex::new(());

/// What one accepted request looked like to the fake server.
#[derive(Debug, Clone)]
struct Request {
    path: String,
    name: Option<String>,
    body: Vec<u8>,
}

/// Spawns a server that accepts up to `accept` connections, records them, and answers `status`.
fn spawn_server(status: u16, accept: usize) -> (String, Arc<Mutex<Vec<Request>>>) {
    spawn_server_with(status, accept, None)
}

/// A callback the fake server runs after reading a request and before it answers.
type BeforeReply = Box<dyn Fn() + Send>;

/// Like [`spawn_server`], running `before_reply` (if any) after the request is read.
fn spawn_server_with(
    status: u16,
    accept: usize,
    before_reply: Option<BeforeReply>,
) -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let received: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
    let recv = received.clone();
    std::thread::spawn(move || {
        for _ in 0..accept {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let mut content_length = 0usize;
            let mut name = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                    break;
                }
                let trimmed = line.trim_end();
                if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if let Some(v) = trimmed.strip_prefix("X-Regia-Report-Name:") {
                    name = Some(v.trim().to_owned());
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let mut stream = reader.into_inner();
            if let Some(hook) = &before_reply {
                hook();
            }
            // Recorded before the reply: the client returns once it reads the reply, and the test
            // reads `received` at once, so a push after the write raced it on a loaded host.
            recv.lock().expect("lock").push(Request { path, name, body });
            let reason = match status {
                200 => "OK",
                _ => "Server Error",
            };
            let response =
                format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/upload"), received)
}

/// Points the global crash directory at a fresh temp dir and drops a crash report into it.
fn seed_crash_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-crash-upload-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    regia_platform::crash::configure(
        Some(dir.clone()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    std::fs::write(dir.join("crash-2026-10-06_00-00-01.txt"), "fake crash body").expect("seed");
    dir
}

fn reporter(endpoint: &str) -> CrashReporter {
    CrashReporter {
        endpoint: endpoint.to_owned(),
        consent: true,
        max_retries: 0,
        backoff: Duration::from_millis(1),
        ..CrashReporter::default()
    }
}

#[test]
fn upload_pending_posts_and_removes_on_success() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("ok");
    let (url, received) = spawn_server(200, 1);

    let summary = reporter(&url).upload_pending();

    assert!(!summary.disabled);
    assert_eq!(summary.attempted, 1);
    assert_eq!(summary.uploaded, 1);
    assert_eq!(summary.failed, 0, "{summary:?}");
    assert_eq!(summary.errors, Vec::<String>::new());

    let got = received.lock().expect("lock");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path, "/upload");
    assert_eq!(got[0].name.as_deref(), Some("crash-2026-10-06_00-00-01.txt"));
    assert_eq!(got[0].body, b"fake crash body");

    // Accepted reports are removed from disk.
    assert!(regia_platform::crash::pending().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejected_upload_keeps_the_report() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("reject");
    let (url, _received) = spawn_server(500, 1);

    let summary = reporter(&url).upload_pending();

    assert_eq!(summary.attempted, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.uploaded, 0);
    assert!(
        summary.errors[0].contains("500"),
        "error mentions the status: {summary:?}"
    );
    // The report stays on disk for the next start.
    assert_eq!(regia_platform::crash::pending().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn final_4xx_quarantines_the_report_so_later_starts_skip_it() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("quarantine");
    let (url, received) = spawn_server(404, 1);

    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.failed, 1);
    assert_eq!(received.lock().expect("lock").len(), 1);
    // Not pending any more, but the bytes are kept.
    assert!(regia_platform::crash::pending().is_empty());
    assert!(dir.join("crash-2026-10-06_00-00-01.txt.rejected").is_file());

    // A second start dials nothing.
    let again = reporter(&url).upload_pending();
    assert_eq!(again.attempted, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_301_quarantines_like_a_404() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("redirect");
    let (url, received) = spawn_server(301, 2);

    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.uploaded, 0);
    assert_eq!(received.lock().expect("lock").len(), 1);
    assert!(regia_platform::crash::pending().is_empty());
    assert!(dir.join("crash-2026-10-06_00-00-01.txt.rejected").is_file());

    let again = reporter(&url).upload_pending();
    assert_eq!(again.attempted, 0);
    assert!(dir.join("crash-2026-10-06_00-00-01.txt.rejected").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transient_4xx_keeps_the_report_pending() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("transient4xx");
    let (url, _received) = spawn_server(429, 1);
    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.failed, 1);
    assert_eq!(
        regia_platform::crash::pending().len(),
        1,
        "429 is retried next start"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn upload_error_list_is_capped() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("errcap");
    for i in 2..=30 {
        std::fs::write(dir.join(format!("crash-2026-10-06_00-00-{i:02}.txt")), "x").expect("seed");
    }
    let (url, _received) = spawn_server(500, 40);
    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.failed, 30);
    assert_eq!(summary.errors.len(), 16, "errors are capped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn accepted_report_deletes_the_file_that_was_sent_even_if_the_crash_dir_moves() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("moved");
    let other = std::env::temp_dir().join(format!("regia-crash-upload-moved-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&other);
    std::fs::create_dir_all(&other).expect("other dir");
    std::fs::write(other.join("crash-2026-10-06_00-00-01.txt"), "unsent").expect("seed other");

    // While the request is in flight, point the global crash directory somewhere else.
    let moved = other.clone();
    let hook: BeforeReply = Box::new(move || {
        regia_platform::crash::configure(
            Some(moved.clone()),
            regia_platform::crash::BuildInfo::detect(),
            regia_platform::OsInfo::detect(),
        );
    });
    let (url, _received) = spawn_server_with(200, 1, Some(hook));

    // `reporter.upload_pending` lists `dir` first, then the 2xx arrives after the move.
    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.uploaded, 1, "{summary:?}");
    assert!(
        !dir.join("crash-2026-10-06_00-00-01.txt").exists(),
        "the sent file is removed"
    );
    assert!(
        other.join("crash-2026-10-06_00-00-01.txt").exists(),
        "the same-named unsent report in the new directory survives"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&other);
}

#[test]
fn disabled_without_endpoint_or_consent() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    seed_crash_dir("disabled");
    let no_endpoint = CrashReporter {
        consent: true,
        ..CrashReporter::default()
    };
    assert!(no_endpoint.upload_pending().disabled);

    let no_consent = CrashReporter {
        endpoint: "http://127.0.0.1:1/upload".into(),
        consent: false,
        ..CrashReporter::default()
    };
    assert!(no_consent.upload_pending().disabled);

    // Nothing was sent; the report is untouched.
    assert_eq!(regia_platform::crash::pending().len(), 1);

    // Consent and an endpoint, but remote cleartext is its own refusal.
    let cleartext = CrashReporter {
        endpoint: "http://collector.example.com/r".into(),
        consent: true,
        allow_cleartext: false,
        ..CrashReporter::default()
    };
    let summary = cleartext.upload_pending();
    assert!(summary.disabled);
    assert_eq!(summary.attempted, 0);
    let error = cleartext.upload_match_log("m.log", b"x").expect_err("cleartext");
    assert!(matches!(error, TransportError::RemoteCleartext));
    let text = error.to_string();
    assert!(text.contains("http") || text.contains("cleartext"), "{text}");
    assert!(!text.contains("no consent"), "{text}");
    assert_eq!(regia_platform::crash::pending().len(), 1);
}

#[test]
fn oversized_reports_are_skipped() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("size");
    let (url, received) = spawn_server(200, 1);
    let reporter = CrashReporter {
        max_bytes: 4, // smaller than "fake crash body"
        ..reporter(&url)
    };
    let summary = reporter.upload_pending();
    assert_eq!(summary.attempted, 0);
    assert_eq!(summary.skipped_too_large, 1);
    assert_eq!(received.lock().expect("lock").len(), 0, "nothing was sent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn match_log_uploads_with_its_name() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    seed_crash_dir("matchlog");
    let (url, received) = spawn_server(200, 1);
    let status = reporter(&url)
        .upload_match_log("match_2026-10-06.log", b"match log body")
        .expect("upload");
    assert_eq!(status, 200);
    let got = received.lock().expect("lock");
    assert_eq!(got[0].name.as_deref(), Some("match_2026-10-06.log"));
    assert_eq!(got[0].body, b"match log body");
}

#[cfg(not(feature = "tls"))]
#[test]
fn https_endpoints_are_refused_without_a_tls_backend() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    seed_crash_dir("tls");
    let summary = reporter("https://crash.example.com/regia").upload_pending();
    assert_eq!(summary.attempted, 1);
    assert_eq!(summary.failed, 1);
    assert!(
        summary.errors[0].contains("tls"),
        "the error explains the missing TLS backend: {summary:?}"
    );
    // Offline-safe: the report is retained.
    assert_eq!(regia_platform::crash::pending().len(), 1);
}

/// The summary is usable in a report (Debug is stable and non-empty).
#[test]
fn upload_summary_is_reportable() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let summary = UploadSummary {
        attempted: 2,
        uploaded: 1,
        failed: 1,
        ..UploadSummary::default()
    };
    assert!(format!("{summary:?}").contains("attempted"));
}

// ---------------------------------------------------------------------------
// Match-log gating (round 2): consent, endpoint and max_bytes apply to the match log too.
// ---------------------------------------------------------------------------

/// Spawns a server that accepts `accept` connections and immediately drops each without replying,
/// so the client sees a transport error (no status line) on every attempt.
fn spawn_dropping_server(accept: usize) -> (String, Arc<Mutex<Vec<()>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let attempts: Arc<Mutex<Vec<()>>> = Arc::new(Mutex::new(Vec::new()));
    let recv = attempts.clone();
    std::thread::spawn(move || {
        for _ in 0..accept {
            let Ok((_stream, _)) = listener.accept() else {
                break;
            };
            recv.lock().expect("lock").push(());
            // Drop the stream without reading or replying: the client's read of the status line
            // fails, so the attempt is a transport error.
        }
    });
    (format!("http://{addr}/upload"), attempts)
}

#[test]
fn match_log_respects_consent_and_endpoint() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    seed_crash_dir("matchlog_gate");

    // No endpoint -> disabled.
    let no_endpoint = CrashReporter {
        consent: true,
        ..CrashReporter::default()
    };
    assert!(matches!(
        no_endpoint.upload_match_log("m.log", b"x"),
        Err(TransportError::Disabled)
    ));

    // No consent -> disabled.
    let no_consent = CrashReporter {
        endpoint: "http://127.0.0.1:1/upload".into(),
        consent: false,
        ..CrashReporter::default()
    };
    assert!(matches!(
        no_consent.upload_match_log("m.log", b"x"),
        Err(TransportError::Disabled)
    ));

    // Nothing was sent to the crash dir either.
    assert_eq!(regia_platform::crash::pending().len(), 1);
}

#[test]
fn match_log_respects_max_bytes() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    seed_crash_dir("matchlog_size");
    let (url, received) = spawn_server(200, 1);
    let reporter = CrashReporter {
        max_bytes: 4,
        ..reporter(&url)
    };
    assert!(matches!(
        reporter.upload_match_log("m.log", b"too large"),
        Err(TransportError::TooLarge { .. })
    ));
    assert_eq!(received.lock().expect("lock").len(), 0, "nothing was sent");
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("regia-crash-upload-matchlog_size-{}", std::process::id())),
    );
}

// ---------------------------------------------------------------------------
// Retry behaviour (round 2): transport errors and HTTP 5xx are retried max_retries+1 times.
// ---------------------------------------------------------------------------

/// A reporter whose endpoint is `url`, with `max_retries` retries and a 1 ms backoff (fast tests).
fn retrying_reporter(url: &str, max_retries: u32) -> CrashReporter {
    CrashReporter {
        endpoint: url.to_owned(),
        consent: true,
        max_retries,
        backoff: Duration::from_millis(1),
        ..CrashReporter::default()
    }
}

#[test]
fn transport_errors_are_retried_max_retries_plus_one_times() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let max_retries = 2u32;
    let (url, attempts) = spawn_dropping_server((max_retries + 1) as usize);
    let reporter = retrying_reporter(&url, max_retries);

    let result = reporter.upload_match_log("m.log", b"body");
    // The final attempt still fails at the transport layer.
    assert!(matches!(result, Err(TransportError::Io { .. })), "{result:?}");
    // Each retry re-dialled: max_retries + 1 total attempts.
    assert_eq!(
        attempts.lock().expect("lock").len(),
        (max_retries + 1) as usize,
        "a refused connection is attempted max_retries + 1 times"
    );
}

#[test]
fn http_5xx_is_retried_max_retries_plus_one_times() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let max_retries = 3u32;
    let (url, received) = spawn_server(500, (max_retries + 1) as usize);
    let reporter = retrying_reporter(&url, max_retries);

    let status = reporter.upload_match_log("m.log", b"body").expect("status");
    assert_eq!(
        status, 500,
        "the last 5xx is returned after retries are exhausted"
    );
    assert_eq!(
        received.lock().expect("lock").len(),
        (max_retries + 1) as usize,
        "an HTTP 5xx is retried max_retries + 1 times"
    );
}

#[test]
fn http_4xx_is_not_retried() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let (url, received) = spawn_server(404, 1);
    let reporter = retrying_reporter(&url, 3);

    let status = reporter.upload_match_log("m.log", b"body").expect("status");
    assert_eq!(status, 404);
    assert_eq!(
        received.lock().expect("lock").len(),
        1,
        "a client error is not retried"
    );
}

#[test]
fn huge_max_retries_is_capped() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    // The cap is 8 retries, so 9 attempts, regardless of the configured value.
    let (url, attempts) = spawn_dropping_server(9);
    let reporter = retrying_reporter(&url, u32::MAX);

    let result = reporter.upload_match_log("m.log", b"body");
    assert!(matches!(result, Err(TransportError::Io { .. })), "{result:?}");
    assert_eq!(attempts.lock().expect("lock").len(), 9);
}

// The uploader deletes the exact file it sent (by path), and the listing hides names
// `crash::remove` refuses, so the only way to make the delete fail is a read-only directory.
#[cfg(unix)]
#[test]
fn accepted_report_that_cannot_be_removed_counts_as_failed() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("unremovable");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("read-only dir");
    // Root ignores directory permissions: then the delete cannot be made to fail here.
    if std::fs::write(dir.join("probe"), "x").is_ok() {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (url, _received) = spawn_server(200, 1);

    let summary = reporter(&url).upload_pending();
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));

    assert_eq!(summary.attempted, 1, "{summary:?}");
    assert_eq!(summary.uploaded, 0, "{summary:?}");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert!(summary.errors[0].contains("not removed"), "{summary:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_symlinked_report_is_never_read_or_sent() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("symlink");
    std::fs::remove_file(dir.join("crash-2026-10-06_00-00-01.txt")).expect("unseed");
    let secret = dir.join("secret.dat");
    std::fs::write(&secret, "TOP SECRET").expect("secret");
    std::os::unix::fs::symlink(&secret, dir.join("crash-2026-10-06_00-00-02.txt")).expect("symlink");
    let (url, received) = spawn_server(200, 1);

    let summary = reporter(&url).upload_pending();

    assert_eq!(summary.attempted, 0, "{summary:?}");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert!(
        received.lock().expect("lock").is_empty(),
        "nothing reached the server"
    );
    assert!(secret.exists(), "the link target is untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

fn breadcrumb_trail() -> String {
    "gpu breadcrumbs: level 2 (fenced), submitted 7, completed 5, acknowledged frame 3\n  \
     suspects (2): clouds, fog\n"
        .to_owned()
}

/// ADR-2307 (G15): a device-lost crash report (`regia_platform::crash::write_device_lost_report`,
/// written by the renderer's loss path) is a pending report like a panic's, and its upload body
/// carries the registered `gpu breadcrumbs:` block that names the suspect passes.
#[test]
fn a_device_lost_report_uploads_with_its_breadcrumb_trail() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!("regia-crash-upload-lost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    regia_platform::crash::configure(
        Some(dir.clone()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    regia_platform::crash::add_report_section("gpu_breadcrumbs", breadcrumb_trail);
    let path =
        regia_platform::crash::write_device_lost_report("device_lost", "unknown", "VK_ERROR_DEVICE_LOST", 3)
            .expect("written");
    let (url, received) = spawn_server(200, 1);
    let summary = reporter(&url).upload_pending();
    assert_eq!((summary.attempted, summary.uploaded), (1, 1), "{summary:?}");
    let got = received.lock().expect("lock");
    let body = String::from_utf8_lossy(&got[0].body).into_owned();
    assert!(
        body.contains("device_lost: unknown: VK_ERROR_DEVICE_LOST"),
        "{body}"
    );
    assert!(body.contains("suspects (2): clouds, fog"), "{body}");
    assert_eq!(
        got[0].name.as_deref(),
        path.file_name().and_then(|n| n.to_str()),
        "the report's own name"
    );
    assert!(!path.exists(), "an accepted report is removed");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An upload can take minutes; a new report written under the same name meanwhile (or any other
/// file now at that path) used to be deleted unsent after the 2xx for the old bytes
/// (audit-concurrency-1103 crash M4). Only the file that was read and sent is removed. The
/// replacement here has the same length as the original *and* the original's mtime — what a sync
/// tool (robocopy `/COPYALL`, rsync `-t`, a cloud-drive client) produces — which on Windows is the
/// case the `same_file` stub misidentified as "unchanged" and deleted unsent.
#[test]
fn a_report_replaced_during_its_upload_is_not_deleted() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("replaced");
    let report = dir.join("crash-2026-10-06_00-00-01.txt");
    let original_mtime = std::fs::metadata(&report)
        .expect("stat")
        .modified()
        .expect("mtime");
    let replaced = report.clone();
    let (url, received) = spawn_server_with(
        200,
        1,
        Some(Box::new(move || {
            // While the upload waits for its answer, the path gets a different file: same
            // length ("fake crash body" is 15 bytes) and the original's mtime restored. Every
            // step must succeed: a hook that silently no-ops would leave the length/mtime
            // checks passing without the file identity being exercised at all
            // (audit fixrev-06-k L5), so a failure panics the server thread and fails the test.
            std::fs::remove_file(&replaced).expect("swap hook: remove the original report");
            std::fs::write(&replaced, "fake crash v2!!").expect("swap hook: write the replacement");
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&replaced)
                .expect("swap hook: reopen the replacement");
            file.set_modified(original_mtime)
                .expect("swap hook: restore the original mtime");
        })),
    );
    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.attempted, 1, "{summary:?}");
    assert_eq!(received.lock().expect("lock")[0].body, b"fake crash body");
    assert_eq!(
        std::fs::read_to_string(&report).ok().as_deref(),
        Some("fake crash v2!!"),
        "the replacement was never sent and must stay pending"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `.rejected` twin that already exists when a report is finally rejected (both files restored
/// from a synced logs folder, say) used to wedge the quarantine: on Windows `rename` never
/// replaces an existing destination, so the rejected report stayed pending and was re-dialed on
/// every launch; on Unix the rename silently overwrote the older quarantine. The old quarantine
/// is replaced deliberately and the report leaves the pending list on every platform.
#[test]
fn quarantine_replaces_an_existing_rejected_file() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("quarantine-collision");
    std::fs::write(
        dir.join("crash-2026-10-06_00-00-01.txt.rejected"),
        "older quarantine",
    )
    .expect("old quarantine");
    let (url, received) = spawn_server(404, 1);

    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(received.lock().expect("lock").len(), 1);
    assert!(
        regia_platform::crash::pending().is_empty(),
        "the report left the pending list"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("crash-2026-10-06_00-00-01.txt.rejected"))
            .ok()
            .as_deref(),
        Some("fake crash body"),
        "the new quarantined content replaced the older twin"
    );

    // A second start dials nothing.
    let again = reporter(&url).upload_pending();
    assert_eq!(again.attempted, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 32 symlinked `crash-*.txt` entries used to fill every launch's 32 report slots (refused at
/// read, never quarantined), so the real reports behind them were never attempted
/// (audit-concurrency-1103 crash L3).
#[cfg(unix)]
#[test]
fn refused_entries_do_not_use_up_the_per_launch_cap() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("crowded");
    let target = dir.join("elsewhere.dat");
    std::fs::write(&target, "not a report").expect("target");
    for i in 0..32 {
        // Older names than the real report, so they sort first.
        std::os::unix::fs::symlink(&target, dir.join(format!("crash-2026-10-01_00-{:02}-00.txt", i)))
            .expect("symlink");
    }
    let (url, received) = spawn_server(200, 1);
    let summary = reporter(&url).upload_pending();
    assert_eq!(summary.attempted, 1, "the real report is attempted: {summary:?}");
    assert_eq!(summary.uploaded, 1, "{summary:?}");
    assert_eq!(received.lock().expect("lock")[0].body, b"fake crash body");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The names of this process's threads (`/proc/self/task/*/comm`).
#[cfg(target_os = "linux")]
fn thread_names() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .map(|tasks| {
            tasks
                .filter_map(Result::ok)
                .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
                .map(|name| name.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The startup upload does blocking network IO and sleeps between retries; it used to run on the
/// shared IO pool (one thread on small machines), starving every other `spawn_io` user for the whole
/// upload (audit-concurrency-1103 crash M1). It runs on its own named thread.
#[cfg(target_os = "linux")]
#[test]
fn the_startup_upload_runs_on_its_own_thread() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = seed_crash_dir("own-thread");
    let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Vec<String>>();
    // While the uploader waits for this reply, its thread is alive: list the threads then.
    let (url, _received) = spawn_server_with(
        200,
        1,
        Some(Box::new(move || {
            let _ = seen_tx.send(thread_names());
        })),
    );
    let mut app = regia_app::App::new();
    app.insert_resource(reporter(&url))
        .add_plugins(regia_crash::CrashReporterPlugin);
    app.update();
    let names = seen_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the startup upload reached the server");
    assert!(
        names.iter().any(|name| name.starts_with("regia-crash-up")),
        "no dedicated upload thread among {names:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
