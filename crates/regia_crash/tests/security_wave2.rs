// SPDX-License-Identifier: MPL-2.0
//! Threat-model finding 16 (ADR-3285): only the log lines inside a crash report went through the
//! scrubber. The panic message, location, backtrace, the other fields and the whole match log went
//! out as they were once the player consented. The first test is the threat-model reproducer,
//! un-ignored; the others plant a home folder, a token and an email in every uploaded field.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use regia_crash::CrashReporter;
use regia_platform::OsInfo;
use regia_platform::crash::BuildInfo;

/// One received request: its `X-Regia-Report-Name` header and its body.
struct Received {
    name: String,
    body: String,
}

/// A server that answers `requests` requests with 200 and hands back each one's name and body.
fn server(requests: usize) -> (String, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/upload", listener.local_addr().expect("addr"));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..requests {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let Ok(clone) = stream.try_clone() else {
                return;
            };
            let mut reader = BufReader::new(clone);
            let mut content_length = 0_usize;
            let mut name = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                    break;
                }
                let line = line.trim_end();
                if let Some(v) = line.strip_prefix("Content-Length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if let Some(v) = line.strip_prefix("X-Regia-Report-Name:") {
                    name = v.trim().to_string();
                }
            }
            let mut body = vec![0_u8; content_length];
            let _ = reader.read_exact(&mut body);
            let mut stream = stream;
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send(Received {
                name,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
    });
    (url, rx)
}

fn reporter(endpoint: String) -> CrashReporter {
    CrashReporter {
        endpoint,
        consent: true,
        max_retries: 0,
        backoff: Duration::from_millis(1),
        ..CrashReporter::default()
    }
}

/// This user's home folder as the scrubber sees it, when it is long enough to be scrubbed (a bare
/// `/` never is).
fn home() -> Option<String> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|home| home.trim_end_matches(['/', '\\']).to_string())
        .find(|home| home.len() >= 3)
}

/// A home folder, a token and an email, tagged with the field they are planted in.
fn plant(field: &str) -> String {
    let home = home().unwrap_or_default();
    format!("{field}: {home}/saves/x.sav token=tok-{field} mail {field}.ann@example.com")
}

fn assert_clean(what: &str, text: &str) {
    if let Some(home) = home() {
        assert!(!text.contains(&home), "{what} kept the home folder:\n{text}");
    }
    assert!(!text.contains("tok-"), "{what} kept a token:\n{text}");
    assert!(!text.contains("hunter2"), "{what} kept a password:\n{text}");
    assert!(!text.contains("@example.com"), "{what} kept an email:\n{text}");
}

/// Finding 16: the whole match log went out as it was.
#[test]
fn finding_16_an_uploaded_match_log_has_its_secrets_redacted() {
    let (url, body) = server(1);
    reporter(url)
        .upload_match_log("m.log", b"joined with password=hunter2\n")
        .expect("uploads");
    let sent = body
        .recv_timeout(Duration::from_secs(30))
        .expect("the server got it");
    assert!(!sent.body.contains("hunter2"), "{}", sent.body);
}

#[test]
fn a_match_log_and_its_name_are_scrubbed_of_a_home_a_token_and_an_email() {
    let (url, body) = server(1);
    let log = format!("{}\n{}\n", plant("line1"), plant("line2"));
    reporter(url)
        .upload_match_log("match-ann@example.com.log", log.as_bytes())
        .expect("uploads");
    let sent = body
        .recv_timeout(Duration::from_secs(30))
        .expect("the server got it");
    assert_clean("the match log", &sent.body);
    assert_clean("the report name", &sent.name);
    assert!(sent.body.contains("token=[redacted]"), "{}", sent.body);
    assert!(!sent.name.is_empty());
}

fn section() -> String {
    plant("section")
}

/// One test drives the process-global crash context (directory, GPU string, level, sections), so
/// nothing else in this binary may configure it.
#[test]
fn every_field_of_a_pending_report_is_scrubbed_before_upload() {
    let dir = regia_test_utils::unique_temp_dir("security-wave2-crash");
    let build = BuildInfo {
        version: plant("version"),
        git_hash: plant("git"),
        profile: plant("profile"),
        engine: plant("engine"),
    };
    let mut os = OsInfo::detect();
    os.version = plant("osversion");
    os.device_model = plant("device");
    regia_platform::crash::configure(Some(dir.clone()), build, os);
    regia_platform::crash::set_gpu_info(plant("gpu"));
    regia_platform::crash::set_active_level(plant("level"));
    regia_platform::crash::add_report_section("security_wave2", section);
    let written = regia_platform::crash::write_device_lost_report(
        "device_lost",
        &plant("reason"),
        &plant("message"),
        3,
    )
    .expect("a report is written");
    // The file on disk is already scrubbed (the agent's diagnostics.crash_reports reads it too).
    let on_disk = std::fs::read_to_string(&written).expect("read the report");
    assert_clean("the written report", &on_disk);
    assert!(on_disk.contains("gpu: gpu: "), "{on_disk}");
    // A report an older build wrote raw is scrubbed on its way out.
    let raw = format!(
        "Regia Engine crash report\npanic: {}\nlocation: {}\nbacktrace:\n{}\n",
        plant("panic"),
        plant("location"),
        plant("frame")
    );
    std::fs::write(dir.join("crash-2000-01-01_00-00-00.txt"), raw).expect("plant an old report");

    let (url, bodies) = server(2);
    let summary = reporter(url).upload_pending();
    assert_eq!(summary.uploaded, 2, "{summary:?}");
    for _ in 0..2 {
        let sent = bodies
            .recv_timeout(Duration::from_secs(30))
            .expect("the server got it");
        assert_clean("an uploaded report", &sent.body);
        assert!(sent.body.contains("[redacted]"), "{}", sent.body);
        assert!(sent.body.contains("[email]"), "{}", sent.body);
    }
    regia_platform::crash::configure(None, BuildInfo::detect(), OsInfo::detect());
    regia_platform::crash::set_gpu_info("");
    regia_platform::crash::set_active_level("");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_utf8_in_a_match_log_still_uploads_scrubbed() {
    let (url, body) = server(1);
    let mut log = b"\xff\xfe binary prefix ".to_vec();
    log.extend_from_slice(plant("tail").as_bytes());
    reporter(url).upload_match_log("m.log", &log).expect("uploads");
    let sent = body
        .recv_timeout(Duration::from_secs(30))
        .expect("the server got it");
    assert_clean("a non-UTF-8 match log", &sent.body);
}
