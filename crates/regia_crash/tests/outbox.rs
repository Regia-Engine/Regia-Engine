// SPDX-License-Identifier: MPL-2.0
//! The incremental report outbox (plan row NL-08, ADR-8402): offset ledger, capped parts and the
//! crash-marker resend, driven through a fake [`UploadTransport`] (no network, no clock).

use std::io::Write;
use std::path::{Path, PathBuf};

use regia_crash::CrashReporter;
use regia_crash::outbox::{Outbox, OutboxSettings, OutboxSources};
use regia_crash::transport::{TransportError, UploadTransport};
use regia_test_utils::unique_temp_dir;

/// One part the fake server received.
#[derive(Debug, Clone)]
struct Got {
    name: String,
    body: Vec<u8>,
}

/// A fake transport: records every part and answers from a script (`Ok(200)` once the script is
/// used up).
#[derive(Default)]
struct Fake {
    got: Vec<Got>,
    script: Vec<Result<u16, ()>>,
    calls: usize,
}

impl UploadTransport for Fake {
    fn post_part(&mut self, name: &str, body: &[u8]) -> Result<u16, TransportError> {
        let answer = self.script.get(self.calls).copied().unwrap_or(Ok(200));
        self.calls += 1;
        match answer {
            Ok(status) => {
                if (200..300).contains(&status) {
                    self.got.push(Got {
                        name: name.to_owned(),
                        body: body.to_vec(),
                    });
                }
                Ok(status)
            }
            Err(()) => Err(TransportError::Io {
                host: "fake".into(),
                source: std::io::Error::new(std::io::ErrorKind::ConnectionReset, "interrupted"),
            }),
        }
    }
}

fn reporter() -> CrashReporter {
    CrashReporter {
        endpoint: "https://reports.example.com/outbox".into(),
        consent: true,
        ..CrashReporter::default()
    }
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log");
    file.write_all(text.as_bytes()).expect("append");
}

fn sources(logs: &[&Path], crash_dir: Option<&Path>) -> OutboxSources {
    OutboxSources {
        logs: logs.iter().map(|p| p.to_path_buf()).collect(),
        crash_dir: crash_dir.map(Path::to_path_buf),
        live: None,
    }
}

fn concat(got: &[Got]) -> Vec<u8> {
    got.iter().flat_map(|g| g.body.iter().copied()).collect()
}

fn ledger(dir: &Path) -> PathBuf {
    dir.join("outbox.ron")
}

#[test]
fn second_upload_sends_only_new_tails() {
    let dir = unique_temp_dir("outbox-tails");
    let log = dir.join("2026-11-17_10-00-00.log");
    append(&log, "first line\nsecond line\n");
    let mut fake = Fake::default();
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let first = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(first.parts_sent, 1, "{first:?}");
    assert_eq!(concat(&fake.got), b"first line\nsecond line\n");

    append(&log, "third line\n");
    // A new process reads the ledger back from disk.
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let second = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(second.parts_sent, 1, "{second:?}");
    assert_eq!(fake.got.len(), 2);
    assert_eq!(fake.got[1].body, b"third line\n", "only the new tail is sent");
    assert!(fake.got[1].name.contains(";23-34"), "{}", fake.got[1].name);

    // Nothing new: nothing sent.
    let third = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(third.parts_sent, 0);
    assert_eq!(fake.got.len(), 2);
    let status = outbox.status(&sources(&[&log], None));
    assert_eq!(status.pending_bytes, 0);
    assert_eq!(status.sources[0].confirmed, 34);
}

#[test]
fn a_rotated_log_starts_a_new_ledger_row() {
    let dir = unique_temp_dir("outbox-rotate");
    let log = dir.join("game.log");
    append(&log, "old session line one\nold session line two\n");
    let mut fake = Fake::default();
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(outbox.ledger().rows.len(), 1);

    // The writer rotates: same path, new content that is longer than the old confirmed offset,
    // so only the header hash can tell.
    std::fs::remove_file(&log).expect("rotate");
    append(
        &log,
        "NEW session line one, which is long\nnew session line two\n",
    );
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let flush = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(flush.rotated, 1, "{flush:?}");
    let rows = &outbox.ledger().rows;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0].retired && !rows[1].retired, "{rows:?}");
    assert_eq!(rows[1].confirmed, 57);
    assert_eq!(
        fake.got[1].body, b"NEW session line one, which is long\nnew session line two\n",
        "the rotated file is sent from byte 0"
    );

    // A shorter replacement rotates too.
    std::fs::remove_file(&log).expect("rotate");
    append(&log, "x\n");
    let flush = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(flush.rotated, 1);
    assert_eq!(fake.got[2].body, b"x\n");
    assert_eq!(outbox.ledger().rows.len(), 3);
}

#[test]
fn a_crash_marker_resends_the_session_once() {
    let dir = unique_temp_dir("outbox-crash");
    let older = dir.join("2026-11-17_09-00-00.log");
    let crashed = dir.join("2026-11-17_10-00-00.log");
    let newer = dir.join("2026-11-17_11-00-00.log");
    append(&older, "older session\n");
    append(&crashed, "crashed session part one\n");
    let mut fake = Fake::default();
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let src = sources(&[&older, &crashed, &newer], Some(&dir));
    outbox.flush(&reporter(), &src, &mut fake);
    assert_eq!(fake.got.len(), 2);

    // The session goes on, then crashes: the platform writes its crash marker, and the next
    // session starts its own log.
    append(&crashed, "crashed session part two\n");
    std::fs::write(dir.join("crash-2026-11-17_10-30-00.txt"), "panic: boom\n").expect("marker");
    append(&newer, "next session\n");
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let flush = outbox.flush(&reporter(), &src, &mut fake);
    assert_eq!(flush.crash_resends, 1, "{flush:?}");
    let resent: Vec<&Got> = fake.got[2..]
        .iter()
        .filter(|g| g.name.contains("10-00-00"))
        .collect();
    assert_eq!(resent.len(), 1, "{:?}", fake.got);
    assert_eq!(
        resent[0].body, b"crashed session part one\ncrashed session part two\n",
        "the whole session log is sent again"
    );
    assert!(resent[0].name.ends_with(";crash"), "{}", resent[0].name);
    assert!(
        !fake.got[2..].iter().any(|g| g.name.contains("09-00-00")),
        "the older session is not resent"
    );

    // Once: the same marker never forces another resend, even after the reporter removed it.
    let count = fake.got.len();
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let again = outbox.flush(&reporter(), &src, &mut fake);
    assert_eq!(again.crash_resends, 0);
    assert_eq!(fake.got.len(), count);
    std::fs::rename(
        dir.join("crash-2026-11-17_10-30-00.txt"),
        dir.join("crash-2026-11-17_10-30-00.txt.rejected"),
    )
    .expect("quarantine");
    let again = outbox.flush(&reporter(), &src, &mut fake);
    assert_eq!(again.crash_resends, 0);
    assert_eq!(fake.got.len(), count);
}

#[test]
fn an_interrupted_part_is_resent_not_skipped() {
    let dir = unique_temp_dir("outbox-interrupt");
    let log = dir.join("2026-11-17_12-00-00.log");
    let line = "0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\n";
    let mut text = String::new();
    while text.len() < 20 * 1024 {
        text.push_str(line);
    }
    append(&log, &text);
    let settings = OutboxSettings {
        part_cap: 4096,
        ..OutboxSettings::default()
    };
    // The second part is cut off mid-send.
    let mut fake = Fake {
        script: vec![Ok(200), Err(())],
        ..Fake::default()
    };
    let mut outbox = Outbox::open(ledger(&dir), settings.clone());
    let first = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(first.parts_sent, 1, "{first:?}");
    assert_eq!(first.failed, 1);
    let confirmed = outbox.ledger().rows[0].confirmed;
    assert_eq!(confirmed, fake.got[0].body.len() as u64);
    let status = outbox.status(&sources(&[&log], None));
    assert!(status.sources[0].last_error.is_some(), "{status:?}");
    assert_eq!(status.pending_bytes, text.len() as u64 - confirmed);

    // A 503 is not a confirmation either.
    fake.script.push(Err(()));
    fake.script[2] = Ok(503);
    let mut outbox = Outbox::open(ledger(&dir), settings.clone());
    let second = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(second.parts_sent, 0);
    assert_eq!(outbox.ledger().rows[0].confirmed, confirmed);

    let mut outbox = Outbox::open(ledger(&dir), settings);
    let third = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert!(third.failed == 0 && third.parts_sent >= 4, "{third:?}");
    // Every byte arrived exactly once and in order: the interrupted part was sent again.
    assert_eq!(concat(&fake.got), text.as_bytes());
    let mut next = 0u64;
    for got in &fake.got {
        let range = got.name.rsplit(';').next().expect("range");
        let (start, end) = range.split_once('-').expect("start-end");
        assert_eq!(start.parse::<u64>().expect("start"), next, "{}", got.name);
        next = end.parse().expect("end");
    }
    assert!(
        outbox.status(&sources(&[&log], None)).sources[0]
            .last_error
            .is_none()
    );
}

#[test]
fn no_part_exceeds_the_cap() {
    let dir = unique_temp_dir("outbox-cap");
    let log = dir.join("2026-11-17_13-00-00.log");
    // Lines of every length, one line longer than the cap, invalid UTF-8 (scrubbing makes each
    // bad byte three bytes) and multibyte characters on the cut.
    let mut bytes = Vec::new();
    for n in 0..400usize {
        bytes.extend(std::iter::repeat_n(b'a' + (n % 26) as u8, n % 97));
        bytes.extend("é€".as_bytes());
        bytes.push(b'\n');
    }
    bytes.extend(std::iter::repeat_n(b'z', 9000));
    bytes.push(b'\n');
    bytes.extend(std::iter::repeat_n(0xffu8, 6000));
    bytes.extend("ünïcödé".repeat(800).as_bytes());
    bytes.push(b'\n');
    std::fs::write(&log, &bytes).expect("log");
    let cap = 4096u64;
    let settings = OutboxSettings {
        part_cap: cap,
        max_parts_per_flush: 1000,
        ..OutboxSettings::default()
    };
    let mut fake = Fake::default();
    let mut outbox = Outbox::open(ledger(&dir), settings);
    let flush = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(flush.failed, 0, "{flush:?}");
    assert!(fake.got.len() > 10);
    let mut replaced = 0;
    for got in &fake.got {
        assert!(
            got.body.len() as u64 <= cap,
            "{} is {} bytes",
            got.name,
            got.body.len()
        );
        let text = std::str::from_utf8(&got.body).expect("parts are UTF-8");
        replaced += text.chars().filter(|&c| c == '\u{FFFD}').count();
    }
    // Only the 6000 invalid bytes became U+FFFD: no cut split a multibyte character.
    assert_eq!(replaced, 6000);
    assert_eq!(outbox.ledger().rows[0].confirmed, bytes.len() as u64);
    // The settings floor: a cap under 4 KiB is raised to it.
    let tiny = OutboxSettings {
        part_cap: 10,
        ..OutboxSettings::default()
    };
    assert_eq!(tiny.effective_part_cap(), 4096);
    assert_eq!(OutboxSettings::default().part_cap, 256 * 1024);
}

#[test]
fn nothing_uploads_without_consent() {
    let dir = unique_temp_dir("outbox-consent");
    let log = dir.join("2026-11-17_14-00-00.log");
    append(&log, "user@example.com logged in from /home/someone\n");
    std::fs::write(dir.join("crash-2026-11-17_14-10-00.txt"), "panic\n").expect("marker");
    let src = sources(&[&log], Some(&dir));
    let mut fake = Fake::default();
    for blocked in [
        CrashReporter::default(),
        CrashReporter {
            consent: false,
            ..reporter()
        },
        CrashReporter {
            endpoint: String::new(),
            ..reporter()
        },
        CrashReporter {
            endpoint: "http://reports.example.com/outbox".into(),
            ..reporter()
        },
    ] {
        let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
        let flush = outbox.flush(&blocked, &src, &mut fake);
        assert!(flush.disabled, "{flush:?}");
        assert_eq!(fake.calls, 0, "the transport is never called");
    }
    assert!(!ledger(&dir).exists(), "a disabled outbox writes no state");

    // With consent the same part goes out scrubbed.
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    let flush = outbox.flush(&reporter(), &src, &mut fake);
    assert!(!flush.disabled);
    let body = String::from_utf8(concat(&fake.got)).expect("utf8");
    assert!(!body.contains("user@example.com"), "{body}");
    assert!(body.contains("[email]"), "{body}");
}

#[test]
fn the_ledger_ages_out_old_rows_by_count() {
    let dir = unique_temp_dir("outbox-aging");
    let settings = OutboxSettings {
        max_rows: 3,
        ..OutboxSettings::default()
    };
    let mut fake = Fake::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    for n in 0..6 {
        let log = dir.join(format!("2026-11-17_15-00-0{n}.log"));
        append(&log, &format!("session {n}\n"));
        paths.push(log);
        // The log writer keeps the newest two session files, like the engine's file sink.
        if paths.len() > 2 {
            std::fs::remove_file(paths.remove(0)).expect("prune");
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let mut outbox = Outbox::open(ledger(&dir), settings.clone());
        outbox.flush(&reporter(), &sources(&refs, None), &mut fake);
        assert!(outbox.ledger().rows.len() <= 3, "{:?}", outbox.ledger().rows);
    }
    let outbox = Outbox::open(ledger(&dir), settings.clone());
    let kept: Vec<String> = outbox
        .ledger()
        .rows
        .iter()
        .map(|r| r.path.file_name().expect("name").to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        kept,
        [
            "2026-11-17_15-00-03.log",
            "2026-11-17_15-00-04.log",
            "2026-11-17_15-00-05.log"
        ],
        "the newest rows are kept"
    );
    assert_eq!(fake.got.len(), 6, "every session went out once");

    // More listed sources than rows: the bound still holds; the oldest listed row goes first.
    let mut extra = Vec::new();
    for n in 0..5 {
        let log = dir.join(format!("2026-11-17_16-00-0{n}.log"));
        append(&log, "x\n");
        extra.push(log);
    }
    let refs: Vec<&Path> = extra.iter().map(PathBuf::as_path).collect();
    let mut outbox = Outbox::open(ledger(&dir), settings);
    outbox.flush(&reporter(), &sources(&refs, None), &mut fake);
    assert_eq!(outbox.ledger().rows.len(), 3);
    assert_eq!(outbox.ledger().rows[2].path, extra[4]);
}

#[test]
fn a_corrupt_ledger_starts_over_and_says_so() {
    let dir = unique_temp_dir("outbox-corrupt");
    std::fs::write(ledger(&dir), "this is not ron (").expect("ledger");
    let outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    assert!(outbox.ledger().rows.is_empty());
    assert!(outbox.load_error().is_some());
    std::fs::write(
        ledger(&dir),
        "(version: 99, next_seq: 0, rows: [], crash_markers: [])",
    )
    .expect("ledger");
    let outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    assert!(
        outbox.load_error().is_some_and(|e| e.contains("version")),
        "{:?}",
        outbox.load_error()
    );
}

#[test]
fn a_private_key_block_is_never_split_across_parts() {
    let dir = unique_temp_dir("outbox-key");
    let log = dir.join("2026-11-17_17-00-00.log");
    let mut text = String::new();
    for n in 0..40 {
        text.push_str(&format!(
            "line {n} before the key, padding padding padding padding padding\n"
        ));
    }
    text.push_str("-----BEGIN RSA PRIVATE KEY-----\n");
    for _ in 0..80 {
        text.push_str("MIIEpAIBAAKCAQEAzSECRETSECRETSECRETSECRETSECRETSECRETSECRET\n");
    }
    text.push_str("-----END RSA PRIVATE KEY-----\nafter the key\n");
    append(&log, &text);
    let settings = OutboxSettings {
        part_cap: 4096,
        ..OutboxSettings::default()
    };
    let mut fake = Fake::default();
    let mut outbox = Outbox::open(ledger(&dir), settings);
    let flush = outbox.flush(&reporter(), &sources(&[&log], None), &mut fake);
    assert_eq!(flush.failed, 0, "{flush:?}");
    assert!(fake.got.len() >= 2, "{:?}", fake.got.len());
    let all = String::from_utf8(concat(&fake.got)).expect("utf8");
    assert!(!all.contains("SECRET"), "key material left the machine");
    assert!(all.contains("[redacted]"));
    assert!(all.contains("line 39 before the key"));
    assert!(all.ends_with("after the key\n"), "{all}");
    assert_eq!(outbox.ledger().rows[0].confirmed, text.len() as u64);
    for got in &fake.got {
        assert!(got.body.len() <= 4096, "{}", got.name);
    }
}

#[test]
fn the_live_log_holds_its_unfinished_last_line() {
    let dir = unique_temp_dir("outbox-live");
    let older = dir.join("2026-11-17_17-30-00.log");
    let live = dir.join("2026-11-17_18-00-00.log");
    append(&older, "old\nno newline at the end");
    append(&live, "done line\nhalf a li");
    std::fs::write(dir.join("notes.txt"), "not a log").expect("other file");
    let src = OutboxSources::from_logs_dir(&dir);
    assert_eq!(src.logs, vec![older.clone(), live.clone()]);
    assert_eq!(src.live.as_deref(), Some(live.as_path()));
    assert_eq!(src.crash_dir.as_deref(), Some(dir.as_path()));

    let mut fake = Fake::default();
    let mut outbox = Outbox::open(ledger(&dir), OutboxSettings::default());
    outbox.flush(&reporter(), &src, &mut fake);
    assert_eq!(fake.got.len(), 2, "{:?}", fake.got);
    assert_eq!(
        fake.got[0].body, b"old\nno newline at the end",
        "a finished session is sent whole"
    );
    assert_eq!(fake.got[1].body, b"done line\n", "the live line waits");
    assert_eq!(outbox.status(&src).sources[1].pending_bytes, 9);

    append(&live, "ne\n");
    let src = OutboxSources::from_logs_dir(&dir);
    outbox.flush(&reporter(), &src, &mut fake);
    assert_eq!(fake.got[2].body, b"half a line\n");
    assert_eq!(outbox.status(&src).pending_bytes, 0);
}

/// A loopback collector: answers 200 to `accept` requests and returns each report name and body.
fn loopback_collector(accept: usize) -> (String, std::thread::JoinHandle<Vec<Got>>) {
    use std::io::{BufRead, BufReader, Read};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/outbox", listener.local_addr().expect("addr"));
    let handle = std::thread::spawn(move || {
        let mut got = Vec::new();
        for _ in 0..accept {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let (mut length, mut name) = (0usize, String::new());
            loop {
                line.clear();
                reader.read_line(&mut line).expect("header");
                let header = line.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some(v) = header.strip_prefix("Content-Length:") {
                    length = v.trim().parse().expect("length");
                }
                if let Some(v) = header.strip_prefix("X-Regia-Report-Name:") {
                    name = v.trim().to_owned();
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("body");
            got.push(Got { name, body });
            let mut stream = reader.into_inner();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("reply");
        }
        got
    });
    (url, handle)
}

#[test]
fn the_http_transport_carries_parts_to_a_loopback_collector() {
    let dir = unique_temp_dir("outbox-http");
    let log = dir.join("2026-11-17_19-00-00.log");
    let line = "a loopback line of some length, padded padded padded padded padded\n";
    append(&log, &line.repeat(100));
    let (url, collector) = loopback_collector(2);
    let reporter = CrashReporter {
        endpoint: url.clone(),
        consent: true,
        ..CrashReporter::default()
    };
    let settings = OutboxSettings {
        part_cap: 4096,
        ..OutboxSettings::default()
    };
    let mut transport = regia_crash::transport::HttpTransport::new(url);
    let mut outbox = Outbox::open(ledger(&dir), settings);
    let flush = outbox.flush(&reporter, &sources(&[&log], None), &mut transport);
    assert_eq!((flush.parts_sent, flush.failed), (2, 0), "{flush:?}");
    let got = collector.join().expect("collector");
    assert_eq!(got[0].name, "2026-11-17_19-00-00.log;0-4087");
    assert_eq!(got[1].name, "2026-11-17_19-00-00.log;4087-6700");
    assert_eq!(concat(&got), line.repeat(100).as_bytes());
}
