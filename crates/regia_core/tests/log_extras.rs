// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-log/src/session.rs, commit d6ed37677), ADR-8012.
//! NL-07 acceptance: peer-local checkpoints, session recovery, and developer gates.
use regia_core::log::{
    devenv,
    session::{PeerLog, SessionOptions},
};
use regia_test_utils::TempDir;

/// `a_live_child_lease_is_skipped_then_a_hard_exit_is_recovered` spawns a child of this binary.
/// Between its fork and its exec the child holds a copy of every open descriptor of this process,
/// including the `active.lock` leases of sessions other tests hold here, so a sibling test that
/// dropped a `PeerLog` and created the next one saw the dropped lease as live and skipped the
/// session (`recovered()` None; 25 of 100 runs at load 90, none with one test thread). The
/// spawning test holds this lock for writing from the spawn until the child reports ready (it has
/// exec'd by then); every test that holds a lease holds it for reading.
static SPAWN: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// The read side of [`SPAWN`] for a test that creates sessions.
fn no_spawn() -> std::sync::RwLockReadGuard<'static, ()> {
    SPAWN.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn two_peers_hash_trace_lines_diff_to_the_first_split() {
    let _spawn = no_spawn();
    let dir = TempDir::new("peer-hashes");
    let a = PeerLog::create(&dir, "match-001", 1, SessionOptions::default()).expect("test operation");
    let b = PeerLog::create(&dir, "match-001", 2, SessionOptions::default()).expect("test operation");
    for tick in 0..12 {
        a.set_tick(tick);
        b.set_tick(tick);
        a.line(tracing::Level::INFO, "net", format_args!("step"))
            .expect("test operation");
        b.line(tracing::Level::INFO, "net", format_args!("step"))
            .expect("test operation");
        a.hash_trace("world", tick, tick * 31).expect("test operation");
        b.hash_trace("world", tick, tick * 31 + u64::from(tick >= 7))
            .expect("test operation");
    }
    a.flush().expect("test operation");
    b.flush().expect("test operation");
    let left = std::fs::read_to_string(a.directory().join("world.hash")).expect("test operation");
    let right = std::fs::read_to_string(b.directory().join("world.hash")).expect("test operation");
    assert_eq!(left.lines().count(), 12);
    assert_eq!(right.lines().count(), 12);
    let split = left.lines().zip(right.lines()).position(|(x, y)| x != y);
    assert_eq!(split, Some(7));
    assert_eq!(left.lines().nth(7), Some("7 00000000000000d9"));
    assert_ne!(a.directory(), b.directory());
    for peer in [&a, &b] {
        let lines = std::fs::read_to_string(peer.directory().join("session.log")).expect("test operation");
        assert!(lines.lines().all(|l| l.starts_with("t=")), "{lines}");
        assert!(lines.contains("t=7 "));
        peer.finish().expect("test operation");
    }
}

#[test]
#[cfg(not(feature = "devtools"))]
fn dev_env_hook_absent_in_release_build() {
    assert!(!std::hint::black_box(devenv::ENABLED));
    // A variable known to exist proves this is compiled out, not merely unset.
    let (name, _) = std::env::vars_os().next().expect("test environment");
    let name = name.to_str().expect("environment key");
    assert_eq!(devenv::var(name), Err(std::env::VarError::NotPresent));
    assert!(devenv::var_os(name).is_none());
    assert!(!devenv::is_set(name));
    assert!(!devenv::flag(name));
}

#[test]
fn an_unclean_session_retains_only_the_configured_tail_once() {
    let _spawn = no_spawn();
    let dir = TempDir::new("unclean-tail");
    let options = SessionOptions {
        tail_lines: 5,
        ..Default::default()
    };
    let old = PeerLog::create(&dir, "001", 4, options).expect("test operation");
    for tick in 0..20 {
        old.set_tick(tick);
        old.line(tracing::Level::INFO, "life", format_args!("event {tick}"))
            .expect("test operation");
    }
    old.flush().expect("test operation");
    drop(old); // Deliberately no finish.
    let next = PeerLog::create(&dir, "002", 4, options).expect("test operation");
    let found = next.recovered().expect("unclean evidence");
    assert_eq!(found.session, "001-peer4");
    assert_eq!(found.lines, 5);
    let tail = std::fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
    assert_eq!(tail.lines().count(), 5);
    assert!(tail.lines().next().expect("test operation").starts_with("t=15 "));
    assert!(tail.lines().last().expect("test operation").ends_with("event 19"));
    assert!(found.directory.join("report.txt").is_file());
    assert!(
        regia_core::log::session::check_previous(&dir, "002", 4, options)
            .expect("test operation")
            .is_none()
    );
    next.finish().expect("test operation");
    drop(next);
    let clean = PeerLog::create(&dir, "003", 4, options).expect("test operation");
    assert!(
        clean.recovered().is_none(),
        "clean newest session wins over older unclean"
    );
}

#[test]
fn active_sessions_are_skipped_and_other_peers_are_untouched() {
    let _spawn = no_spawn();
    let dir = TempDir::new("active-peers");
    let options = SessionOptions::default();
    let old = PeerLog::create(&dir, "001", 7, options).expect("test operation");
    old.flush().expect("test operation");
    drop(old);
    let other = PeerLog::create(&dir, "001", 8, options).expect("test operation");
    other.flush().expect("test operation");
    drop(other);
    let live = PeerLog::create(&dir, "003", 7, options).expect("test operation");
    assert_eq!(live.recovered().expect("test operation").session, "001-peer7");
    let next = PeerLog::create(&dir, "004", 7, options).expect("test operation");
    assert!(next.recovered().is_none());
    assert!(!dir.join("unclean-003-peer7").exists());
    assert!(!dir.join("unclean-001-peer8").exists());
}

#[test]
fn exclusive_paths_names_and_file_limits_prevent_overwrite_or_escape() {
    let _spawn = no_spawn();
    let dir = TempDir::new("peer-paths");
    let options = SessionOptions {
        max_hash_traces: 1,
        ..Default::default()
    };
    for bad in [
        "",
        "../escape",
        "a/b",
        "a\\b",
        ".",
        "a\n",
        "日本",
        "unclean-old",
        "UNCLEAN-old",
        "pending-old",
    ] {
        assert!(PeerLog::create(&dir, bad, 0, options).is_err());
    }
    let log = PeerLog::create(&dir, "safe", 0, options).expect("test operation");
    assert!(PeerLog::create(&dir, "safe", 0, options).is_err());
    assert!(log.hash_trace("../escape", 0, 0).is_err());
    log.hash_trace("world", u64::MAX, u64::MAX)
        .expect("test operation");
    assert!(log.hash_trace("second", 0, 0).is_err());
    log.hash_trace("world", 1, 0).expect("test operation");
    log.finish().expect("test operation");
    log.finish().expect("test operation");
    assert!(log.hash_trace("world", 2, 1).is_err());
    assert!(
        log.line(tracing::Level::INFO, "net", format_args!("late"))
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(log.directory().join("world.hash")).expect("test operation"),
        "18446744073709551615 ffffffffffffffff\n1 0000000000000000\n"
    );
}

#[test]
fn embedded_newlines_do_not_forge_unstamped_peer_lines() {
    let _spawn = no_spawn();
    let dir = TempDir::new("peer-newlines");
    let log = PeerLog::create(&dir, "safe", 0, SessionOptions::default()).expect("test operation");
    log.set_tick(91);
    log.line(
        tracing::Level::WARN,
        "net\nforged",
        format_args!("first\nsecond\r\t\0"),
    )
    .expect("test operation");
    log.flush().expect("test operation");
    let text = std::fs::read_to_string(log.directory().join("session.log")).expect("test operation");
    assert_eq!(text.lines().count(), 2);
    assert!(
        text.lines()
            .last()
            .expect("test operation")
            .starts_with("t=91 peer=0 WARN net\\nforged")
    );
    assert!(text.contains("first\\nsecond\\r\\t\\u{0}"));
}

#[test]
fn recovery_byte_limit_discards_the_partial_first_line() {
    let _spawn = no_spawn();
    let dir = TempDir::new("peer-byte-tail");
    let options = SessionOptions {
        tail_bytes: 65,
        tail_lines: 20,
        ..Default::default()
    };
    let old = PeerLog::create(&dir, "001", 0, options).expect("test operation");
    old.line(tracing::Level::INFO, "net", format_args!("{}", "x".repeat(100)))
        .expect("test operation");
    old.set_tick(2);
    old.line(tracing::Level::INFO, "net", format_args!("last"))
        .expect("test operation");
    old.flush().expect("test operation");
    drop(old);
    let next = PeerLog::create(&dir, "002", 0, options).expect("test operation");
    let found = next.recovered().expect("test operation");
    let text = std::fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
    assert_eq!(text, "t=2 peer=0 INFO net last\n");
}

#[test]
fn every_physical_tracing_line_and_ring_event_carries_the_tick() {
    let _spawn = no_spawn();
    use tracing_subscriber::layer::SubscriberExt;
    let dir = TempDir::new("tick-formatter");
    let sink = regia_core::log::FileSink::create(&dir, 1).expect("test operation");
    let subscriber = tracing_subscriber::registry()
        .with(regia_core::log::ring::RingLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .event_format(regia_core::log::TickFormat(
                    tracing_subscriber::fmt::format().with_ansi(false),
                ))
                .with_writer(sink.clone()),
        );
    regia_core::log::set_tick(123);
    tracing::subscriber::with_default(subscriber, || {
        let _span = tracing::info_span!("test_tick_span", number = 7).entered();
        tracing::info!(target: "tick_format_acceptance", "first\nsecond");
    });
    sink.flush();
    let text = std::fs::read_to_string(sink.path()).expect("test operation");
    assert_eq!(text.lines().count(), 2);
    assert!(
        !text.contains('\u{1b}'),
        "file span fields must stay ANSI-free: {text}"
    );
    assert!(text.lines().all(|l| l.starts_with("t=123 ")), "{text}");
    let (records, _) = regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        target: Some("tick_format_acceptance".into()),
        ..Default::default()
    });
    assert_eq!(records.len(), 1);
    assert!(records[0].message.starts_with("t=123 "));
}

#[test]
#[cfg(feature = "devtools")]
fn explicitly_enabled_development_build_reads_the_environment() {
    assert!(std::hint::black_box(devenv::ENABLED));
    let (name, value) = std::env::vars_os().next().expect("test environment");
    let name = name.to_str().expect("environment key");
    assert_eq!(devenv::var_os(name), Some(value));
    assert_eq!(devenv::var(name), std::env::var(name));
    assert!(devenv::is_set(name));
    assert_eq!(
        devenv::flag(name),
        std::env::var(name).ok().as_deref() == Some("1")
    );
}

#[test]
fn lease_child() {
    use std::io::{Read, Write};
    let Some(root) = std::env::var_os("REGIA_LOG_LEASE_CHILD_ROOT") else {
        return;
    };
    let peer = PeerLog::create(std::path::Path::new(&root), "001", 3, SessionOptions::default())
        .expect("test operation");
    peer.line(tracing::Level::INFO, "net", format_args!("child alive"))
        .expect("test operation");
    peer.flush().expect("test operation");
    println!("LEASE_READY");
    std::io::stdout().flush().expect("test operation");
    std::io::stdin().read_exact(&mut [0]).expect("test operation");
    // Model a hard exit, skipping Drop and any clean marker. Files were explicitly flushed.
    std::process::exit(9);
}

#[test]
fn a_live_child_lease_is_skipped_then_a_hard_exit_is_recovered() {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = TempDir::new("peer-child-lease");
    let spawning = SPAWN.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().expect("test operation"))
            .args(["--exact", "lease_child", "--nocapture"])
            .env("REGIA_LOG_LEASE_CHILD_ROOT", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("test operation"),
    );
    let output = child.0.stdout.take().expect("test operation");
    let mut ready = false;
    for line in std::io::BufReader::new(output).lines() {
        if line.expect("test operation").contains("LEASE_READY") {
            ready = true;
            break;
        }
    }
    assert!(ready, "child acquired and flushed its session");
    drop(spawning);
    assert!(
        regia_core::log::session::check_previous(&root, "002", 3, SessionOptions::default())
            .expect("test operation")
            .is_none()
    );
    child
        .0
        .stdin
        .take()
        .expect("test operation")
        .write_all(b"x")
        .expect("test operation");
    assert_eq!(child.0.wait().expect("test operation").code(), Some(9));
    let found = regia_core::log::session::check_previous(&root, "002", 3, SessionOptions::default())
        .expect("test operation")
        .expect("test operation");
    assert_eq!(found.session, "001-peer3");
    let tail = std::fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
    assert!(tail.contains("child alive"));
}

#[test]
fn a_checkpoint_open_failure_preserves_existing_evidence_and_refuses_clean() {
    let _spawn = no_spawn();
    let root = TempDir::new("peer-open-failure");
    let log = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
    let existing = log.directory().join("world.hash");
    regia_test_utils::write_atomic(&existing, b"existing evidence\n").expect("test operation");
    assert!(log.hash_trace("world", 1, 0).is_err());
    assert_eq!(
        std::fs::read_to_string(existing).expect("test operation"),
        "existing evidence\n"
    );
    assert!(log.finish().is_err());
    assert!(!log.directory().join("clean").exists());
}

#[test]
fn a_message_that_mentions_clean_exit_does_not_mark_the_session_clean() {
    let _spawn = no_spawn();
    let root = TempDir::new("peer-false-clean");
    let log = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("create peer");
    log.line(tracing::Level::INFO, "chat", format_args!("session end clean=1"))
        .expect("write message");
    drop(log);
    let next = PeerLog::create(&root, "002", 0, SessionOptions::default()).expect("next peer");
    assert!(next.recovered().is_some());
}

#[test]
fn an_interrupted_creation_is_skipped_until_its_locked_log_is_ready() {
    let _spawn = no_spawn();
    let root = TempDir::new("peer-incomplete-create");
    let incomplete = root.join("001-peer3");
    std::fs::create_dir(&incomplete).expect("incomplete session directory");
    regia_test_utils::write_atomic(&incomplete.join("active.lock"), b"").expect("lease not acquired yet");
    let next = PeerLog::create(&root, "002", 3, SessionOptions::default()).expect("new launch succeeds");
    assert!(next.recovered().is_none());
    assert!(!root.join("unclean-001-peer3").exists());
    assert!(next.directory().join("ready").exists());
}
