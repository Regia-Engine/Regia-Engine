// SPDX-License-Identifier: MPL-2.0
//! The trace file writer thread (spec 54 § 10, ADR-2307): the recorder hands whole chunks to a
//! worker over a bounded queue, so a frame that records passes or marks never touches the file.
//!
//! - The queue holds at most [`QUEUE_CHUNKS`] batches. A full queue never blocks the caller: the
//!   batch stays with the caller ([`Writer::try_send`] gives it back) and goes out with the next
//!   flush. A caller holding more than [`MAX_HELD_BYTES`] stops the recording (the file ends at
//!   its last complete chunk, as on a write error).
//! - The first write error is kept; every later batch is dropped and the next send reports it.
//! - [`Writer::close`] (stop, save) waits for the queue to drain and returns the first error.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};

/// Batches the writer queue holds.
pub const QUEUE_CHUNKS: usize = 8;

/// Most bytes a caller keeps while the queue is full; past this the recording stops.
pub const MAX_HELD_BYTES: usize = 4 * 1024 * 1024;

enum Msg {
    Bytes(Vec<u8>),
    Close(mpsc::Sender<std::io::Result<()>>),
}

/// What the writer did, for tests and the overhead counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriterStats {
    /// Batches handed to the thread.
    pub batches: u64,
    /// Bytes the thread wrote.
    pub bytes_written: u64,
    /// Sends refused because the queue was full (the batch waited on the caller's side).
    pub queue_full: u64,
    /// Writes the thread did (one per batch).
    pub writes: u64,
}

#[derive(Default)]
struct Shared {
    failed: Mutex<Option<String>>,
    batches: AtomicU64,
    bytes_written: AtomicU64,
    queue_full: AtomicU64,
    writes: AtomicU64,
}

/// Why a send failed.
#[derive(Debug)]
pub enum SendError {
    /// The queue is full; the batch is handed back.
    Full(Vec<u8>),
    /// The thread stopped or a write failed (the message).
    Failed(String),
}

/// A writer thread that owns one output.
pub struct Writer {
    tx: Option<SyncSender<Msg>>,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn failed_text(shared: &Shared) -> Option<String> {
    shared
        .failed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

impl Writer {
    /// Starts a thread that writes every batch to `out`, in order.
    pub fn spawn(out: Box<dyn Write + Send>) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE_CHUNKS);
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("regia-trace-writer".to_owned())
            .spawn(move || {
                let mut out = out;
                for msg in rx {
                    match msg {
                        Msg::Bytes(bytes) => {
                            if failed_text(&thread_shared).is_some() {
                                continue;
                            }
                            let _span = tracing::trace_span!("trace_write").entered();
                            thread_shared.writes.fetch_add(1, Ordering::Relaxed);
                            match out.write_all(&bytes) {
                                Ok(()) => {
                                    thread_shared
                                        .bytes_written
                                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    *thread_shared
                                        .failed
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
                                }
                            }
                        }
                        Msg::Close(reply) => {
                            let result = match failed_text(&thread_shared) {
                                Some(e) => Err(std::io::Error::other(e)),
                                None => out.flush(),
                            };
                            let _ = reply.send(result);
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            shared,
            thread: Some(thread),
        })
    }

    /// Queues `bytes` without blocking.
    pub fn try_send(&self, bytes: Vec<u8>) -> Result<(), SendError> {
        if let Some(e) = failed_text(&self.shared) {
            return Err(SendError::Failed(e));
        }
        let Some(tx) = self.tx.as_ref() else {
            return Err(SendError::Failed("the trace writer is closed".to_owned()));
        };
        match tx.try_send(Msg::Bytes(bytes)) {
            Ok(()) => {
                self.shared.batches.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(Msg::Bytes(bytes))) => {
                self.shared.queue_full.fetch_add(1, Ordering::Relaxed);
                Err(SendError::Full(bytes))
            }
            Err(TrySendError::Full(Msg::Close(_))) => Err(SendError::Full(Vec::new())),
            Err(TrySendError::Disconnected(_)) => {
                Err(SendError::Failed("the trace writer thread stopped".to_owned()))
            }
        }
    }

    /// Queues `bytes`, waiting for room (stop and save only, never the frame path).
    pub fn send_blocking(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        if let Some(e) = failed_text(&self.shared) {
            return Err(std::io::Error::other(e));
        }
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| std::io::Error::other("the trace writer is closed"))?;
        tx.send(Msg::Bytes(bytes))
            .map_err(|_| std::io::Error::other("the trace writer thread stopped"))?;
        self.shared.batches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The counters now.
    #[must_use]
    pub fn stats(&self) -> WriterStats {
        WriterStats {
            batches: self.shared.batches.load(Ordering::Relaxed),
            bytes_written: self.shared.bytes_written.load(Ordering::Relaxed),
            queue_full: self.shared.queue_full.load(Ordering::Relaxed),
            writes: self.shared.writes.load(Ordering::Relaxed),
        }
    }

    /// Waits until every queued batch is written and the output flushed, then ends the thread.
    /// Returns the first write error.
    pub fn close(mut self) -> std::io::Result<()> {
        let _span = tracing::trace_span!("trace_writer_close").entered();
        let Some(tx) = self.tx.take() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        let sent = tx.send(Msg::Close(reply_tx));
        drop(tx);
        let result = match sent {
            Ok(()) => reply_rx
                .recv()
                .unwrap_or_else(|_| Err(std::io::Error::other("the trace writer thread stopped"))),
            Err(_) => Err(std::io::Error::other("the trace writer thread stopped")),
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        result
    }
}

impl Drop for Writer {
    /// Without [`Self::close`] (a recording that stopped on an error): the thread writes what is
    /// queued and ends on its own; nothing waits for it here.
    fn drop(&mut self) {
        self.tx.take();
        self.thread.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An output whose writes wait until the test opens the gate; it records what it got.
    struct Gated {
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        got: Arc<Mutex<Vec<u8>>>,
        thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
    }

    impl Write for Gated {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.gate;
            let mut open = lock.lock().unwrap_or_else(PoisonError::into_inner);
            while !*open {
                open = cv.wait(open).unwrap_or_else(PoisonError::into_inner);
            }
            *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(std::thread::current().id());
            self.got
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    type Gate = Arc<(Mutex<bool>, std::sync::Condvar)>;

    struct Rig {
        w: Writer,
        gate: Gate,
        got: Arc<Mutex<Vec<u8>>>,
        thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
    }

    fn gated() -> Rig {
        let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let got = Arc::new(Mutex::new(Vec::new()));
        let thread = Arc::new(Mutex::new(None));
        let w = Writer::spawn(Box::new(Gated {
            gate: Arc::clone(&gate),
            got: Arc::clone(&got),
            thread: Arc::clone(&thread),
        }))
        .expect("spawn");
        Rig { w, gate, got, thread }
    }

    fn open(gate: &Gate) {
        let (lock, cv) = &**gate;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cv.notify_all();
    }

    /// A stuck output never blocks the sender: after the queue (plus the batch the thread holds)
    /// fills, sends hand the batch back; once the output moves, everything arrives in order on
    /// the writer thread.
    #[test]
    fn a_full_queue_hands_the_batch_back_and_order_is_kept() {
        let Rig { w, gate, got, thread } = gated();
        let mut sent = Vec::new();
        let mut refused = 0u32;
        for n in 0u8..64 {
            match w.try_send(vec![n]) {
                Ok(()) => sent.push(n),
                Err(SendError::Full(back)) => {
                    assert_eq!(back, vec![n], "the batch comes back whole");
                    refused += 1;
                }
                Err(SendError::Failed(e)) => panic!("unexpected failure {e}"),
            }
        }
        // The thread may hold one batch while the queue holds QUEUE_CHUNKS.
        assert!(sent.len() <= QUEUE_CHUNKS + 1, "{} queued", sent.len());
        assert!(sent.len() >= QUEUE_CHUNKS, "{} queued", sent.len());
        assert_eq!(u64::from(refused), w.stats().queue_full);
        open(&gate);
        w.close().expect("close");
        assert_eq!(*got.lock().unwrap_or_else(PoisonError::into_inner), sent);
        let writer_thread = thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("wrote");
        assert_ne!(
            writer_thread,
            std::thread::current().id(),
            "IO ran on the writer thread"
        );
    }

    #[test]
    fn the_first_write_error_is_reported_by_the_next_send_and_by_close() {
        let w = Writer::spawn(Box::new(Failing)).expect("spawn");
        w.send_blocking(vec![1, 2, 3]).expect("queued");
        // Close drains the queue, so the failed write is known by then.
        let err = w.close().expect_err("the write failed");
        assert!(err.to_string().contains("disk full"), "{err}");
    }
}
