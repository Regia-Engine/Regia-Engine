// SPDX-License-Identifier: MPL-2.0
//! Opt-in ordered peer writer (ADR-8493, audit CT-28).
//!
//! Synchronization uses channels and barriers. A stalled sink is held until the test
//! releases it; assertions do not read the clock.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use regia_core::log::session::{AsyncWriterBounds, PeerLog, SessionOptions};
use regia_test_utils::TempDir;

struct StallInner {
    entered: Mutex<Option<Sender<()>>>,
    release: Mutex<Option<Receiver<()>>>,
    buf: Mutex<Vec<u8>>,
    armed: AtomicBool,
}

struct StallSink {
    inner: Arc<StallInner>,
}

impl Write for StallSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if !self.inner.armed.swap(true, Ordering::SeqCst) {
            if let Some(entered) = self
                .inner
                .entered
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .take()
            {
                let _ = entered.send(());
            }
            if let Some(release) = self
                .inner
                .release
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .take()
            {
                let _ = release.recv();
            }
        }
        self.inner
            .buf
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Blocks the first write until released, then fails every write. The producer can enqueue
/// while the worker is stuck; the failure is observed at flush.
struct FailSink {
    entered: Option<Sender<()>>,
    release: Option<Receiver<()>>,
    armed: bool,
}

impl Write for FailSink {
    fn write(&mut self, _data: &[u8]) -> io::Result<usize> {
        if !self.armed {
            self.armed = true;
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.recv();
            }
        }
        Err(io::Error::other("injected write failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounds(max_records: usize, max_queued_bytes: usize, max_message_bytes: usize) -> SessionOptions {
    SessionOptions::default().with_async_writer(AsyncWriterBounds {
        max_records,
        max_queued_bytes,
        max_message_bytes,
    })
}

fn stall_pair() -> (StallSink, Sender<()>, Receiver<()>, Arc<StallInner>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let inner = Arc::new(StallInner {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        buf: Mutex::new(Vec::new()),
        armed: AtomicBool::new(false),
    });
    let sink = StallSink {
        inner: Arc::clone(&inner),
    };
    (sink, release_tx, entered_rx, inner)
}

fn sink_text(inner: &StallInner) -> String {
    let bytes = inner.buf.lock().unwrap_or_else(|err| err.into_inner()).clone();
    String::from_utf8(bytes).expect("utf8")
}

#[test]
fn a_stalled_sink_does_not_block_the_producer_and_finish_drains_in_order() {
    let root = TempDir::new("peer-async-stall");
    let (sink, release_tx, entered_rx, stalled) = stall_pair();
    let log = PeerLog::create_with_log_sink(&root, "001", 1, bounds(8, 64 * 1024, 4 * 1024), Box::new(sink))
        .expect("create");
    // Create already accepted the session-begin line. A second line must return while the
    // worker is still inside the stalled write: the caller never waits for the sink.
    log.line_at(4, tracing::Level::INFO, "net", format_args!("alpha"))
        .expect("enqueue alpha");
    log.line_at(5, tracing::Level::INFO, "net", format_args!("beta"))
        .expect("enqueue beta");
    let queued = log.diagnostics();
    assert!(queued.queued_records >= 1);
    assert!(queued.queued_bytes <= 64 * 1024);
    assert_eq!(queued.dropped_lines, 0);
    entered_rx.recv().expect("worker blocked in the sink");
    assert!(!log.directory().join("clean").exists());

    let (done_tx, done_rx) = mpsc::channel();
    let log = Arc::new(log);
    let finishing = Arc::clone(&log);
    thread::spawn(move || {
        let result = finishing.finish();
        let _ = done_tx.send(result);
    });
    assert!(
        done_rx.try_recv().is_err(),
        "finish published before the stalled worker drained"
    );
    assert!(!log.directory().join("clean").exists());
    release_tx.send(()).expect("release sink");
    done_rx.recv().expect("finish returned").expect("clean finish");
    assert!(log.directory().join("clean").exists());
    let text = sink_text(&stalled);
    let lines: Vec<_> = text.lines().collect();
    assert!(lines[0].contains("session begin"), "{lines:?}");
    let alpha = lines
        .iter()
        .position(|line| line.contains("alpha"))
        .expect("alpha");
    let beta = lines.iter().position(|line| line.contains("beta")).expect("beta");
    assert!(alpha < beta, "{lines:?}");
    assert!(lines[alpha].starts_with("t=4 "), "{lines:?}");
    assert!(lines[beta].starts_with("t=5 "), "{lines:?}");
    let after = log.diagnostics();
    assert_eq!(after.queued_records, 0);
    assert_eq!(after.queued_bytes, 0);
    assert!(after.complete);
    assert!(!after.incomplete);
    assert_eq!(after.errors, 0);
    assert_eq!(after.written_records, 3);
}

#[test]
fn byte_and_message_caps_hold_and_checkpoint_loss_refuses_clean() {
    let root = TempDir::new("peer-async-caps");
    let (sink, release_tx, entered_rx, stalled) = stall_pair();
    // One charged record is the session-begin line, so the queue is full after create.
    let log =
        PeerLog::create_with_log_sink(&root, "001", 3, bounds(1, 96, 96), Box::new(sink)).expect("create");
    entered_rx.recv().expect("begin line reached the stalled sink");
    let charged = log.diagnostics();
    assert_eq!(charged.queued_records, 1);
    assert!(charged.queued_bytes <= 96, "{}", charged.queued_bytes);
    assert!(charged.queued_bytes > 0);

    log.line(tracing::Level::INFO, "net", format_args!("fits"))
        .expect("human overflow is not a caller error");
    let long = "x".repeat(200);
    log.line(tracing::Level::INFO, "net", format_args!("{long}"))
        .expect("oversize human line is dropped");
    let overflow = log.diagnostics();
    assert_eq!(overflow.queued_records, 1, "the record cap stayed full");
    assert!(overflow.queued_bytes <= 96);
    assert_eq!(overflow.dropped_lines, 2);
    assert!(overflow.dropped_bytes > 0);
    assert_eq!(overflow.oversize_lines, 1);
    assert!(!overflow.incomplete);

    let checkpoint = log.hash_trace("world", 9, 0x00ab);
    assert!(checkpoint.is_err(), "a checkpoint that does not fit must fail");
    let lost = log.diagnostics();
    assert!(lost.incomplete);
    assert!(lost.errors >= 1);
    assert_eq!(lost.queued_records, 1);

    release_tx.send(()).expect("release");
    assert!(log.finish().is_err());
    assert!(!log.directory().join("clean").exists());
    assert!(!log.diagnostics().complete);
    assert!(log.diagnostics().incomplete);
    let text = sink_text(&stalled);
    assert!(!text.contains("fits"), "{text}");
    assert!(!text.contains(&long), "{text}");
    assert!(!log.directory().join("world.hash").exists());
    drop(log);
    let next = PeerLog::create(&root, "002", 3, SessionOptions::default()).expect("recover");
    let found = next.recovered().expect("checkpoint loss is unclean evidence");
    assert_eq!(found.session, "001-peer3");
}

#[test]
fn per_peer_tick_and_sequence_survive_concurrent_producers() {
    let root = TempDir::new("peer-async-race");
    let options = bounds(64, 64 * 1024, 4 * 1024);
    let left = PeerLog::create(&root, "match", 1, options).expect("peer 1");
    let right = PeerLog::create(&root, "match", 2, options).expect("peer 2");
    let barrier = Arc::new(Barrier::new(4));
    let mut receipts = Vec::new();
    thread::scope(|scope| {
        for (log, tick, hash, label) in [
            (&left, 11u64, 0x11u64, "alpha"),
            (&left, 12, 0x12, "beta"),
            (&right, 21, 0x21, "gamma"),
            (&right, 22, 0x22, "delta"),
        ] {
            let barrier = Arc::clone(&barrier);
            let (tx, rx) = mpsc::channel();
            scope.spawn(move || {
                barrier.wait();
                log.line_at(tick, tracing::Level::INFO, "net", format_args!("{label}"))
                    .expect("line");
                let line = log.last_enqueue().expect("line receipt");
                log.hash_trace(label, tick, hash).expect("hash");
                let hash_enq = log.last_enqueue().expect("hash receipt");
                tx.send((line, hash_enq)).expect("receipt");
            });
            receipts.push(rx);
        }
    });
    let left_a = receipts[0].recv().expect("left a");
    let left_b = receipts[1].recv().expect("left b");
    let right_a = receipts[2].recv().expect("right a");
    let right_b = receipts[3].recv().expect("right b");
    assert_eq!(left_a.0.tick, 11);
    assert_eq!(left_b.0.tick, 12);
    assert_eq!(right_a.0.tick, 21);
    assert_eq!(right_b.0.tick, 22);
    assert_ne!(left_a.0.sequence, left_b.0.sequence);
    assert_ne!(right_a.0.sequence, right_b.0.sequence);
    assert!(left_a.1.sequence > left_a.0.sequence);
    assert!(right_a.1.sequence > right_a.0.sequence);
    // Sequences are per peer. Both sessions start at 1 for the begin line.
    assert!(left.diagnostics().next_sequence > 1);
    assert!(right.diagnostics().next_sequence > 1);

    left.finish().expect("finish left");
    right.finish().expect("finish right");
    let left_log = std::fs::read_to_string(left.directory().join("session.log")).expect("left log");
    let right_log = std::fs::read_to_string(right.directory().join("session.log")).expect("right log");
    assert!(left_log.contains("t=11 peer=1 INFO net alpha\n"), "{left_log}");
    assert!(left_log.contains("t=12 peer=1 INFO net beta\n"), "{left_log}");
    assert!(right_log.contains("t=21 peer=2 INFO net gamma\n"), "{right_log}");
    assert!(right_log.contains("t=22 peer=2 INFO net delta\n"), "{right_log}");
    let alpha = std::fs::read_to_string(left.directory().join("alpha.hash")).expect("alpha hash");
    let beta = std::fs::read_to_string(left.directory().join("beta.hash")).expect("beta hash");
    let gamma = std::fs::read_to_string(right.directory().join("gamma.hash")).expect("gamma hash");
    let delta = std::fs::read_to_string(right.directory().join("delta.hash")).expect("delta hash");
    assert_eq!(alpha, "11 0000000000000011\n");
    assert_eq!(beta, "12 0000000000000012\n");
    assert_eq!(gamma, "21 0000000000000021\n");
    assert_eq!(delta, "22 0000000000000022\n");
    let left_alpha_at = left_log.find("name=alpha").expect("alpha log");
    let left_beta_at = left_log.find("name=beta").expect("beta log");
    if left_a.1.sequence < left_b.1.sequence {
        assert!(left_alpha_at < left_beta_at, "{left_log}");
    } else {
        assert!(left_beta_at < left_alpha_at, "{left_log}");
    }
}

#[test]
fn write_failure_leaves_unclean_evidence() {
    let root = TempDir::new("peer-async-fail");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let log = PeerLog::create_with_log_sink(
        &root,
        "001",
        4,
        bounds(8, 8 * 1024, 2 * 1024),
        Box::new(FailSink {
            entered: Some(entered_tx),
            release: Some(release_rx),
            armed: false,
        }),
    )
    .expect("create");
    log.line_at(
        3,
        tracing::Level::ERROR,
        "net",
        format_args!("will fail on the worker"),
    )
    .expect("enqueue does not wait for the sink");
    entered_rx
        .recv()
        .expect("worker blocked before the failing write");
    release_tx.send(()).expect("release");
    assert!(log.flush().is_err());
    assert!(log.finish().is_err());
    assert!(!log.directory().join("clean").exists());
    let diag = log.diagnostics();
    assert!(diag.errors >= 1);
    assert!(diag.incomplete);
    assert!(!diag.complete);
    assert_eq!(diag.queued_records, 0);
    drop(log);
    let next = PeerLog::create(&root, "002", 4, SessionOptions::default()).expect("next");
    assert!(next.recovered().is_some());
}

#[test]
fn async_hash_bytes_match_the_synchronous_contract() {
    let root = TempDir::new("peer-async-hash");
    let options = bounds(32, 32 * 1024, 2 * 1024);
    let peer_a = PeerLog::create(&root, "match-001", 1, options).expect("peer a");
    let peer_b = PeerLog::create(&root, "match-001", 2, options).expect("peer b");
    for tick in 0..12 {
        peer_a.set_tick(tick);
        peer_b.set_tick(tick);
        peer_a
            .line(tracing::Level::INFO, "net", format_args!("step"))
            .expect("line a");
        peer_b
            .line(tracing::Level::INFO, "net", format_args!("step"))
            .expect("line b");
        peer_a.hash_trace("world", tick, tick * 31).expect("hash a");
        peer_b
            .hash_trace("world", tick, tick * 31 + u64::from(tick >= 7))
            .expect("hash b");
    }
    peer_a.finish().expect("finish a");
    peer_b.finish().expect("finish b");
    let left = std::fs::read_to_string(peer_a.directory().join("world.hash")).expect("left");
    let right = std::fs::read_to_string(peer_b.directory().join("world.hash")).expect("right");
    assert_eq!(left.lines().count(), 12);
    assert_eq!(right.lines().count(), 12);
    let split = left.lines().zip(right.lines()).position(|(x, y)| x != y);
    assert_eq!(split, Some(7));
    assert_eq!(left.lines().nth(7), Some("7 00000000000000d9"));
    assert_ne!(peer_a.directory(), peer_b.directory());
    let session = std::fs::read_to_string(peer_a.directory().join("session.log")).expect("session");
    assert!(session.lines().all(|line| line.starts_with("t=")), "{session}");
    assert!(session.contains("t=7 "));
    let diag = peer_a.publish_diagnostics();
    assert!(diag.complete);
    assert_eq!(diag.dropped_lines, 0);
    assert_eq!(diag.errors, 0);
    assert_eq!(diag.queued_bytes, 0);
}

#[test]
fn an_async_checkpoint_write_failure_preserves_bytes_and_refuses_clean() {
    let root = TempDir::new("peer-async-open");
    let log = PeerLog::create(&root, "001", 0, bounds(8, 8 * 1024, 2 * 1024)).expect("create");
    let existing = log.directory().join("world.hash");
    regia_test_utils::write_atomic(&existing, b"existing evidence\n").expect("plant");
    log.hash_trace("world", 1, 0).expect("enqueue open");
    assert!(log.flush().is_err());
    assert_eq!(
        std::fs::read_to_string(&existing).expect("read"),
        "existing evidence\n"
    );
    assert!(log.finish().is_err());
    assert!(!log.directory().join("clean").exists());
    assert!(log.diagnostics().incomplete);
}
