// SPDX-License-Identifier: MPL-2.0
//! Threads that may be inside the GPU driver (ADR-287): one registry, so the process never runs
//! its exit handlers while one of them is still in a driver call.
//!
//! The engine runs GPU work on helper threads: the telemetry probe (allocator reports), the
//! pipeline-warm and material-graph workers (pipeline compiles), the encode workers (command
//! buffer finishes), and the clean exit's renderer and device drops. Each bounds its own shutdown
//! wait, so under load one can still be in the driver when `main` returns; the process-exit
//! handlers then tear the driver down under it (SIGSEGV on NVIDIA 615.71.09; ADR-269, ADR-276).
//!
//! Every such thread is started through [`GpuWorkerBuilder`], which registers it in a
//! [`GpuWorkers`] registry (the process's: [`gpu_workers`]) for as long as its body runs. At the
//! end of the process, [`crate::finish_exit`] asks every registered worker to stop
//! ([`GpuWorkerBuilder::on_stop`]), waits for them within one shared budget
//! ([`EXIT_WORKER_WAIT`], [`EXIT_WORKER_WAIT_ENV`]), and when one is still running at the end
//! of it ends the process as a deliberate exit (no exit handlers) instead of returning from
//! `main`.
//!
//! A worker counts as possibly inside the driver for its whole life by default: it may drop the
//! last clone of a device when it ends. A pooled worker that idles between jobs and holds no GPU
//! object of its own opts into [`GpuWorkerBuilder::driver_calls_marked`]: it counts only inside
//! its [`GpuWorker::driver_call`] sections, so an idle pool never holds the exit. CPU-only work
//! (sky tables, emissive fits, shader validation) is registered the same way with no section:
//! it is listed in the exit log, never waited for. A marked worker that takes new work opens its
//! section with [`GpuWorker::try_driver_call`], which refuses once the exit has asked it to stop:
//! otherwise it could enter the driver after the exit's wait had seen it idle and returned.
//!
//! No `catch_unwind` anywhere: shipping builds use `panic = "abort"`. A worker's registration
//! ends when its [`GpuWorker`] drops, which also happens while a panic unwinds in an unwinding
//! build; in an aborting build the process is gone.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

/// How long the end of the process waits in all for registered GPU workers (ADR-287), counted
/// from the start of the clean exit ([`note_exit_started`]). The same 30 s rule as the
/// material-graph (ADR-269) and pipeline-warm (ADR-276) waits: it only bounds a hung driver,
/// since the wait ends when the last worker does. [`EXIT_WORKER_WAIT_ENV`] overrides it.
pub const EXIT_WORKER_WAIT: Duration = Duration::from_secs(30);

/// Overrides [`EXIT_WORKER_WAIT`], in milliseconds (`0`: do not wait).
pub const EXIT_WORKER_WAIT_ENV: &str = "REGIA_EXIT_WORKER_WAIT_MS";

/// A worker's stop request ([`GpuWorkerBuilder::on_stop`]).
type StopFn = Box<dyn Fn() + Send + Sync>;

/// Wakes [`GpuWorkers::wait`] when a worker ends or leaves a driver call.
#[derive(Default)]
struct Notify {
    epoch: Mutex<u64>,
    changed: Condvar,
}

impl Notify {
    fn bump(&self) {
        *self.epoch.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.changed.notify_all();
    }
}

/// One registered worker.
struct Slot {
    name: String,
    /// Counted only inside [`GpuWorker::driver_call`] sections.
    marked: bool,
    /// Open driver-call sections.
    in_driver: AtomicU32,
    finished: AtomicBool,
    stop_requested: AtomicBool,
    /// Taken (and dropped) when the stop is signalled or the worker ends, so the registry keeps
    /// nothing the closure captured.
    on_stop: Mutex<Option<StopFn>>,
    thread: OnceLock<Thread>,
    notify: Arc<Notify>,
}

impl Slot {
    /// The exit must wait for this worker. The open sections are read `SeqCst`, pairing with
    /// [`GpuWorker::try_driver_call`] (see there).
    fn outstanding(&self) -> bool {
        !self.finished.load(Ordering::Acquire) && (!self.marked || self.in_driver.load(Ordering::SeqCst) > 0)
    }

    fn finish(&self) {
        drop(self.on_stop.lock().unwrap_or_else(PoisonError::into_inner).take());
        self.finished.store(true, Ordering::Release);
        self.notify.bump();
    }

    fn left(&self) -> LeftWorker {
        LeftWorker {
            name: self.name.clone(),
            in_driver: self.in_driver.load(Ordering::Acquire) > 0,
        }
    }
}

/// A registered worker still running when [`GpuWorkers::wait`] gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftWorker {
    /// The thread's name (`regia-pipeline-warm-3`).
    pub name: String,
    /// It was inside a [`GpuWorker::driver_call`] section. A worker that does not mark its calls
    /// reports false while it runs outside one, but still counts as possibly in the driver.
    pub in_driver: bool,
}

impl std::fmt::Display for LeftWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.in_driver {
            write!(f, "{} (in a driver call)", self.name)
        } else {
            f.write_str(&self.name)
        }
    }
}

/// A registry of threads that may be inside the GPU driver. The process has one
/// ([`gpu_workers`]); tests make their own.
pub struct GpuWorkers {
    slots: Mutex<Vec<Arc<Slot>>>,
    notify: Arc<Notify>,
    /// [`GpuWorkers::signal_stop`] ran (set under the `slots` lock): a worker registered later
    /// starts with its stop already requested.
    stopping: AtomicBool,
}

impl Default for GpuWorkers {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GpuWorkers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuWorkers")
            .field("running", &self.running())
            .finish()
    }
}

impl GpuWorkers {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            notify: Arc::new(Notify::default()),
            stopping: AtomicBool::new(false),
        }
    }

    fn insert(&self, slot: Arc<Slot>) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.retain(|s| !s.finished.load(Ordering::Acquire));
        // Under the lock `signal_stop` takes its list under: a slot is either in that list or
        // sees the stop here.
        if self.stopping.load(Ordering::SeqCst) {
            slot.stop_requested.store(true, Ordering::SeqCst);
        }
        slots.push(slot);
    }

    fn live(&self) -> Vec<Arc<Slot>> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.retain(|s| !s.finished.load(Ordering::Acquire));
        slots.clone()
    }

    /// Every registered worker that has not ended, the idle marked ones included.
    #[must_use]
    pub fn running(&self) -> Vec<LeftWorker> {
        self.live().iter().map(|s| s.left()).collect()
    }

    /// The workers the exit has to wait for: running, and either unmarked or inside a
    /// [`GpuWorker::driver_call`] section.
    #[must_use]
    pub fn outstanding(&self) -> Vec<LeftWorker> {
        self.live()
            .iter()
            .filter(|s| s.outstanding())
            .map(|s| s.left())
            .collect()
    }

    /// Asks every running worker to stop, once each: sets its [`GpuWorker::stop_requested`] flag,
    /// runs its [`GpuWorkerBuilder::on_stop`] callback (on this thread, with no registry lock
    /// held), then unparks it. Does not wait. A worker registered after this call starts with its
    /// stop already requested (its callback does not run).
    pub fn signal_stop(&self) {
        let live = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            self.stopping.store(true, Ordering::SeqCst);
            slots.retain(|s| !s.finished.load(Ordering::Acquire));
            slots.clone()
        };
        for slot in live {
            // `SeqCst`, pairing with `GpuWorker::try_driver_call`: set before `wait` reads the
            // open sections.
            if slot.stop_requested.swap(true, Ordering::SeqCst) {
                continue;
            }
            let stop = slot.on_stop.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(stop) = stop {
                stop();
            }
            if let Some(thread) = slot.thread.get() {
                thread.unpark();
            }
        }
    }

    /// Waits until no worker is [`GpuWorkers::outstanding`], at most `budget`; returns the ones
    /// still outstanding then (empty: all done). A zero budget only looks. Blocks on a condition
    /// variable that every worker's end and every closed driver call signal: no polling.
    #[must_use]
    pub fn wait(&self, budget: Duration) -> Vec<LeftWorker> {
        let deadline = Instant::now().checked_add(budget);
        let mut epoch = self.notify.epoch.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            // Checked under the epoch lock: a worker that ends after this takes the lock to
            // signal, so its wake-up cannot be missed.
            let left = self.outstanding();
            if left.is_empty() {
                return left;
            }
            epoch = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return left;
                    }
                    self.notify
                        .changed
                        .wait_timeout(epoch, remaining)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                // A budget past the clock's range: no deadline.
                None => self
                    .notify
                    .changed
                    .wait(epoch)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

/// The process's registry (the one [`GpuWorkerBuilder::spawn`] uses and [`crate::finish_exit`]
/// waits for).
#[must_use]
pub fn gpu_workers() -> &'static GpuWorkers {
    static PROCESS: OnceLock<GpuWorkers> = OnceLock::new();
    PROCESS.get_or_init(GpuWorkers::new)
}

/// When this process's clean exit started (the shared budget's origin).
static EXIT_STARTED: OnceLock<Instant> = OnceLock::new();

/// Starts the shared exit budget ([`EXIT_WORKER_WAIT`]) now, if no earlier call did. The
/// renderer's first clean-exit step calls it; without a call, the budget starts when
/// [`crate::finish_exit`] runs.
pub fn note_exit_started() {
    let _ = EXIT_STARTED.set(Instant::now());
}

/// [`EXIT_WORKER_WAIT`] or the [`EXIT_WORKER_WAIT_ENV`] override (milliseconds); an unreadable
/// value keeps the default.
#[must_use]
pub fn parse_exit_worker_wait(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(EXIT_WORKER_WAIT, Duration::from_millis)
}

/// What is left of this process's shared exit budget.
pub(crate) fn exit_worker_wait_left() -> Duration {
    let budget = parse_exit_worker_wait(regia_core::brand::env_var(EXIT_WORKER_WAIT_ENV).ok().as_deref());
    match EXIT_STARTED.get() {
        Some(started) => budget.saturating_sub(started.elapsed()),
        None => budget,
    }
}

/// Starts a registered GPU worker thread: [`std::thread::Builder`] plus the registration.
#[must_use = "a builder does nothing until spawned"]
pub struct GpuWorkerBuilder {
    name: String,
    marked: bool,
    on_stop: Option<StopFn>,
}

impl std::fmt::Debug for GpuWorkerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuWorkerBuilder")
            .field("name", &self.name)
            .field("marked", &self.marked)
            .field("on_stop", &self.on_stop.is_some())
            .finish()
    }
}

impl GpuWorkerBuilder {
    /// A worker thread named `name` (the thread's name and the exit log's).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            marked: false,
            on_stop: None,
        }
    }

    /// The worker counts as inside the driver only within its [`GpuWorker::driver_call`]
    /// sections, not for its whole life. For a pooled thread that idles between jobs and drops
    /// no GPU object of its own when it ends, and for CPU-only work (no section at all).
    pub fn driver_calls_marked(mut self) -> Self {
        self.marked = true;
        self
    }

    /// Runs `stop` (once) when the exit asks the registered workers to stop
    /// ([`GpuWorkers::signal_stop`]); the thread is unparked right after. It must not block,
    /// and should hold weak references only: the registry keeps it until it runs or the worker
    /// ends.
    pub fn on_stop(mut self, stop: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_stop = Some(Box::new(stop));
        self
    }

    /// Spawns the worker, registered in the process's registry ([`gpu_workers`]).
    ///
    /// # Errors
    /// The thread could not start (as [`std::thread::Builder::spawn`]); nothing stays registered.
    pub fn spawn<T, F>(self, body: F) -> std::io::Result<JoinHandle<T>>
    where
        F: FnOnce(&GpuWorker) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_in(gpu_workers(), body)
    }

    /// Spawns the worker, registered in `registry` until `body` has returned and dropped what
    /// it captured. `T` is dropped by whoever joins the handle: return no GPU object.
    ///
    /// # Errors
    /// The thread could not start (as [`std::thread::Builder::spawn`]); nothing stays registered.
    pub fn spawn_in<T, F>(self, registry: &GpuWorkers, body: F) -> std::io::Result<JoinHandle<T>>
    where
        F: FnOnce(&GpuWorker) -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = Arc::new(Slot {
            name: self.name.clone(),
            marked: self.marked,
            in_driver: AtomicU32::new(0),
            finished: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            on_stop: Mutex::new(self.on_stop),
            thread: OnceLock::new(),
            notify: registry.notify.clone(),
        });
        registry.insert(slot.clone());
        let worker = GpuWorker { slot: slot.clone() };
        // `body` is consumed by the call, so what it captured (a device clone) is dropped before
        // `worker`, which this closure owns until it ends: the registration covers that drop.
        // The thread records itself before anything else, so a stop signalled right after the
        // spawn still unparks it.
        let spawned = std::thread::Builder::new().name(self.name).spawn(move || {
            let _ = worker.slot.thread.set(std::thread::current());
            body(&worker)
        });
        match spawned {
            Ok(handle) => {
                let _ = slot.thread.set(handle.thread().clone());
                Ok(handle)
            }
            Err(err) => {
                slot.finish();
                Err(err)
            }
        }
    }
}

/// A running worker's registration, lent to its body. Dropped (the registration ends) after the
/// body and everything it captured.
pub struct GpuWorker {
    slot: Arc<Slot>,
}

impl std::fmt::Debug for GpuWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuWorker")
            .field("name", &self.slot.name)
            .finish()
    }
}

impl GpuWorker {
    /// Marks a driver call until the returned guard drops (nesting is fine). Cheap: two atomic
    /// operations and, on leaving, one uncontended lock to wake a waiting exit.
    pub fn driver_call(&self) -> DriverCall<'_> {
        self.slot.in_driver.fetch_add(1, Ordering::SeqCst);
        DriverCall { slot: &self.slot }
    }

    /// [`GpuWorker::driver_call`], unless the exit has asked this worker to stop
    /// ([`GpuWorkers::signal_stop`]): `None` then, and the worker must not enter the driver.
    ///
    /// A marked worker ([`GpuWorkerBuilder::driver_calls_marked`]) opens the section for new
    /// work with this, so it cannot enter the driver after the exit's [`GpuWorkers::wait`] saw it
    /// idle and returned. The section opens before the stop flag is read, and the exit sets
    /// the flag before its wait reads the open sections, all four `SeqCst`: either this call sees
    /// the stop, or the wait sees the open section and waits for it to close.
    pub fn try_driver_call(&self) -> Option<DriverCall<'_>> {
        let call = self.driver_call();
        if self.slot.stop_requested.load(Ordering::SeqCst) {
            // Closes the section again and wakes a waiting exit.
            drop(call);
            return None;
        }
        Some(call)
    }

    /// The exit asked this worker to stop ([`GpuWorkers::signal_stop`]).
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.slot.stop_requested.load(Ordering::Acquire)
    }

    /// The worker's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.slot.name
    }
}

impl Drop for GpuWorker {
    fn drop(&mut self) {
        self.slot.finish();
    }
}

/// An open driver call of a [`GpuWorker`] ([`GpuWorker::driver_call`]).
#[must_use = "the call is marked only while the guard lives"]
pub struct DriverCall<'a> {
    slot: &'a Slot,
}

impl std::fmt::Debug for DriverCall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverCall")
            .field("worker", &self.slot.name)
            .finish()
    }
}

impl Drop for DriverCall<'_> {
    fn drop(&mut self) {
        self.slot.in_driver.fetch_sub(1, Ordering::AcqRel);
        self.slot.notify.bump();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// A worker that blocks until the test sends on the returned channel.
    fn blocked(
        registry: &GpuWorkers,
        builder: GpuWorkerBuilder,
        mark: bool,
    ) -> (mpsc::Sender<()>, JoinHandle<()>) {
        let (release, wait) = mpsc::channel::<()>();
        let (entered, started) = mpsc::channel::<()>();
        let handle = builder
            .spawn_in(registry, move |worker| {
                let _call = mark.then(|| worker.driver_call());
                let _ = entered.send(());
                let _ = wait.recv();
            })
            .expect("spawn");
        // The body is running (and inside its call, when marked) before the test looks.
        started.recv().expect("worker started");
        (release, handle)
    }

    #[test]
    fn a_running_worker_is_outstanding_until_it_ends() {
        let registry = GpuWorkers::new();
        let (release, handle) = blocked(&registry, GpuWorkerBuilder::new("fake-gpu-0"), true);
        let left = registry.wait(Duration::ZERO);
        assert_eq!(
            left,
            [LeftWorker {
                name: "fake-gpu-0".into(),
                in_driver: true
            }]
        );
        release.send(()).expect("release");
        handle.join().expect("join");
        assert!(registry.wait(Duration::ZERO).is_empty());
        assert!(
            registry.running().is_empty(),
            "a finished worker leaves the registry"
        );
    }

    #[test]
    fn a_marked_worker_counts_only_inside_its_driver_calls() {
        let registry = GpuWorkers::new();
        let (release, handle) = blocked(
            &registry,
            GpuWorkerBuilder::new("fake-pool-0").driver_calls_marked(),
            false,
        );
        assert!(
            registry.outstanding().is_empty(),
            "an idle pool worker does not hold the exit"
        );
        assert_eq!(registry.running().len(), 1, "but it is listed");
        let (release_in, handle_in) = blocked(
            &registry,
            GpuWorkerBuilder::new("fake-pool-1").driver_calls_marked(),
            true,
        );
        assert_eq!(registry.outstanding().len(), 1);
        assert_eq!(registry.outstanding()[0].name, "fake-pool-1");
        for (tx, h) in [(release, handle), (release_in, handle_in)] {
            tx.send(()).expect("release");
            h.join().expect("join");
        }
        assert!(registry.running().is_empty());
    }

    #[test]
    fn the_stop_signal_ends_a_worker_and_the_wait_returns_after_it() {
        let registry = GpuWorkers::new();
        let (release, wait) = mpsc::channel::<()>();
        let (entered, started) = mpsc::channel::<()>();
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let worker_log = log.clone();
        let stop_log = log.clone();
        // The worker blocks until its stop callback releases it: only the exit's signal can end
        // it, so a wait that returns empty has waited for it.
        let release = Mutex::new(release);
        let handle = GpuWorkerBuilder::new("fake-probe")
            .on_stop(move || {
                stop_log
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push("stop signalled");
                let _ = release.lock().unwrap_or_else(PoisonError::into_inner).send(());
            })
            .spawn_in(&registry, move |worker| {
                let _ = entered.send(());
                let _ = wait.recv();
                assert!(worker.stop_requested());
                worker_log
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push("worker ended");
            })
            .expect("spawn");
        started.recv().expect("started");
        registry.signal_stop();
        registry.signal_stop(); // once per worker
        let left = registry.wait(Duration::MAX);
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            *log.lock().unwrap_or_else(PoisonError::into_inner),
            ["stop signalled", "worker ended"]
        );
        handle.join().expect("join");
    }

    #[test]
    fn what_the_body_captured_drops_before_the_registration_ends() {
        struct Probe(Arc<AtomicBool>, Arc<GpuWorkers>);
        impl Drop for Probe {
            fn drop(&mut self) {
                // The capture's drop (a device clone's, in a real worker) still counts.
                self.0.store(!self.1.outstanding().is_empty(), Ordering::Release);
            }
        }
        let registry = Arc::new(GpuWorkers::new());
        let seen = Arc::new(AtomicBool::new(false));
        let probe = Probe(seen.clone(), registry.clone());
        let handle = GpuWorkerBuilder::new("fake-drop")
            .spawn_in(&registry, move |_| {
                let _held = &probe;
            })
            .expect("spawn");
        handle.join().expect("join");
        assert!(
            seen.load(Ordering::Acquire),
            "registered while the capture dropped"
        );
        assert!(registry.wait(Duration::ZERO).is_empty());
    }

    /// A marked pool worker that takes work after the exit's stop would enter the driver after
    /// the exit's wait saw it idle (grok27 diff-core #1): `try_driver_call` refuses then, while a
    /// section opened before the stop still holds the exit until it closes.
    #[test]
    fn a_stopped_marked_worker_opens_no_driver_call() {
        let registry = GpuWorkers::new();
        let (go, wait_go) = mpsc::channel::<()>();
        let (opened, saw_open) = mpsc::channel::<bool>();
        let handle = GpuWorkerBuilder::new("fake-pool-2")
            .driver_calls_marked()
            .spawn_in(&registry, move |worker| {
                let first = worker.try_driver_call();
                let _ = opened.send(first.is_some());
                // The test signals the stop while this section is open.
                let _ = wait_go.recv();
                drop(first);
                worker.try_driver_call().is_some()
            })
            .expect("spawn");
        assert!(saw_open.recv().expect("worker"), "no stop yet: the call opens");
        assert_eq!(
            registry.outstanding(),
            [LeftWorker {
                name: "fake-pool-2".into(),
                in_driver: true
            }]
        );
        registry.signal_stop();
        assert_eq!(
            registry.wait(Duration::ZERO).len(),
            1,
            "a section opened before the stop still holds the exit"
        );
        go.send(()).expect("release");
        assert!(
            !handle.join().expect("join"),
            "no driver call opens after the stop"
        );
        assert!(registry.wait(Duration::MAX).is_empty());
    }

    #[test]
    fn a_worker_registered_after_the_stop_starts_stopped() {
        let registry = GpuWorkers::new();
        registry.signal_stop();
        let handle = GpuWorkerBuilder::new("fake-late")
            .driver_calls_marked()
            .spawn_in(&registry, |worker| {
                (worker.stop_requested(), worker.try_driver_call().is_some())
            })
            .expect("spawn");
        assert_eq!(handle.join().expect("join"), (true, false));
        assert!(registry.wait(Duration::ZERO).is_empty());
    }

    #[test]
    fn the_wait_override_reads_milliseconds() {
        assert_eq!(parse_exit_worker_wait(None), EXIT_WORKER_WAIT);
        assert_eq!(parse_exit_worker_wait(Some("0")), Duration::ZERO);
        assert_eq!(parse_exit_worker_wait(Some(" 250 ")), Duration::from_millis(250));
        assert_eq!(parse_exit_worker_wait(Some("soon")), EXIT_WORKER_WAIT);
    }

    #[test]
    fn left_workers_name_their_driver_call() {
        let left = LeftWorker {
            name: "regia-matgraph-1".into(),
            in_driver: true,
        };
        assert_eq!(left.to_string(), "regia-matgraph-1 (in a driver call)");
    }
}
