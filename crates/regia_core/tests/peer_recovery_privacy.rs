// SPDX-License-Identifier: MPL-2.0
//! CT-31: unclean-session reports are scrubbed before publication.
//! Raw `session.log` and `.hash` rows stay byte-identical. The published directory carries
//! `privacy.txt` so a later report-source job can tell a sanitized payload from a raw log.
use std::sync::{Mutex, MutexGuard};

use regia_core::log::session::{
    self, PeerLog, RECOVERY_PRIVACY_POLICY, RECOVERY_PRIVACY_POLICY_VERSION, RECOVERY_REPORT_FILES,
    RecoveryManifest, SessionOptions,
};
use regia_core::privacy::home_spellings;
use regia_test_utils::{TempDir, write_atomic};

fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

fn report_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut out = Vec::new();
    for name in RECOVERY_REPORT_FILES {
        let bytes = session::read_recovery_file(dir, name).expect("test operation");
        out.extend(bytes);
    }
    out
}

fn assert_absent(bytes: &[u8], needle: &str) {
    let needle = needle.as_bytes();
    assert!(
        !bytes.windows(needle.len()).any(|window| window == needle),
        "published report still contains a planted value"
    );
}

fn manifest(dir: &std::path::Path) -> RecoveryManifest {
    session::read_recovery_manifest(dir).expect("test operation")
}

#[test]
fn recovered_lines_are_scrubbed_and_local_evidence_stays() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-scrub");
    let home = std::env::var("HOME").expect("HOME");
    assert!(
        home.trim_end_matches(['/', '\\']).len() >= 3,
        "HOME is long enough for the privacy scrubber"
    );
    let mut spellings = home_spellings(&home);
    if let Ok(profile) = std::env::var("USERPROFILE")
        && profile.trim_end_matches(['/', '\\']).len() >= 3
    {
        spellings.extend(home_spellings(&profile));
    }
    spellings.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    spellings.dedup();
    assert!(!spellings.is_empty());
    let payload = format!(
        "{} token=synthetic-secret-value ann@example.com ann%40example.com",
        spellings.join(" ")
    );
    let options = SessionOptions {
        tail_bytes: 512 * 1024,
        tail_lines: 50,
        ..SessionOptions::default()
    };
    let old = PeerLog::create(&root, "001", 2, options).expect("test operation");
    old.hash_trace("world", 4, 0xabc).expect("test operation");
    old.line(tracing::Level::WARN, "net", format_args!("{payload}"))
        .expect("test operation");
    old.flush().expect("test operation");
    let log_path = old.directory().join("session.log");
    let hash_path = old.directory().join("world.hash");
    let log_before = std::fs::read(&log_path).expect("test operation");
    let hash_before = std::fs::read(&hash_path).expect("test operation");
    drop(old);

    let found = session::check_previous(&root, "002", 2, options)
        .expect("test operation")
        .expect("test operation");
    assert!(found.sanitized);
    assert!(found.complete);
    assert_eq!(found.policy_version, RECOVERY_PRIVACY_POLICY_VERSION);
    assert_eq!(std::fs::read(&log_path).expect("test operation"), log_before);
    assert_eq!(std::fs::read(&hash_path).expect("test operation"), hash_before);
    assert!(
        log_before
            .windows(b"synthetic-secret-value".len())
            .any(|w| w == b"synthetic-secret-value")
    );

    let bytes = report_bytes(&found.directory);
    assert_absent(&bytes, "synthetic-secret-value");
    assert_absent(&bytes, "ann@example.com");
    assert_absent(&bytes, "ann%40example.com");
    for spelling in &spellings {
        assert_absent(&bytes, spelling);
    }
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("[redacted]"), "{text}");
    assert!(text.contains("[email]"), "{text}");
    assert!(text.contains('~'), "{text}");
    let parsed = manifest(&found.directory);
    assert_eq!(parsed.policy, RECOVERY_PRIVACY_POLICY);
    assert_eq!(parsed.policy_version, RECOVERY_PRIVACY_POLICY_VERSION);
    assert!(parsed.sanitized);
    assert!(parsed.complete);
    assert!(!parsed.invalid_utf8);
    assert!(!parsed.partial_line);
    assert!(parsed.source_bytes <= options.tail_bytes);
    assert!(parsed.published_bytes <= options.tail_bytes);
    assert!(parsed.lines <= options.tail_lines as u64);
    assert!(session::is_published_recovery_report(&found.directory));
    assert!(session::read_recovery_file(&found.directory, "notes.txt").is_err());
    assert!(session::read_recovery_file(&found.directory, "../session.log").is_err());
}

#[test]
fn post_scrub_byte_cap_drops_lines_instead_of_cutting_a_token() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-cap");
    let old = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
    old.hash_trace("world", 1, 1).expect("test operation");
    old.line(tracing::Level::INFO, "net", format_args!("token=ab"))
        .expect("test operation");
    old.flush().expect("test operation");
    let log_path = old.directory().join("session.log");
    let hash_path = old.directory().join("world.hash");
    let log_before = std::fs::read(&log_path).expect("test operation");
    let hash_before = std::fs::read(&hash_path).expect("test operation");
    let raw_len = log_before.len() as u64;
    drop(old);
    let options = SessionOptions {
        tail_bytes: raw_len,
        tail_lines: 50,
        ..SessionOptions::default()
    };
    let found = session::check_previous(&root, "002", 0, options)
        .expect("test operation")
        .expect("test operation");
    let tail = std::fs::read(found.directory.join("lastlines.log")).expect("test operation");
    assert!(tail.len() as u64 <= raw_len);
    assert_absent(&tail, "token=ab");
    assert!(tail.windows(b"[redacted]".len()).any(|w| w == b"[redacted]"));
    let parsed = manifest(&found.directory);
    assert!(!parsed.complete);
    assert_eq!(parsed.published_bytes, tail.len() as u64);
    assert!(parsed.published_bytes <= raw_len);
    assert!(parsed.source_bytes <= raw_len);
    assert!(parsed.lines <= options.tail_lines as u64);
    assert_eq!(std::fs::read(&log_path).expect("test operation"), log_before);
    assert_eq!(std::fs::read(&hash_path).expect("test operation"), hash_before);
}

#[test]
fn tail_line_count_and_bytes_stay_inside_the_configured_bounds() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-bounds");
    let options = SessionOptions {
        tail_bytes: 4096,
        tail_lines: 4,
        ..SessionOptions::default()
    };
    let old = PeerLog::create(&root, "001", 1, options).expect("test operation");
    for tick in 0..30 {
        old.set_tick(tick);
        old.line(tracing::Level::INFO, "net", format_args!("event {tick}"))
            .expect("test operation");
    }
    old.flush().expect("test operation");
    drop(old);
    let found = session::check_previous(&root, "002", 1, options)
        .expect("test operation")
        .expect("test operation");
    let parsed = manifest(&found.directory);
    assert!(parsed.lines <= 4);
    assert!(parsed.published_bytes <= 4096);
    assert!(parsed.source_bytes <= 4096);
    let tail = std::fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
    assert_eq!(tail.lines().count() as u64, parsed.lines);
    assert_eq!(tail.len() as u64, parsed.published_bytes);
    assert!(found.lines <= 4);
}

#[test]
fn invalid_utf8_and_a_partial_tail_line_stay_explicit() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-explicit");
    let options = SessionOptions {
        tail_bytes: 64,
        tail_lines: 20,
        ..SessionOptions::default()
    };
    let old = PeerLog::create(&root, "001", 0, options).expect("test operation");
    old.line(tracing::Level::INFO, "net", format_args!("{}", "x".repeat(80)))
        .expect("test operation");
    old.set_tick(2);
    old.line(tracing::Level::INFO, "net", format_args!("last"))
        .expect("test operation");
    old.flush().expect("test operation");
    let log_path = old.directory().join("session.log");
    drop(old);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("test operation");
        file.write_all(b"\xff\xfebad\n").expect("test operation");
    }
    let log_before = std::fs::read(&log_path).expect("test operation");
    // The window is the whole file, so the invalid sequence is inside it and the lossy
    // tail must say so. The partial-line case is a second session below.
    let wide = SessionOptions {
        tail_bytes: log_before.len() as u64,
        tail_lines: 20,
        ..SessionOptions::default()
    };
    let found = session::check_previous(&root, "002", 0, wide)
        .expect("test operation")
        .expect("test operation");
    assert_eq!(std::fs::read(&log_path).expect("test operation"), log_before);
    let parsed = manifest(&found.directory);
    assert!(parsed.invalid_utf8);
    assert!(!parsed.complete);
    let tail = std::fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
    assert!(tail.contains('\u{FFFD}'));

    let root_partial = TempDir::new("peer-recover-partial");
    let partial_options = SessionOptions {
        tail_bytes: 65,
        tail_lines: 20,
        ..SessionOptions::default()
    };
    let old = PeerLog::create(&root_partial, "001", 0, partial_options).expect("test operation");
    old.line(tracing::Level::INFO, "net", format_args!("{}", "q".repeat(100)))
        .expect("test operation");
    old.set_tick(2);
    old.line(tracing::Level::INFO, "net", format_args!("kept"))
        .expect("test operation");
    old.flush().expect("test operation");
    drop(old);
    let found = session::check_previous(&root_partial, "002", 0, partial_options)
        .expect("test operation")
        .expect("test operation");
    let tail = std::fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
    assert_eq!(tail, "t=2 peer=0 INFO net kept\n");
    let parsed = manifest(&found.directory);
    assert!(parsed.partial_line);
    assert!(!parsed.complete);
    assert!(parsed.source_bytes <= 65);
    assert!(parsed.published_bytes <= 65);
    assert!(session::is_published_recovery_report(&found.directory));
}

#[test]
fn a_directory_session_log_is_rejected() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-dir-log");
    let old = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
    let directory = old.directory().to_owned();
    drop(old);
    std::fs::remove_file(directory.join("session.log")).expect("test operation");
    std::fs::create_dir(directory.join("session.log")).expect("test operation");
    assert!(session::check_previous(&root, "002", 0, SessionOptions::default()).is_err());
    assert!(!root.join("unclean-001-peer0").exists());
    assert!(!session::is_published_recovery_report(
        &root.join("unclean-001-peer0")
    ));
}

#[test]
#[cfg(unix)]
fn a_symlink_cannot_export_an_outside_file() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-link");
    let outside = TempDir::new("peer-recover-outside");
    let secret_path = outside.join("secret.txt");
    write_atomic(&secret_path, b"token=outside-secret-value ann@example.com\n").expect("test operation");
    let outside_before = std::fs::read(&secret_path).expect("test operation");
    let old = PeerLog::create(&root, "001", 5, SessionOptions::default()).expect("test operation");
    old.hash_trace("world", 3, 9).expect("test operation");
    old.flush().expect("test operation");
    let hash_before = std::fs::read(old.directory().join("world.hash")).expect("test operation");
    let session_dir = old.directory().to_owned();
    drop(old);
    std::fs::remove_file(session_dir.join("session.log")).expect("test operation");
    std::os::unix::fs::symlink(&secret_path, session_dir.join("session.log")).expect("test operation");
    let err =
        session::check_previous(&root, "002", 5, SessionOptions::default()).expect_err("test operation");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(!err.to_string().contains("outside-secret-value"));
    assert!(!root.join("unclean-001-peer5").exists());
    assert_eq!(
        std::fs::read(session_dir.join("world.hash")).expect("test operation"),
        hash_before
    );
    assert_eq!(
        std::fs::read(&secret_path).expect("test operation"),
        outside_before
    );
    let mut stack = vec![root.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("test operation") {
            let entry = entry.expect("test operation");
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).expect("test operation");
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if meta.is_file() {
                let bytes = std::fs::read(&path).expect("test operation");
                assert_absent(&bytes, "outside-secret-value");
            }
        }
    }

    let blocked = match PeerLog::create(&root, "002", 5, SessionOptions::default()) {
        Ok(_) => panic!("linked session.log must fail the next launch"),
        Err(err) => err,
    };
    assert!(!blocked.to_string().contains("outside-secret-value"));

    // A later launch cannot get past the linked log above. The output-link case
    // needs its own root so that refusal is not what this half is checking.
    let root = TempDir::new("peer-recover-redirect-root");
    let redirect = TempDir::new("peer-recover-redirect");
    write_atomic(&redirect.join("keep.txt"), b"untouched\n").expect("test operation");
    let old = PeerLog::create(&root, "003", 5, SessionOptions::default()).expect("test operation");
    old.line(tracing::Level::INFO, "net", format_args!("token=local-only"))
        .expect("test operation");
    old.flush().expect("test operation");
    drop(old);
    std::os::unix::fs::symlink(redirect.path(), root.join("unclean-003-peer5")).expect("test operation");
    assert!(
        session::check_previous(&root, "004", 5, SessionOptions::default())
            .expect("test operation")
            .is_none()
    );
    assert_eq!(
        std::fs::read(redirect.join("keep.txt")).expect("test operation"),
        b"untouched\n"
    );
    assert!(!redirect.join("report.txt").exists());
    assert!(!redirect.join("privacy.txt").exists());
    assert!(!session::is_published_recovery_report(redirect.path()));
}

#[test]
fn interrupted_publication_leaves_no_published_report() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-interrupt");
    let old = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
    old.line(
        tracing::Level::INFO,
        "net",
        format_args!("token=interrupt-secret"),
    )
    .expect("test operation");
    old.flush().expect("test operation");
    let log_before = std::fs::read(old.directory().join("session.log")).expect("test operation");
    drop(old);
    session::arm_recovery_publish_interrupt();
    assert!(session::check_previous(&root, "002", 0, SessionOptions::default()).is_err());
    assert!(!root.join("unclean-001-peer0").exists());
    for entry in std::fs::read_dir(root.path()).expect("test operation") {
        let entry = entry.expect("test operation");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(!name.starts_with("pending-"), "{name}");
        assert!(!session::is_published_recovery_report(&entry.path()));
    }
    assert_eq!(
        std::fs::read(root.join("001-peer0/session.log")).expect("test operation"),
        log_before
    );

    let pending = root.join("pending-planted");
    std::fs::create_dir(&pending).expect("test operation");
    write_atomic(
        &pending.join("privacy.txt"),
        b"policy=regia.recovery.privacy\npolicy_version=1\nsanitized=1\ncomplete=1\ninvalid_utf8=0\npartial_line=0\npublished_bytes=0\nsource_bytes=0\nlines=0\n",
    )
    .expect("test operation");
    write_atomic(&pending.join("report.txt"), b"token=interrupt-secret\n").expect("test operation");
    write_atomic(&pending.join("lastlines.log"), b"").expect("test operation");
    assert!(!session::is_published_recovery_report(&pending));
}

#[test]
#[cfg(unix)]
fn a_linked_manifest_is_not_a_published_report() {
    let _guard = lock();
    let root = TempDir::new("peer-recover-manifest-link");
    let outside = TempDir::new("peer-recover-manifest-outside");
    let planted = outside.join("privacy.txt");
    let planted_bytes = b"policy=regia.recovery.privacy\npolicy_version=1\nsanitized=1\ncomplete=1\ninvalid_utf8=0\npartial_line=0\npublished_bytes=0\nsource_bytes=0\nlines=0\ntoken=link-secret\n";
    write_atomic(&planted, planted_bytes).expect("test operation");
    let old = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
    old.flush().expect("test operation");
    drop(old);
    let found = session::check_previous(&root, "002", 0, SessionOptions::default())
        .expect("test operation")
        .expect("test operation");
    std::fs::remove_file(found.directory.join("privacy.txt")).expect("test operation");
    std::os::unix::fs::symlink(&planted, found.directory.join("privacy.txt")).expect("test operation");
    let err = session::read_recovery_manifest(&found.directory).expect_err("test operation");
    assert!(!err.to_string().contains("link-secret"));
    assert!(!session::is_published_recovery_report(&found.directory));
    assert_eq!(std::fs::read(&planted).expect("test operation"), planted_bytes);
}
