// SPDX-License-Identifier: MPL-2.0
//! The decode ring (specs/50 § 2.4 and § 10, ADR-2044): a session on its own thread decoding into a
//! ring of pictures bounded by a frame count **and** a byte budget. The worker decodes ahead until
//! the ring is full and then waits; the main thread takes pictures without blocking (or blocks in
//! `Lockstep`). A seek bumps the ring's generation, drops queued pictures and restarts the worker
//! at the cue point, so pictures decoded before a seek are never shown.
//!
//! The budget is checked before each decode with the size of the last picture as the estimate (a
//! clip keeps one size), so the queued bytes stay at or under the budget; a single picture larger
//! than the budget is still allowed alone, so a tiny budget slows decoding but never deadlocks.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::session::{FeedPoll, FeedStats, FrameFeed, Session};

/// Ring limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingBudget {
    /// Most queued pictures (1..=8).
    pub max_frames: usize,
    /// Most queued picture bytes (YUV planes).
    pub max_bytes: usize,
}

impl Default for RingBudget {
    /// 4 pictures, 32 MiB: four 1080p 8-bit 4:2:0 pictures are 12.4 MiB (specs/50 § 10).
    fn default() -> Self {
        Self {
            max_frames: 4,
            max_bytes: DEFAULT_RING_BYTES,
        }
    }
}

/// Default byte budget of a ring.
pub const DEFAULT_RING_BYTES: usize = 32 * 1024 * 1024;

/// Ring counters (`media.state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingStats {
    /// Pictures queued now.
    pub queued: u32,
    /// Bytes queued now.
    pub bytes: u64,
    /// Most bytes ever queued at once.
    pub peak_bytes: u64,
    /// The byte budget.
    pub budget_bytes: u64,
    /// The frame cap.
    pub max_frames: u32,
    /// Times the worker found the ring full and waited.
    pub full_waits: u64,
    /// Non-blocking polls that found the ring empty while the stream was still running.
    pub starved_polls: u64,
    /// Pictures dropped by a seek before they were shown.
    pub dropped_by_seek: u64,
}

fn picture_bytes(poll: &FeedPoll) -> usize {
    match poll {
        FeedPoll::Frame(p) => p.planes.iter().map(|plane| plane.len()).sum(),
        _ => 0,
    }
}

struct RingState {
    queue: VecDeque<(u64, FeedPoll, FeedStats, usize)>,
    generation: u64,
    /// Generation, target, accurate.
    seek: Option<(u64, u64, bool)>,
    stop: bool,
    worker_done: bool,
    worker_waiting: bool,
    last_size: usize,
    stats: RingStats,
}

struct Shared {
    state: Mutex<RingState>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, RingState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A [`Session`] on a worker thread feeding a bounded ring.
pub struct RingFeed {
    shared: Arc<Shared>,
    budget: RingBudget,
    stats: FeedStats,
    /// The terminal poll (`End`, or `Failed` with its error) of the current generation: the worker
    /// waits after it and queues nothing more, so later polls answer it again until a seek.
    terminal: Option<Option<MediaError>>,
    /// Seen by the session between packets (set by the drop).
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RingFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingFeed")
            .field("budget", &self.budget)
            .field("ring", &self.ring_stats())
            .finish_non_exhaustive()
    }
}

/// Sets `worker_done` and wakes every waiter when the decode thread ends, by return or unwind.
struct DoneOnExit(Arc<Shared>);

impl Drop for DoneOnExit {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.worker_done = true;
        drop(s);
        self.0.changed.notify_all();
    }
}

impl RingFeed {
    /// Moves `session` to a new thread decoding into a ring bounded by `budget`.
    ///
    /// # Errors
    /// [`MediaError::DecodeFailed`] when the thread cannot start.
    pub fn spawn(mut session: Session, budget: RingBudget) -> Result<Self, MediaError> {
        let stop = session.stop_flag();
        Self::spawn_feed(session, stop, budget)
    }

    // Keep the worker shared with the channel-scripted tests without exposing a new Session API.
    fn spawn_feed(
        mut session: impl FrameFeed + 'static,
        stop: Arc<AtomicBool>,
        budget: RingBudget,
    ) -> Result<Self, MediaError> {
        let budget = RingBudget {
            max_frames: budget.max_frames.clamp(1, 8),
            max_bytes: budget.max_bytes.max(1),
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(RingState {
                queue: VecDeque::new(),
                generation: 0,
                seek: None,
                stop: false,
                worker_done: false,
                worker_waiting: false,
                last_size: 0,
                stats: RingStats {
                    budget_bytes: budget.max_bytes as u64,
                    max_frames: budget.max_frames as u32,
                    ..RingStats::default()
                },
            }),
            changed: Condvar::new(),
        });
        let worker = shared.clone();
        let worker_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("regia-video-ring".into())
            .spawn(move || {
                // Every way out of this thread, a panic in the demuxer or the audio sink
                // included, marks the worker done and wakes the waiters, so a blocking `poll`
                // or `wait_settled` answers instead of waiting forever (audit-v035-conc H2).
                let _done = DoneOnExit(worker.clone());
                let mut idle = false;
                let mut generation = 0u64;
                loop {
                    {
                        let mut s = worker.lock();
                        loop {
                            if s.stop {
                                s.worker_done = true;
                                worker.changed.notify_all();
                                return;
                            }
                            if let Some((g, to, accurate)) = s.seek.take() {
                                generation = g;
                                drop(s);
                                if accurate {
                                    session.seek_accurate(to);
                                } else {
                                    session.seek(to);
                                }
                                idle = false;
                                s = worker.lock();
                                continue;
                            }
                            let room = s.queue.is_empty()
                                || (s.queue.len() < budget.max_frames
                                    && s.stats.bytes as usize + s.last_size <= budget.max_bytes);
                            if !idle && room {
                                break;
                            }
                            if !idle {
                                s.stats.full_waits += 1;
                            }
                            s.worker_waiting = true;
                            worker.changed.notify_all();
                            s = worker.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
                            s.worker_waiting = false;
                        }
                    }
                    let poll = {
                        let _span = tracing::trace_span!("video_decode").entered();
                        session.poll(true)
                    };
                    if worker_stop.load(Ordering::Acquire) {
                        let mut s = worker.lock();
                        s.worker_done = true;
                        worker.changed.notify_all();
                        return;
                    }
                    idle = !matches!(poll, FeedPoll::Frame(_));
                    let stats = FrameFeed::stats(&session);
                    let size = picture_bytes(&poll);
                    let mut s = worker.lock();
                    if s.generation != generation || s.seek.is_some() {
                        // A seek arrived while decoding: this picture belongs to the old position.
                        idle = false;
                        continue;
                    }
                    if size > 0 {
                        s.last_size = size;
                    }
                    s.stats.bytes += size as u64;
                    s.stats.peak_bytes = s.stats.peak_bytes.max(s.stats.bytes);
                    s.queue.push_back((generation, poll, stats, size));
                    s.stats.queued = s.queue.len() as u32;
                    worker.changed.notify_all();
                }
            })
            .map_err(|e| MediaError::DecodeFailed(format!("cannot start the decode thread: {e}")))?;
        Ok(Self {
            shared,
            budget,
            stats: FeedStats::default(),
            terminal: None,
            stop,
            thread: Some(thread),
        })
    }

    /// The limits in use.
    pub fn budget(&self) -> RingBudget {
        self.budget
    }
}

impl FrameFeed for RingFeed {
    fn poll(&mut self, block: bool) -> FeedPoll {
        match &self.terminal {
            Some(None) => return FeedPoll::End,
            Some(Some(e)) => return FeedPoll::Failed(e.clone()),
            None => {}
        }
        let mut s = self.shared.lock();
        loop {
            while let Some((generation, poll, stats, size)) = s.queue.pop_front() {
                s.stats.bytes = s.stats.bytes.saturating_sub(size as u64);
                s.stats.queued = s.queue.len() as u32;
                // The worker must look at the ring again before it counts as settled.
                s.worker_waiting = false;
                self.shared.changed.notify_all();
                if generation != s.generation {
                    s.stats.dropped_by_seek += 1;
                    continue;
                }
                self.stats = stats;
                match &poll {
                    FeedPoll::End => self.terminal = Some(None),
                    FeedPoll::Failed(e) => self.terminal = Some(Some(e.clone())),
                    FeedPoll::Frame(_) | FeedPoll::Pending => {}
                }
                return poll;
            }
            if s.worker_done {
                return FeedPoll::Failed(MediaError::DecodeFailed("decode thread ended".into()));
            }
            if !block {
                s.stats.starved_polls += 1;
                return FeedPoll::Pending;
            }
            s = self
                .shared
                .changed
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn seek(&mut self, to_ns: u64) {
        self.send_seek(to_ns, false);
    }

    fn seek_accurate(&mut self, to_ns: u64) {
        self.send_seek(to_ns, true);
    }

    fn stats(&self) -> FeedStats {
        self.stats
    }

    fn ring(&self) -> Option<RingStats> {
        Some(self.ring_stats())
    }
}

impl RingFeed {
    fn send_seek(&mut self, to_ns: u64, accurate: bool) {
        self.terminal = None;
        let mut s = self.shared.lock();
        s.generation += 1;
        let generation = s.generation;
        let dropped = s.queue.len() as u64;
        s.queue.clear();
        s.stats.dropped_by_seek += dropped;
        s.stats.bytes = 0;
        s.stats.queued = 0;
        s.seek = Some((generation, to_ns, accurate));
        s.worker_waiting = false;
        self.shared.changed.notify_all();
    }

    /// Blocks until the worker waits (the ring is full, or the stream ended or failed) with no seek
    /// pending: the ring's contents are then stable until the next poll or seek. For tests and
    /// tools that need a deterministic ring state; the player never calls it.
    pub fn wait_settled(&self) -> RingStats {
        let mut s = self.shared.lock();
        while !(s.worker_waiting && s.seek.is_none()) && !s.worker_done {
            s = self
                .shared
                .changed
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
        s.stats
    }

    /// Ring counters.
    pub fn ring_stats(&self) -> RingStats {
        self.shared.lock().stats
    }
}

impl Drop for RingFeed {
    fn drop(&mut self) {
        // Seen between packets, so a worker deep in packets that yield no picture stops too.
        self.stop.store(true, Ordering::Release);
        let ended = {
            let mut s = self.shared.lock();
            s.stop = true;
            s.queue.clear();
            self.shared.changed.notify_all();
            // Bounded (audit-v035-conc M6): a worker blocked in a read of a stalled pipe or
            // network stream is detached instead of hanging the dropping thread; it ends when its
            // read returns.
            let (s, _) = self
                .shared
                .changed
                .wait_timeout_while(s, crate::session::DECODE_THREAD_JOIN_WAIT, |s| !s.worker_done)
                .unwrap_or_else(PoisonError::into_inner);
            s.worker_done
        };
        if let Some(t) = self.thread.take() {
            if ended {
                let _ = t.join();
            } else {
                tracing::warn!(target: "regia_media", "video ring decode thread still busy at drop; detached");
            }
        }
    }
}

/// audit-v035-conc H2: a panic on the decode thread (a demuxer read here) killed it without
/// marking it done, so a blocking `poll` waited forever instead of answering `Failed`.
#[cfg(test)]
mod worker_death_tests {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{RingBudget, RingFeed};
    use crate::mux::PatternClip;
    use crate::session::{FeedPoll, FrameFeed, Session, SessionOptions};

    /// A reader that panics on every read once armed (after the session opened).
    struct ArmedReader {
        inner: Cursor<Vec<u8>>,
        armed: Arc<AtomicBool>,
    }

    impl Read for ArmedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            assert!(!self.armed.load(Ordering::Relaxed), "simulated demuxer panic");
            self.inner.read(buf)
        }
    }

    impl Seek for ArmedReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn a_decode_thread_that_panics_fails_the_feed_instead_of_hanging() {
        let clip = PatternClip::new(64, 32, 20, 30);
        let armed = Arc::new(AtomicBool::new(false));
        let reader = ArmedReader {
            inner: Cursor::new(clip.to_webm()),
            armed: armed.clone(),
        };
        let session = Session::open(reader, &SessionOptions::default(), None).expect("open");
        armed.store(true, Ordering::Relaxed);
        let mut feed = RingFeed::spawn(session, RingBudget::default()).expect("spawn");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Pictures decoded from what the demuxer already held may come first.
            let outcome = loop {
                match feed.poll(true) {
                    FeedPoll::Frame(_) => continue,
                    other => break other,
                }
            };
            let _ = tx.send(matches!(outcome, FeedPoll::Failed(_)));
        });
        // Bounds a hang only.
        let failed = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("a blocking poll on a dead decode thread never answered");
        assert!(failed, "the feed ends as Failed");
    }
}

/// audit-v035-conc M6: dropping a feed joined its decode thread without a bound, so a demuxer
/// blocked in a read of a stalled stream hung the dropping thread for as long as the read.
#[cfg(test)]
mod bounded_drop_tests {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{RingBudget, RingFeed};
    use crate::mux::PatternClip;
    use crate::session::{Session, SessionOptions, WorkerFeed};

    /// A reader whose reads block (on a channel nobody sends to) once armed: a stalled pipe.
    struct StalledReader {
        inner: Cursor<Vec<u8>>,
        armed: Arc<AtomicBool>,
        never: crossbeam_channel::Receiver<()>,
    }

    impl Read for StalledReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.armed.load(Ordering::Relaxed) {
                let _ = self.never.recv();
            }
            self.inner.read(buf)
        }
    }

    impl Seek for StalledReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// A session whose reads stall after it opened, and the sender that keeps them stalled.
    fn stalled_session() -> (Session, crossbeam_channel::Sender<()>) {
        let clip = PatternClip::new(64, 32, 20, 30);
        let armed = Arc::new(AtomicBool::new(false));
        let (keep, never) = crossbeam_channel::bounded(0);
        let reader = StalledReader {
            inner: Cursor::new(clip.to_webm()),
            armed: armed.clone(),
            never,
        };
        let session = Session::open(reader, &SessionOptions::default(), None).expect("open");
        armed.store(true, Ordering::Relaxed);
        (session, keep)
    }

    /// Drops `feed` on a helper thread; true when the drop returned (bounds a hang only).
    fn drop_returns<T: Send + 'static>(feed: T) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(feed);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(120)).is_ok()
    }

    #[test]
    fn dropping_a_ring_feed_over_a_stalled_stream_returns() {
        let (session, _keep) = stalled_session();
        let feed = RingFeed::spawn(session, RingBudget::default()).expect("spawn");
        assert!(
            drop_returns(feed),
            "the drop waited for a read that never returns"
        );
    }

    #[test]
    fn dropping_a_worker_feed_over_a_stalled_stream_returns() {
        let (session, _keep) = stalled_session();
        let feed = WorkerFeed::spawn(session, 4).expect("spawn");
        assert!(
            drop_returns(feed),
            "the drop waited for a read that never returns"
        );
    }
}

/// Channel scripts exercise the real worker/queue, without a demuxer, codec, file or clock.
#[cfg(test)]
mod seek_generation_tests {
    use std::sync::mpsc::{self, Receiver, Sender};

    use super::*;
    use crate::codec::{ChromaFormat, DecodedPicture};

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Decode,
        Seek { target: u64, accurate: bool },
        Dropped,
    }

    struct ScriptedFeed {
        events: Sender<Event>,
        replies: Receiver<FeedPoll>,
        stats: FeedStats,
    }

    impl FrameFeed for ScriptedFeed {
        fn poll(&mut self, block: bool) -> FeedPoll {
            assert!(block, "the worker requests a blocking decode");
            self.events.send(Event::Decode).expect("test is listening");
            let poll = self.replies.recv().expect("test releases each decode");
            self.stats.packets_read += 1;
            poll
        }

        fn seek(&mut self, target: u64) {
            self.events
                .send(Event::Seek {
                    target,
                    accurate: false,
                })
                .expect("test is listening");
        }

        fn seek_accurate(&mut self, target: u64) {
            self.events
                .send(Event::Seek {
                    target,
                    accurate: true,
                })
                .expect("test is listening");
        }

        fn stats(&self) -> FeedStats {
            self.stats
        }
    }

    impl Drop for ScriptedFeed {
        fn drop(&mut self) {
            let _ = self.events.send(Event::Dropped);
        }
    }

    struct Script {
        events: Receiver<Event>,
        replies: Sender<FeedPoll>,
    }

    impl Script {
        fn spawn(max_frames: usize, max_bytes: usize) -> (RingFeed, Self) {
            let (events_tx, events) = mpsc::channel();
            let (replies, replies_rx) = mpsc::channel();
            let session = ScriptedFeed {
                events: events_tx,
                replies: replies_rx,
                stats: FeedStats::default(),
            };
            let feed = RingFeed::spawn_feed(
                session,
                Arc::new(AtomicBool::new(false)),
                RingBudget {
                    max_frames,
                    max_bytes,
                },
            )
            .expect("spawn synthetic feed");
            (feed, Self { events, replies })
        }

        fn event(&self, expected: Event) {
            assert_eq!(self.events.recv().expect("worker event"), expected);
        }

        fn release(&self, poll: FeedPoll) {
            self.replies.send(poll).expect("worker awaits decode");
        }

        fn picture(&self, pts_ns: i64, width: u32) {
            self.release(FeedPoll::Frame(picture(pts_ns, width)));
        }

        fn finish(self, mut feed: RingFeed) {
            // Retain the actual handle so Drop's bounded detach cannot masquerade as completion.
            let thread = feed.thread.take().expect("worker join handle");
            let shared = feed.shared.clone();
            let Self { events, replies } = self;
            // An unexpected extra decode must fail on disconnect, not wait for an unsent reply.
            drop(replies);
            drop(feed);
            thread.join().expect("worker completed without panic");
            assert!(shared.lock().worker_done);
            assert_eq!(events.recv().expect("feed dropped"), Event::Dropped);
            assert_eq!(events.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        }
    }

    fn picture(pts_ns: i64, width: u32) -> DecodedPicture {
        // Valid 8-bit 4:2:0 at height 2: width 2 is 6 bytes; width 4 is 12 bytes.
        DecodedPicture {
            pts_ns,
            width,
            height: 2,
            bit_depth: 8,
            chroma: ChromaFormat::Yuv420,
            color: Default::default(),
            planes: [
                vec![17; width as usize * 2].into(),
                vec![31; width as usize / 2].into(),
                vec![47; width as usize / 2].into(),
            ],
        }
    }

    fn take(feed: &mut RingFeed, expected_pts: i64, expected_packets: u64) {
        let FeedPoll::Frame(p) = feed.poll(false) else {
            panic!("expected picture at {expected_pts}");
        };
        assert_eq!(p, picture(expected_pts, 2));
        assert_eq!(feed.stats().packets_read, expected_packets);
    }

    fn counts(feed: &RingFeed, queued: u32, bytes: u64, dropped: u64) {
        let stats = feed.ring_stats();
        assert_eq!(stats.queued, queued, "{stats:?}");
        assert_eq!(stats.bytes, bytes, "{stats:?}");
        assert_eq!(stats.dropped_by_seek, dropped, "{stats:?}");
    }

    #[test]
    fn two_seeks_during_one_decode_coalesce_and_never_present_the_old_generation() {
        let (mut feed, script) = Script::spawn(2, 12);
        script.event(Event::Decode);
        script.picture(1, 2);
        script.event(Event::Decode);
        script.picture(2, 2);
        let full = feed.wait_settled();
        assert_eq!((full.queued, full.bytes, full.peak_bytes), (2, 12, 12));
        assert_eq!((full.max_frames, full.budget_bytes), (2, 12));
        assert!(full.full_waits > 0);
        take(&mut feed, 1, 1);
        script.event(Event::Decode); // Old decode in flight; frame 2 is still queued.
        feed.seek(100);
        feed.seek_accurate(200);
        counts(&feed, 0, 0, 1); // Count the queued discard, once, across both seeks.
        assert!(matches!(feed.poll(false), FeedPoll::Pending));
        assert_eq!(feed.ring_stats().starved_polls, 1);
        script.picture(3, 2); // Release the pre-seek decode only after both seeks.
        script.event(Event::Seek {
            target: 200,
            accurate: true,
        });
        script.event(Event::Decode);
        counts(&feed, 0, 0, 1); // No stale bytes or picture were reinserted.
        assert!(matches!(feed.poll(false), FeedPoll::Pending));
        script.picture(200, 2);
        script.event(Event::Decode);
        script.picture(201, 2);
        feed.wait_settled();
        counts(&feed, 2, 12, 1);
        take(&mut feed, 200, 4);
        script.event(Event::Decode);
        script.release(FeedPoll::End);
        feed.wait_settled();
        counts(&feed, 2, 6, 1); // The terminal entry has no picture bytes.
        take(&mut feed, 201, 5);
        counts(&feed, 1, 0, 1);
        assert!(matches!(feed.poll(true), FeedPoll::End));
        counts(&feed, 0, 0, 1);
        assert!(matches!(feed.poll(true), FeedPoll::End));
        assert!(matches!(feed.poll(false), FeedPoll::End));
        assert_eq!(feed.stats().packets_read, 6);
        assert_eq!(feed.ring_stats().peak_bytes, 12);
        assert_eq!(feed.ring_stats().starved_polls, 2);
        script.finish(feed);
    }

    #[test]
    fn a_second_seek_invalidates_a_decode_from_the_intermediate_generation() {
        let (mut feed, script) = Script::spawn(1, 6);
        script.event(Event::Decode);
        script.picture(1, 2);
        feed.wait_settled();
        take(&mut feed, 1, 1);
        script.event(Event::Decode);
        feed.seek_accurate(100);
        script.picture(2, 2);
        script.event(Event::Seek {
            target: 100,
            accurate: true,
        });
        script.event(Event::Decode); // First seek has actually reached the worker.
        feed.seek(200);
        script.picture(100, 2);
        script.event(Event::Seek {
            target: 200,
            accurate: false,
        });
        script.event(Event::Decode);
        counts(&feed, 0, 0, 0); // In-flight discards do not increment the queued-discard counter.
        assert!(matches!(feed.poll(false), FeedPoll::Pending));
        script.picture(200, 2);
        feed.wait_settled();
        counts(&feed, 1, 6, 0);
        take(&mut feed, 200, 4);
        script.event(Event::Decode);
        script.release(FeedPoll::End);
        feed.wait_settled();
        assert!(matches!(feed.poll(true), FeedPoll::End));
        counts(&feed, 0, 0, 0);
        script.finish(feed);
    }

    #[test]
    fn stale_eof_and_failure_cannot_terminate_the_latest_generation() {
        for old_terminal in [
            FeedPoll::End,
            FeedPoll::Failed(MediaError::DecodeFailed("old".into())),
        ] {
            let (mut feed, script) = Script::spawn(2, 12);
            script.event(Event::Decode);
            feed.seek(100);
            feed.seek_accurate(200);
            script.release(old_terminal);
            script.event(Event::Seek {
                target: 200,
                accurate: true,
            });
            script.event(Event::Decode);
            counts(&feed, 0, 0, 0);
            assert!(matches!(feed.poll(false), FeedPoll::Pending));
            script.picture(200, 2);
            script.event(Event::Decode);
            script.release(FeedPoll::End);
            feed.wait_settled();
            take(&mut feed, 200, 2);
            assert!(matches!(feed.poll(true), FeedPoll::End));
            script.finish(feed);
        }
    }

    #[test]
    fn terminal_polls_latch_until_a_seek_and_workers_join_after_eof_or_error() {
        for failed in [false, true] {
            let (mut feed, script) = Script::spawn(2, 12);
            script.event(Event::Decode);
            assert!(matches!(feed.poll(false), FeedPoll::Pending));
            let terminal = || {
                if failed {
                    FeedPoll::Failed(MediaError::DecodeFailed("scripted failure".into()))
                } else {
                    FeedPoll::End
                }
            };
            script.release(terminal());
            feed.wait_settled();
            counts(&feed, 1, 0, 0);
            for block in [false, true, false] {
                match (feed.poll(block), failed) {
                    (FeedPoll::End, false) => {}
                    (FeedPoll::Failed(MediaError::DecodeFailed(message)), true) => {
                        assert_eq!(message, "scripted failure");
                    }
                    (other, _) => panic!("unexpected terminal: {other:?}"),
                }
                counts(&feed, 0, 0, 0);
                assert_eq!(feed.stats().packets_read, 1);
            }
            assert_eq!(feed.ring_stats().starved_polls, 1);
            feed.seek(300);
            script.event(Event::Seek {
                target: 300,
                accurate: false,
            });
            script.event(Event::Decode);
            assert!(matches!(feed.poll(false), FeedPoll::Pending));
            script.picture(300, 2);
            script.event(Event::Decode);
            script.release(terminal());
            feed.wait_settled();
            take(&mut feed, 300, 2);
            match (feed.poll(true), failed) {
                (FeedPoll::End, false) => {}
                (FeedPoll::Failed(MediaError::DecodeFailed(message)), true) => {
                    assert_eq!(message, "scripted failure");
                }
                (other, _) => panic!("unexpected terminal after seek: {other:?}"),
            }
            counts(&feed, 0, 0, 0);
            script.finish(feed);
        }
    }

    #[test]
    fn an_oversized_picture_is_alone_then_ordinary_pictures_resume_within_budget() {
        let (mut feed, script) = Script::spawn(3, 9);
        script.event(Event::Decode);
        script.picture(1, 4); // 12 bytes exceeds the 9-byte budget, but must make progress.
        feed.wait_settled();
        counts(&feed, 1, 12, 0);
        assert_eq!(feed.ring_stats().peak_bytes, 12);
        let FeedPoll::Frame(p) = feed.poll(false) else {
            panic!("oversized picture");
        };
        assert_eq!(p, picture(1, 4));
        script.event(Event::Decode);
        script.picture(2, 2);
        feed.wait_settled();
        counts(&feed, 1, 6, 0); // A second ordinary picture would exceed the byte budget.
        take(&mut feed, 2, 2);
        script.event(Event::Decode);
        script.picture(3, 2);
        feed.wait_settled();
        counts(&feed, 1, 6, 0);
        take(&mut feed, 3, 3);
        script.event(Event::Decode);
        script.release(FeedPoll::End);
        feed.wait_settled();
        assert!(matches!(feed.poll(true), FeedPoll::End));
        counts(&feed, 0, 0, 0);
        assert_eq!(feed.ring_stats().peak_bytes, 12);
        script.finish(feed);
    }

    #[test]
    fn dropping_the_consumer_wakes_and_joins_a_worker_waiting_on_a_full_ring() {
        // Exercise both independent limits, including the over-budget singleton exception.
        for (max_frames, max_bytes) in [(1, 60), (8, 6), (8, 1)] {
            let (feed, script) = Script::spawn(max_frames, max_bytes);
            script.event(Event::Decode);
            script.picture(1, 2);
            let full = feed.wait_settled();
            counts(&feed, 1, 6, 0);
            assert!(full.full_waits > 0);
            script.finish(feed); // No further decode release: Drop must wake the ring wait.
        }
    }
}
