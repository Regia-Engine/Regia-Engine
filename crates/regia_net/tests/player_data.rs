// SPDX-License-Identifier: MPL-2.0
//! `known_hosts.ron` on the player's machine (ADR-3102 with ADR-2416,
//! `book/user-guide/player-data.md`).
//!
//! A table-driven sweep of the read side: truncated, garbage, empty, not-UTF-8, newer
//! `format_version`, an interrupted write (a stray temp file) and a read-only directory.
//! `KnownHosts::load` (what `NetLink` and the `net.*` agent methods call) stays fail-closed: a
//! code, and the file byte for byte as it was. `KnownHosts::load_or_recover` (a game's "reset
//! trusted servers") moves the file aside to `.bad-<n>`, logs one warn line and starts empty.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_net::ServerKey;
use regia_net::known_hosts::{KNOWN_HOSTS_FILE, KnownHosts, MAX_BAD_STORE_COPIES};

/// A fresh directory unique to this process and call (the `unique_temp_dir` rule).
fn unique_temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-net-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = sink
        .0
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    (out, text)
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A store pinning one host.
fn good() -> KnownHosts {
    let mut store = KnownHosts::new();
    store.check("game.example:7777", ServerKey([7; 32])).expect("pin");
    store
}

#[test]
fn known_hosts_reads_sweep_damage_and_versions() {
    let text = good().to_ron();
    let truncated = text.as_bytes()[..text.len() / 2].to_vec();
    let newer = text.replace("format_version: 1", "format_version: 2");
    assert_ne!(newer, text);
    // (name, bytes, stray temp file, expected strict code or None when it loads)
    type Case<'a> = (&'a str, Vec<u8>, Option<(&'a str, &'a [u8])>, Option<&'a str>);
    let cases: Vec<Case<'_>> = vec![
        ("good", text.clone().into_bytes(), None, None),
        ("truncated", truncated, None, Some("net_known_hosts_unreadable")),
        (
            "garbage",
            b"not ron at all".to_vec(),
            None,
            Some("net_known_hosts_unreadable"),
        ),
        ("empty", Vec::new(), None, Some("net_known_hosts_unreadable")),
        (
            "not UTF-8",
            vec![b'(', 0xFF, 0xFE],
            None,
            Some("net_known_hosts_unreadable"),
        ),
        (
            "newer format_version",
            newer.into_bytes(),
            None,
            Some("net_known_hosts_newer"),
        ),
        (
            "interrupted write (stray temp file)",
            text.clone().into_bytes(),
            Some(("known_hosts.ron.tmp4242-0", b"(format_ver")),
            None,
        ),
    ];
    for (name, bytes, stray, code) in cases {
        let dir = unique_temp_dir("known-hosts-sweep");
        let path = dir.join(KNOWN_HOSTS_FILE);
        std::fs::write(&path, &bytes).expect("stage");
        if let Some((stray_name, stray_bytes)) = stray {
            std::fs::write(dir.join(stray_name), stray_bytes).expect("stray");
        }
        let staged = files(&dir);

        // Strict: fail-closed, nothing moves.
        match (KnownHosts::load(&path), code) {
            (Ok(store), None) => assert_eq!(store, good(), "{name}"),
            (Err(e), Some(code)) => assert_eq!(e.code(), code, "{name}"),
            (other, _) => panic!("{name}: unexpected {other:?}"),
        }
        assert_eq!(files(&dir), staged, "{name}: the strict load moves nothing");

        // Recovering: moved aside, one warn line, an empty store.
        let ((store, moved), log) = capture(|| KnownHosts::load_or_recover(&path).expect(name));
        let bad = dir.join("known_hosts.ron.bad-1");
        if code.is_none() {
            assert_eq!(store, good(), "{name}");
            assert_eq!(moved, None, "{name}");
            assert_eq!(files(&dir), staged, "{name}");
            assert!(!log.contains("WARN"), "{name}: {log}");
        } else {
            assert!(store.is_empty(), "{name}: no trusted hosts");
            assert_eq!(moved.as_deref(), Some(bad.as_path()), "{name}");
            assert!(!path.exists(), "{name}");
            assert_eq!(
                std::fs::read(&bad).expect("copy"),
                bytes,
                "{name}: the old pins are kept"
            );
            assert_eq!(log.matches(" WARN ").count(), 1, "{name}: {log}");
            assert!(
                log.contains("known_hosts.ron") && log.contains("known_hosts.ron.bad-1"),
                "{name}: {log}"
            );
            assert!(log.contains(code.unwrap_or_default()), "{name}: the code: {log}");
            // The next pin saves a fresh store beside the copy.
            good().save(&path).expect("save after reset");
            assert_eq!(std::fs::read(&bad).expect("copy"), bytes, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn damaged_store_copies_are_bounded() {
    let dir = unique_temp_dir("known-hosts-bounded");
    let path = dir.join(KNOWN_HOSTS_FILE);
    for round in 1..=(MAX_BAD_STORE_COPIES + 1) {
        std::fs::write(&path, format!("broken {round}")).expect("stage");
        let (result, _) = capture(|| KnownHosts::load_or_recover(&path));
        assert!(result.is_ok(), "round {round}");
    }
    assert_eq!(files(&dir).len(), MAX_BAD_STORE_COPIES as usize);
    let first = std::fs::read_to_string(dir.join("known_hosts.ron.bad-1")).expect("first");
    assert_eq!(first, "broken 2", "the oldest was dropped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A damaged store in a read-only directory cannot be moved: both loads refuse and the file is
/// byte for byte what it was.
#[cfg(unix)]
#[test]
fn a_damaged_store_that_cannot_be_moved_is_left_as_it_is() {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_temp_dir("known-hosts-readonly");
    let path = dir.join(KNOWN_HOSTS_FILE);
    std::fs::write(&path, b"broken").expect("stage");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("read-only");
    if std::fs::write(dir.join(".probe"), b"x").is_ok() {
        eprintln!("read-only directories are not enforced here (root?); case skipped");
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let err = KnownHosts::load_or_recover(&path).expect_err("cannot move");
    assert_eq!(err.code(), "net_known_hosts_unreadable");
    assert!(err.to_string().contains("cannot be moved aside"), "{err}");
    // A save into the read-only directory fails and keeps the old file.
    let err = good().save(&path).expect_err("read-only");
    assert_eq!(err.code(), "net_known_hosts_write_failed");
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    assert_eq!(std::fs::read(&path).expect("kept"), b"broken");
    assert_eq!(files(&dir), vec![KNOWN_HOSTS_FILE]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// corr-net (ADR-5580): a store the process cannot open (permissions; on Windows a sharing
/// violation while a scanner holds it) is a read failure, not a damaged store. The recovering
/// load leaves it in place, as its `# Errors` says, instead of moving every trust pin aside.
#[cfg(unix)]
#[test]
fn corr_net_a_store_that_cannot_be_opened_is_not_moved_aside() {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_temp_dir("known-hosts-noread");
    let path = dir.join(KNOWN_HOSTS_FILE);
    good().save(&path).expect("stage a valid store");
    let bytes = std::fs::read(&path).expect("staged");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::File::open(&path).is_ok() {
        eprintln!("file permissions are not enforced here (root?); case skipped");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let err = KnownHosts::load_or_recover(&path).expect_err("a read failure is an error");
    assert_eq!(err.code(), "net_known_hosts_unreadable");
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    assert_eq!(files(&dir), vec![KNOWN_HOSTS_FILE], "nothing was moved aside");
    assert_eq!(std::fs::read(&path).expect("kept"), bytes, "the pins are intact");
    let _ = std::fs::remove_dir_all(&dir);
}
