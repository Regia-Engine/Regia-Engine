// SPDX-License-Identifier: MPL-2.0
//! One load at a time, with cancel: a level (map) switch that never holds two maps (ADR-8027).
//!
//! A game that loads its next map on a worker (a lobby preloading the picked map, a menu switching
//! maps) has to stop the old load when the player picks another one, and free what that load and
//! the map before it built. Dropping only the receiving end of the worker's channel is not enough:
//! the old load runs to its end beside the new one, and the old map is freed at the same time the
//! new one fills memory, so the peak is two maps (and the old load's own peak) instead of one.
//!
//! [`LoadSwitch`] keeps the loads in a line:
//!
//! - [`LoadSwitch::switch_to`] cancels the load in flight (its [`CancelToken`] reads cancelled),
//!   retires the current value, and starts the new load on a worker of its own.
//! - Retiring means: a *retire worker* waits for the cancelled load's thread to end and drops
//!   whatever it built (a finished value nobody wants any more is dropped there too), or drops the
//!   retired value. Dropping happens on that worker, never on the caller's frame.
//! - A new load waits until no retire worker is running before it calls its load function. So at
//!   most one map is alive at a time: the current value or the load in flight, not both, as long as
//!   each load returns soon after its token reads cancelled.
//! - A load switched away from before its turn came never runs (it is counted as skipped).
//!
//! The value type does its own cleanup in `Drop` (stop its threads, unmap its files); the switch
//! only decides *where* and *when* that drop runs. A load function polls
//! [`CancelToken::is_cancelled`] (or `?`s [`CancelToken::check`]) between steps; a load that never
//! looks finishes first and is then dropped, and the next load waits for it.
//!
//! [`LoadSwitch::poll`] (non-blocking, for a frame loop) and [`LoadSwitch::wait`] (blocking, for
//! tools and tests) report a load landing or failing. [`LoadSwitch::stats`] counts loads started,
//! skipped, cancelled, landed and failed and values retired; `regia_framework`'s level switch
//! reports them as `level.switch_status`.
//!
//! ```
//! use regia_core::load_switch::{LoadSwitch, SwitchEvent};
//! use std::time::Duration;
//!
//! let mut maps: LoadSwitch<Vec<u8>> = LoadSwitch::new();
//! maps.switch_to("canyon", |token| {
//!     let mut bank = Vec::new();
//!     for _ in 0..4 {
//!         token.check()?; // returns early once the player picked another map
//!         bank.extend_from_slice(&[1_u8; 1024]);
//!     }
//!     Ok(bank)
//! });
//! let event = maps.wait(Duration::from_secs(30));
//! assert!(matches!(event, Some(SwitchEvent::Loaded { .. })));
//! assert_eq!(maps.current().map(Vec::len), Some(4096));
//! maps.unload(); // dropped on a retire worker
//! assert!(maps.wait_retired(Duration::from_secs(30)));
//! ```

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Retire workers in flight, shared by a switch and its loads.
#[derive(Default)]
struct Gate {
    pending: Mutex<usize>,
    changed: Condvar,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, usize> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes every waiter so it re-reads its condition (a count change or a cancel).
    fn wake(&self) {
        let _held = self.lock();
        self.changed.notify_all();
    }

    /// Blocks until no retire worker runs, or `cancel` is set. True when the gate is clear.
    fn wait_clear(&self, cancel: &AtomicBool) -> bool {
        let mut pending = self.lock();
        while *pending > 0 && !cancel.load(Ordering::Acquire) {
            pending = self.changed.wait(pending).unwrap_or_else(PoisonError::into_inner);
        }
        *pending == 0
    }

    /// Blocks until no retire worker runs, or `timeout` passes. True when the gate is clear.
    fn wait_idle(&self, timeout: Duration) -> bool {
        let pending = self.lock();
        let (pending, _) = self
            .changed
            .wait_timeout_while(pending, timeout, |pending| *pending > 0)
            .unwrap_or_else(PoisonError::into_inner);
        *pending == 0
    }
}

/// Counts one retire worker for as long as it lives (also when its drop panics).
struct RetireTicket(Arc<Gate>);

impl RetireTicket {
    fn new(gate: &Arc<Gate>) -> Self {
        *gate.lock() += 1;
        Self(Arc::clone(gate))
    }
}

impl Drop for RetireTicket {
    fn drop(&mut self) {
        let mut pending = self.0.lock();
        *pending = pending.saturating_sub(1);
        self.0.changed.notify_all();
    }
}

/// The cancel flag a load function reads between its steps.
#[derive(Clone, Debug)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that is never cancelled by a switch (for running a load function directly).
    #[must_use]
    pub fn never() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// True once the load was switched away from or cancelled: return soon (any `Err`).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// The flag itself, for a loader that takes a shared cancel flag (it reads `true` once the
    /// load was cancelled; the loader must only read it).
    #[must_use]
    pub fn flag(&self) -> &Arc<AtomicBool> {
        &self.flag
    }

    /// `Err(Cancelled)` once the load was cancelled, for `?` between load steps.
    ///
    /// # Errors
    /// [`Cancelled`] when the token reads cancelled.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A load stopped because it was switched away from (see [`CancelToken::check`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("load cancelled")
    }
}

impl std::error::Error for Cancelled {}

impl From<Cancelled> for String {
    fn from(value: Cancelled) -> Self {
        value.to_string()
    }
}

/// What [`LoadSwitch::poll`] and [`LoadSwitch::wait`] report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchEvent {
    /// The load landed; its value is now [`LoadSwitch::current`].
    Loaded {
        /// The name it was started with.
        name: String,
    },
    /// The load returned an error, or its thread ended without a result (a panic, a thread that
    /// could not start). Nothing is current.
    Failed {
        /// The name it was started with.
        name: String,
        /// The load's error text.
        error: String,
    },
}

/// Counters of one [`LoadSwitch`] since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadSwitchStats {
    /// Loads whose load function ran.
    pub loads_started: u64,
    /// Loads switched away from before their turn: their load function never ran.
    pub loads_skipped: u64,
    /// Loads switched away from or cancelled while running or queued (includes the skipped ones).
    pub loads_cancelled: u64,
    /// Loads that landed and became current.
    pub loads_landed: u64,
    /// Loads that failed (an error, a panic, a thread that did not start).
    pub loads_failed: u64,
    /// Values and cancelled loads handed to a retire worker.
    pub retires: u64,
    /// The longest a load waited for the retire workers before it ran, in milliseconds.
    pub max_wait_ms: u64,
    /// How long the most recent load that ran waited for the retire workers, in milliseconds.
    pub last_wait_ms: u64,
}

#[derive(Default)]
struct Counters {
    started: AtomicU64,
    skipped: AtomicU64,
    max_wait_ms: AtomicU64,
    last_wait_ms: AtomicU64,
}

/// A load in flight. The receiver sits in a mutex so the switch is `Sync` (an ECS resource).
struct Job<T> {
    name: String,
    cancel: Arc<AtomicBool>,
    rx: Mutex<Receiver<Result<T, String>>>,
    thread: Option<JoinHandle<()>>,
}

/// One value (one map) loaded at a time, with cancel. See the [module docs](self).
pub struct LoadSwitch<T: Send + 'static> {
    current: Option<(String, T)>,
    job: Option<Job<T>>,
    gate: Arc<Gate>,
    counters: Arc<Counters>,
    cancelled: u64,
    landed: u64,
    failed: u64,
    retires: u64,
}

impl<T: Send + 'static> Default for LoadSwitch<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> fmt::Debug for LoadSwitch<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadSwitch")
            .field("current", &self.current_name())
            .field("loading", &self.loading())
            .field("retiring", &self.retiring())
            .field("stats", &self.stats())
            .finish()
    }
}

impl<T: Send + 'static> LoadSwitch<T> {
    /// A switch with nothing loaded.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: None,
            job: None,
            gate: Arc::new(Gate::default()),
            counters: Arc::new(Counters::default()),
            cancelled: 0,
            landed: 0,
            failed: 0,
            retires: 0,
        }
    }

    /// Switches to a new value: cancels the load in flight, retires the current value, and runs
    /// `load` on a worker named after `name` once every retire worker has finished. Returns at once.
    ///
    /// `load` gets the job's [`CancelToken`]; it should return early (any `Err`) once the token
    /// reads cancelled. Its value lands on a later [`LoadSwitch::poll`] or [`LoadSwitch::wait`].
    pub fn switch_to<F>(&mut self, name: impl Into<String>, load: F)
    where
        F: FnOnce(&CancelToken) -> Result<T, String> + Send + 'static,
    {
        let name = name.into();
        let _span = tracing::info_span!("load_switch", name = %name).entered();
        self.cancel();
        self.unload();
        let (tx, rx) = mpsc::channel::<Result<T, String>>();
        let failed_start = tx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = CancelToken {
            flag: Arc::clone(&cancel),
        };
        let gate = Arc::clone(&self.gate);
        let counters = Arc::clone(&self.counters);
        let thread_name = format!("regia-load-{name}");
        let spawned = std::thread::Builder::new().name(thread_name).spawn(move || {
            let queued = Instant::now();
            let clear = gate.wait_clear(&token.flag);
            let waited = u64::try_from(queued.elapsed().as_millis()).unwrap_or(u64::MAX);
            counters.max_wait_ms.fetch_max(waited, Ordering::Relaxed);
            if !clear || token.is_cancelled() {
                counters.skipped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            counters.started.fetch_add(1, Ordering::Relaxed);
            counters.last_wait_ms.store(waited, Ordering::Relaxed);
            let result = {
                let _span = tracing::info_span!("load_switch_load").entered();
                load(&token)
            };
            // Nobody receives any more (the switch was dropped): the value drops here, on this
            // worker, which is where it would have been retired.
            drop(tx.send(result));
        });
        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(err) => {
                drop(failed_start.send(Err(format!("could not start the load thread: {err}"))));
                None
            }
        };
        self.job = Some(Job {
            name,
            cancel,
            rx: Mutex::new(rx),
            thread,
        });
    }

    /// Cancels the load in flight, if any: its token reads cancelled and a retire worker waits for
    /// its thread and drops what it built. The current value stays. True when a load was cancelled.
    pub fn cancel(&mut self) -> bool {
        let Some(job) = self.job.take() else {
            return false;
        };
        tracing::info!(target: "regia_core", load = %job.name, "load switch: cancelling the load in flight");
        job.cancel.store(true, Ordering::Release);
        // A load still queued behind the gate wakes and sees the flag.
        self.gate.wake();
        self.cancelled += 1;
        let Job { rx, thread, .. } = job;
        self.spawn_retire(move || {
            if let Some(thread) = thread {
                drop(thread.join());
            }
            // A value that landed after the cancel is dropped here.
            let rx = rx.into_inner().unwrap_or_else(PoisonError::into_inner);
            while let Ok(result) = rx.try_recv() {
                drop(result);
            }
        });
        true
    }

    /// Retires the current value on a retire worker. True when there was one.
    pub fn unload(&mut self) -> bool {
        match self.current.take() {
            Some((name, value)) => {
                tracing::info!(target: "regia_core", value = %name, "load switch: retiring the current value");
                self.retire(value);
                true
            }
            None => false,
        }
    }

    /// Drops `value` on a retire worker; the next load waits for it. For a value the caller took
    /// out of the switch ([`LoadSwitch::take_current`]) or one it held elsewhere (the open level's
    /// parsed data).
    pub fn retire<V: Send + 'static>(&mut self, value: V) {
        self.spawn_retire(move || drop(value));
    }

    /// Reports a load that landed or failed since the last call, without blocking.
    pub fn poll(&mut self) -> Option<SwitchEvent> {
        let job = self.job.as_ref()?;
        let received = match job.rx.lock().unwrap_or_else(PoisonError::into_inner).try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => None,
        };
        Some(self.finish_job(received))
    }

    /// Like [`LoadSwitch::poll`], but blocks up to `timeout` for the load in flight. `None` when
    /// nothing is in flight or it did not finish in time.
    pub fn wait(&mut self, timeout: Duration) -> Option<SwitchEvent> {
        let job = self.job.as_ref()?;
        let received = match job
            .rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(timeout)
        {
            Ok(result) => Some(result),
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => None,
        };
        Some(self.finish_job(received))
    }

    fn finish_job(&mut self, received: Option<Result<T, String>>) -> SwitchEvent {
        let Some(job) = self.job.take() else {
            return SwitchEvent::Failed {
                name: String::new(),
                error: "no load in flight".into(),
            };
        };
        // The worker sends as its last act: its thread ends right after, holding nothing.
        drop(job.thread);
        match received {
            Some(Ok(value)) => {
                self.landed += 1;
                tracing::info!(target: "regia_core", value = %job.name, "load switch: landed");
                let name = job.name.clone();
                self.current = Some((job.name, value));
                SwitchEvent::Loaded { name }
            }
            Some(Err(error)) => {
                self.failed += 1;
                tracing::warn!(target: "regia_core", value = %job.name, "load switch: load failed: {error}");
                SwitchEvent::Failed {
                    name: job.name,
                    error,
                }
            }
            None => {
                self.failed += 1;
                let error = "the load ended without a result (it panicked)".to_string();
                tracing::warn!(target: "regia_core", value = %job.name, "load switch: {error}");
                SwitchEvent::Failed {
                    name: job.name,
                    error,
                }
            }
        }
    }

    /// The current (landed) value.
    #[must_use]
    pub fn current(&self) -> Option<&T> {
        self.current.as_ref().map(|(_, value)| value)
    }

    /// The current value, mutably.
    pub fn current_mut(&mut self) -> Option<&mut T> {
        self.current.as_mut().map(|(_, value)| value)
    }

    /// The name the current value was loaded under.
    #[must_use]
    pub fn current_name(&self) -> Option<&str> {
        self.current.as_ref().map(|(name, _)| name.as_str())
    }

    /// Takes the current value out of the switch. The caller owns it from then on: hand it back
    /// with [`LoadSwitch::retire`] so it is freed on a worker and the next load waits for it.
    pub fn take_current(&mut self) -> Option<(String, T)> {
        self.current.take()
    }

    /// The name of the load in flight (queued or running).
    #[must_use]
    pub fn loading(&self) -> Option<&str> {
        self.job.as_ref().map(|job| job.name.as_str())
    }

    /// Retire workers still running.
    #[must_use]
    pub fn retiring(&self) -> usize {
        *self.gate.lock()
    }

    /// Blocks until every retire worker has finished, or `timeout` passes. True when none runs.
    #[must_use]
    pub fn wait_retired(&self, timeout: Duration) -> bool {
        self.gate.wait_idle(timeout)
    }

    /// Counters since the switch was made.
    #[must_use]
    pub fn stats(&self) -> LoadSwitchStats {
        LoadSwitchStats {
            loads_started: self.counters.started.load(Ordering::Relaxed),
            loads_skipped: self.counters.skipped.load(Ordering::Relaxed),
            loads_cancelled: self.cancelled,
            loads_landed: self.landed,
            loads_failed: self.failed,
            retires: self.retires,
            max_wait_ms: self.counters.max_wait_ms.load(Ordering::Relaxed),
            last_wait_ms: self.counters.last_wait_ms.load(Ordering::Relaxed),
        }
    }

    fn spawn_retire(&mut self, work: impl FnOnce() + Send + 'static) {
        self.retires += 1;
        let ticket = RetireTicket::new(&self.gate);
        // The closure goes through a slot so a failed spawn can still run it here.
        let slot = Arc::new(Mutex::new(Some(work)));
        let theirs = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("regia-load-retire".into())
            .spawn(move || {
                let _ticket = ticket;
                let _span = tracing::info_span!("load_switch_retire").entered();
                let work = theirs.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(work) = work {
                    work();
                }
            });
        if spawned.is_err() {
            // No thread: free it here rather than leave it beside the next load. The ticket went
            // with the closure the spawn dropped, so the gate is already clear of it.
            tracing::warn!(target: "regia_core", "load switch: no retire thread; freeing on the caller");
            let work = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(work) = work {
                work();
            }
        }
    }
}

impl<T: Send + 'static> Drop for LoadSwitch<T> {
    /// Cancels the load in flight and retires the current value; the retire workers finish on
    /// their own (the drop does not wait for them).
    fn drop(&mut self) {
        self.cancel();
        self.unload();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const DEADLINE: Duration = Duration::from_secs(60);

    /// A test value: counts the values alive at once and keeps the highest count.
    struct Map {
        probe: Probe,
    }

    impl Drop for Map {
        fn drop(&mut self) {
            self.probe.live.fetch_sub(1, Ordering::SeqCst);
            self.probe.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default)]
    struct Probe {
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        ran: Arc<AtomicUsize>,
    }

    impl Probe {
        fn map(&self) -> Map {
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            Map { probe: self.clone() }
        }
    }

    /// Hands out load steps; a load blocks for each step until the test grants it. A poke wakes
    /// the waiters without a grant so a cancelled load sees its token.
    #[derive(Clone, Default)]
    struct Pacer {
        state: Arc<(Mutex<(u64, u64)>, Condvar)>,
    }

    impl Pacer {
        fn grant(&self, steps: u64) {
            let (lock, cv) = &*self.state;
            lock.lock().expect("test lock").0 += steps;
            cv.notify_all();
        }

        fn poke(&self) {
            let (lock, cv) = &*self.state;
            lock.lock().expect("test lock").1 += 1;
            cv.notify_all();
        }

        /// Waits for step `index` (0-based): false when `token` reads cancelled first.
        fn step(&self, index: u64, token: &CancelToken) -> bool {
            let (lock, cv) = &*self.state;
            let mut state = lock.lock().expect("test lock");
            loop {
                if token.is_cancelled() {
                    return false;
                }
                if state.0 > index {
                    return true;
                }
                state = cv.wait(state).expect("test wait");
            }
        }

        /// Steps taken so far are counted by the load into `done`.
        fn load(
            &self,
            probe: &Probe,
            steps: u64,
            done: Arc<(Mutex<u64>, Condvar)>,
        ) -> impl FnOnce(&CancelToken) -> Result<Map, String> + Send + 'static {
            let pacer = self.clone();
            let probe = probe.clone();
            move |token: &CancelToken| {
                probe.ran.fetch_add(1, Ordering::SeqCst);
                // The partial bank is alive from the first step.
                let map = probe.map();
                for index in 0..steps {
                    if !pacer.step(index, token) {
                        return Err(Cancelled.into());
                    }
                    let (lock, cv) = &*done;
                    *lock.lock().expect("test lock") += 1;
                    cv.notify_all();
                }
                Ok(map)
            }
        }
    }

    fn progress() -> Arc<(Mutex<u64>, Condvar)> {
        Arc::new((Mutex::new(0), Condvar::new()))
    }

    fn wait_progress(done: &Arc<(Mutex<u64>, Condvar)>, at_least: u64) {
        let (lock, cv) = &**done;
        let guard = lock.lock().expect("test lock");
        let (guard, timeout) = cv
            .wait_timeout_while(guard, DEADLINE, |n| *n < at_least)
            .expect("test wait");
        assert!(!timeout.timed_out(), "load stuck at step {}", *guard);
    }

    fn landed(switch: &mut LoadSwitch<Map>) -> String {
        match switch.wait(DEADLINE) {
            Some(SwitchEvent::Loaded { name }) => name,
            other => panic!("expected a landed load, got {other:?}"),
        }
    }

    #[test]
    fn switch_mid_load_cancels_it_and_the_next_load_waits_for_the_free() {
        let probe = Probe::default();
        let (pa, pb) = (Pacer::default(), Pacer::default());
        let (da, db) = (progress(), progress());
        let mut switch = LoadSwitch::new();
        switch.switch_to("a", pa.load(&probe, 100, Arc::clone(&da)));
        pa.grant(3);
        wait_progress(&da, 3);
        switch.switch_to("b", pb.load(&probe, 2, Arc::clone(&db)));
        // B is queued behind A's free; A sits in its step wait until poked.
        assert_eq!(switch.loading(), Some("b"));
        pa.poke();
        pb.grant(2);
        assert_eq!(landed(&mut switch), "b");
        assert_eq!(
            probe.peak.load(Ordering::SeqCst),
            1,
            "two maps were alive at once"
        );
        assert_eq!(probe.live.load(Ordering::SeqCst), 1);
        assert_eq!(
            *da.0.lock().expect("test lock"),
            3,
            "the cancelled load ran on after the switch"
        );
        let stats = switch.stats();
        assert_eq!(stats.loads_started, 2);
        assert_eq!(stats.loads_cancelled, 1);
        assert_eq!(stats.loads_landed, 1);
        assert_eq!(stats.loads_skipped, 0);
        assert_eq!(switch.current_name(), Some("b"));
    }

    #[test]
    fn a_load_switched_away_from_before_its_turn_never_runs() {
        let probe = Probe::default();
        let (pa, pb, pc) = (Pacer::default(), Pacer::default(), Pacer::default());
        let mut switch = LoadSwitch::new();
        switch.switch_to("a", pa.load(&probe, 100, progress()));
        pa.grant(1);
        // B queues behind A's retire (A blocks in its second step until poked), then C replaces B.
        switch.switch_to("b", pb.load(&probe, 1, progress()));
        switch.switch_to("c", pc.load(&probe, 1, progress()));
        pa.poke();
        pb.grant(1);
        pc.grant(1);
        assert_eq!(landed(&mut switch), "c");
        assert!(switch.wait_retired(DEADLINE));
        // A may have been cancelled before it started (then it never ran either).
        assert!(
            probe.ran.load(Ordering::SeqCst) <= 2,
            "the skipped pick loaded anyway"
        );
        let stats = switch.stats();
        assert!(stats.loads_skipped >= 1, "{stats:?}");
        assert_eq!(stats.loads_started + stats.loads_skipped, 3, "{stats:?}");
        assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
        assert_eq!(probe.live.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_load_that_ignores_the_token_is_freed_on_a_worker_before_the_next_runs() {
        let probe = Probe::default();
        let (pa, pb) = (Pacer::default(), Pacer::default());
        let mut switch = LoadSwitch::new();
        let started = progress();
        let slow = {
            let probe = probe.clone();
            let pa = pa.clone();
            let started = Arc::clone(&started);
            move |_token: &CancelToken| {
                let map = probe.map();
                {
                    let (lock, cv) = &*started;
                    *lock.lock().expect("test lock") = 1;
                    cv.notify_all();
                }
                // Past its last check: waits for its step with a token nobody cancels.
                pa.step(0, &CancelToken::never());
                Ok(map)
            }
        };
        switch.switch_to("a", slow);
        wait_progress(&started, 1);
        switch.switch_to("b", pb.load(&probe, 1, progress()));
        pb.grant(1);
        // A lands after the cancel; B still waits for A's free.
        pa.grant(1);
        assert_eq!(landed(&mut switch), "b");
        assert_eq!(
            probe.peak.load(Ordering::SeqCst),
            1,
            "two maps were alive at once"
        );
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 1, "the late value was kept");
    }

    #[test]
    fn twenty_switches_keep_at_most_one_map_and_free_every_other() {
        let probe = Probe::default();
        let mut switch = LoadSwitch::new();
        let mut pacer = Pacer::default();
        switch.switch_to("map-0", pacer.load(&probe, 4, progress()));
        for round in 1..=20_u64 {
            if round % 3 == 0 {
                // Let this one land, then switch away from the landed map.
                pacer.grant(4);
                landed(&mut switch);
            } else {
                // Switch mid-load (after `round % 4` steps).
                pacer.grant(round % 4);
                pacer.poke();
            }
            let next = Pacer::default();
            switch.switch_to(format!("map-{round}"), next.load(&probe, 4, progress()));
            pacer.poke();
            pacer = next;
        }
        pacer.grant(4);
        assert_eq!(landed(&mut switch), "map-20");
        assert!(switch.wait_retired(DEADLINE));
        assert_eq!(
            probe.peak.load(Ordering::SeqCst),
            1,
            "two maps were alive at once"
        );
        assert_eq!(
            probe.live.load(Ordering::SeqCst),
            1,
            "a switched-away map was never freed"
        );
        let ran = probe.ran.load(Ordering::SeqCst);
        assert_eq!(probe.dropped.load(Ordering::SeqCst), ran - 1);
        switch.unload();
        assert!(switch.wait_retired(DEADLINE));
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_or_panicking_load_reports_and_keeps_nothing() {
        let probe = Probe::default();
        let mut switch: LoadSwitch<Map> = LoadSwitch::new();
        let p = probe.clone();
        switch.switch_to("broken", move |_| {
            let _partial = p.map();
            Err("bad header".to_string())
        });
        assert_eq!(
            switch.wait(DEADLINE),
            Some(SwitchEvent::Failed {
                name: "broken".into(),
                error: "bad header".into()
            })
        );
        switch.switch_to("panics", |_| panic!("load step exploded"));
        match switch.wait(DEADLINE) {
            Some(SwitchEvent::Failed { name, .. }) => assert_eq!(name, "panics"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(switch.current().is_none());
        assert_eq!(switch.stats().loads_failed, 2);
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unload_and_retire_free_on_a_worker_and_gate_the_next_load() {
        let probe = Probe::default();
        let mut switch = LoadSwitch::new();
        let p = probe.clone();
        switch.switch_to("a", move |_| Ok(p.map()));
        assert_eq!(landed(&mut switch), "a");
        let (_, taken) = switch.take_current().expect("current");
        assert!(switch.current().is_none());
        // A value held elsewhere goes back through the switch; the next load waits for its drop.
        switch.retire(taken);
        let p = probe.clone();
        switch.switch_to("b", move |_| Ok(p.map()));
        assert_eq!(landed(&mut switch), "b");
        assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
        assert!(switch.unload());
        assert!(!switch.unload());
        assert!(switch.wait_retired(DEADLINE));
        assert_eq!(switch.retiring(), 0);
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
        assert_eq!(switch.stats().retires, 2);
    }

    #[test]
    fn cancel_keeps_the_current_value_and_drop_frees_everything() {
        let probe = Probe::default();
        let pacer = Pacer::default();
        let mut switch = LoadSwitch::new();
        let p = probe.clone();
        switch.switch_to("a", move |_| Ok(p.map()));
        assert_eq!(landed(&mut switch), "a");
        assert!(!switch.cancel(), "nothing in flight");
        // A load started directly would retire "a"; keep it by cancelling only what is in flight.
        let job_probe = probe.clone();
        let (lock_step, step_pacer) = (progress(), pacer.clone());
        let keep = switch.take_current().expect("a");
        switch.switch_to("b", step_pacer.load(&job_probe, 5, Arc::clone(&lock_step)));
        switch.retire(keep);
        assert!(switch.cancel());
        pacer.poke();
        assert!(
            switch.wait(Duration::from_millis(0)).is_none(),
            "nothing left in flight"
        );
        assert!(switch.wait_retired(DEADLINE));
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
        let p = probe.clone();
        switch.switch_to("c", move |_| Ok(p.map()));
        assert_eq!(landed(&mut switch), "c");
        let gate = Arc::clone(&switch.gate);
        drop(switch);
        assert!(gate.wait_idle(DEADLINE));
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn token_check_and_never() {
        let token = CancelToken::never();
        assert!(token.check().is_ok());
        token.flag.store(true, Ordering::Release);
        assert_eq!(token.check(), Err(Cancelled));
        assert!(token.flag().load(Ordering::Acquire));
        assert_eq!(String::from(Cancelled), "load cancelled");
    }
}
