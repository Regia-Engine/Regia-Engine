// SPDX-License-Identifier: MPL-2.0
//! Non-blocking console writer: formatted log lines go through a bounded queue to a writer
//! thread, and are dropped (and counted) when the queue is full.
//!
//! Used for stderr when it is not a terminal. A launcher that pipes stderr and never reads it
//! fills the pipe (64 KiB on Linux) after a few hundred lines; a synchronous write then blocks
//! the logging thread — the frame thread — forever. With this writer only the writer thread
//! blocks; the game keeps running and loses console lines instead (the file sink, the in-memory
//! ring and telemetry still get every record).

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;

/// Lines the console queue holds before new lines are dropped.
pub const CONSOLE_QUEUE_LINES: usize = 1024;

struct Shared {
    /// Lines dropped because the queue was full (the sink is not keeping up or is stuck).
    dropped: AtomicU64,
    /// Lines offered to the queue: counted before the line enters it, so a flush on another
    /// thread never misses a line that is already queued (ADR-2396). Every offered line ends up
    /// either `written` or `dropped`.
    queued: AtomicU64,
    /// Lines the writer thread has handed to the sink.
    written: AtomicU64,
    /// Whether the last line the writer thread handled reached the sink (the sink is closed or
    /// broken when it did not).
    last_ok: AtomicBool,
    /// Test hook run right after a line entered the channel, before `send` returns.
    #[cfg(test)]
    after_send: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// A bounded, lossy, non-blocking line writer in front of a [`Write`] sink. Cloning shares the
/// queue and the writer thread.
///
/// Logging through it never blocks: a full queue drops the line and counts it
/// ([`NonBlockingConsole::dropped`]); the writer thread writes a one-line note with the number
/// of dropped lines once the sink accepts output again. The thread is detached: it keeps no
/// lock anyone else needs and does not keep the process alive.
#[derive(Clone)]
pub struct NonBlockingConsole {
    tx: SyncSender<Vec<u8>>,
    shared: Arc<Shared>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            dropped: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            written: AtomicU64::new(0),
            last_ok: AtomicBool::new(true),
            #[cfg(test)]
            after_send: std::sync::Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for NonBlockingConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NonBlockingConsole")
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

impl NonBlockingConsole {
    /// Starts the writer thread for `sink` with a queue of `capacity` lines (at least 1).
    pub fn spawn<W: Write + Send + 'static>(sink: W, capacity: usize) -> io::Result<Self> {
        let (tx, rx) = sync_channel::<Vec<u8>>(capacity.max(1));
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("regia-log-console".into())
            .spawn(move || {
                let mut sink = sink;
                let mut reported = 0u64;
                // Ends when every sender is gone; the global subscriber keeps one for the
                // whole process, so in practice the thread ends with the process.
                while let Ok(line) = rx.recv() {
                    let dropped = thread_shared.dropped.load(Ordering::Relaxed);
                    if dropped > reported {
                        // A note that fails has nowhere to be reported; it is retried (with the
                        // larger count) on the next line.
                        let noted = writeln!(
                            sink,
                            "[regia] {} console log line(s) dropped: stderr is not being read fast enough",
                            dropped - reported
                        );
                        if noted.is_ok() {
                            reported = dropped;
                        }
                    }
                    let ok = sink.write_all(&line).is_ok() && sink.flush().is_ok();
                    thread_shared.last_ok.store(ok, Ordering::Relaxed);
                    thread_shared.written.fetch_add(1, Ordering::Release);
                }
            })?;
        Ok(Self { tx, shared })
    }

    /// Lines dropped so far because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Waits up to `timeout` until every line queued before this call has been written to the
    /// sink. Returns `false` on timeout (the sink is stuck) or when the last write failed (the
    /// sink is closed or broken); never waits longer than `timeout`.
    pub fn flush_timeout(&self, timeout: Duration) -> bool {
        let target = self.shared.queued.load(Ordering::Acquire);
        // `Duration::MAX` ("wait as long as it takes") must not overflow the deadline.
        let deadline = Instant::now().checked_add(timeout);
        let settled =
            || self.shared.written.load(Ordering::Acquire) + self.shared.dropped.load(Ordering::Acquire);
        while settled() < target {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.shared.last_ok.load(Ordering::Relaxed)
    }

    fn send(&self, line: Vec<u8>) {
        // Count first: a line is in the channel only after it is counted.
        self.shared.queued.fetch_add(1, Ordering::Release);
        match self.tx.try_send(line) {
            Ok(()) => {
                #[cfg(test)]
                self.test_after_send();
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
impl NonBlockingConsole {
    fn test_after_send(&self) {
        let hook = self.shared.after_send.lock().ok().and_then(|h| h.clone());
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Writer for one log event: collects the formatted bytes and queues them as one line on drop.
pub struct ConsoleLine<'a> {
    console: &'a NonBlockingConsole,
    buf: Vec<u8>,
}

impl Write for ConsoleLine<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ConsoleLine<'_> {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            self.console.send(std::mem::take(&mut self.buf));
        }
    }
}

impl<'a> MakeWriter<'a> for NonBlockingConsole {
    type Writer = ConsoleLine<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        ConsoleLine {
            console: self,
            buf: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};

    /// A sink whose writes block until released, recording what it got afterwards.
    #[derive(Clone, Default)]
    struct Stuck {
        gate: Arc<(Mutex<bool>, Condvar)>,
        got: Arc<Mutex<Vec<u8>>>,
    }

    impl Stuck {
        fn release(&self) {
            let (open, cv) = &*self.gate;
            *open.lock().expect("gate") = true;
            cv.notify_all();
        }
    }

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (open, cv) = &*self.gate;
            let mut open = open.lock().expect("gate");
            while !*open {
                open = cv.wait(open).expect("gate");
            }
            self.got.lock().expect("got").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Audit 11-03 core M1 (ADR-2396): a line that is already in the queue when another thread
    /// flushes counts as queued before the flush, so the flush waits for it (here: times out
    /// against the stuck sink) instead of reporting it delivered.
    #[test]
    fn flush_waits_for_a_line_whose_send_is_still_returning() {
        let sink = Stuck::default();
        let console = NonBlockingConsole::spawn(sink.clone(), 8).expect("writer thread");
        let (in_queue_tx, in_queue_rx) = std::sync::mpsc::channel::<()>();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let in_queue_tx = Mutex::new(in_queue_tx);
        *console.shared.after_send.lock().expect("hook") = Some(Arc::new(move || {
            let _ = in_queue_tx.lock().expect("tx").send(());
            let _ = resume_rx.lock().expect("rx").recv();
        }));
        let sender = console.clone();
        let logger = std::thread::spawn(move || {
            let mut w = sender.make_writer();
            w.write_all(b"pending\n").expect("buffer");
            drop(w);
        });
        in_queue_rx.recv().expect("the line entered the queue");
        assert!(
            !console.flush_timeout(Duration::from_millis(50)),
            "the queued line has not reached the stuck sink"
        );
        *console.shared.after_send.lock().expect("hook") = None;
        resume_tx.send(()).expect("resume");
        logger.join().expect("logger");
        sink.release();
        assert!(console.flush_timeout(Duration::from_secs(10)));
        let got = String::from_utf8(sink.got.lock().expect("got").clone()).expect("utf8");
        assert_eq!(got, "pending\n");
    }

    /// A sink whose every write fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A closed sink is not reported as flushed: the panic hook's flush must not claim the
    /// backtrace was delivered when the pipe is gone.
    #[test]
    fn flush_reports_a_failed_sink() {
        let console = NonBlockingConsole::spawn(Broken, 4).expect("writer thread");
        let mut w = console.make_writer();
        w.write_all(b"line\n").expect("buffer");
        drop(w);
        assert!(!console.flush_timeout(Duration::from_secs(10)));
    }

    /// A sink that never drains (like an unread stderr pipe) does not block logging: events
    /// keep flowing through a real `fmt` layer, the overflow is counted, and once the sink
    /// drains again the queued lines and a drop note arrive.
    #[test]
    fn undrained_sink_never_blocks_logging() {
        use tracing_subscriber::layer::SubscriberExt;

        let sink = Stuck::default();
        let console = NonBlockingConsole::spawn(sink.clone(), 16).expect("writer thread");
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(console.clone()),
        );
        let start = Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..20_000 {
                tracing::info!("line {i} {}", "x".repeat(64));
            }
        });
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "logging blocked: {:?}",
            start.elapsed()
        );
        // At most the queue plus the one line the writer thread holds got through.
        assert!(console.dropped() >= 20_000 - 17, "dropped {}", console.dropped());
        assert!(
            !console.flush_timeout(Duration::from_millis(20)),
            "flush is bounded"
        );

        sink.release();
        assert!(
            console.flush_timeout(Duration::from_secs(10)),
            "drains once released"
        );
        // The next line after the stall carries the drop note.
        let mut w = console.make_writer();
        w.write_all(b"after\n").expect("buffer");
        drop(w);
        assert!(console.flush_timeout(Duration::from_secs(10)));
        let got = String::from_utf8(sink.got.lock().expect("got").clone()).expect("utf8");
        assert!(got.contains("line 0 "), "{got}");
        assert!(got.contains("console log line(s) dropped"), "{got}");
        assert!(got.ends_with("after\n"), "{got}");
    }
}
