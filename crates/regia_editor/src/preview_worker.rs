// SPDX-License-Identifier: MPL-2.0
//! Preview renders off the UI thread (ADR-8584).
//!
//! The first draw of an editor preview builds a frame renderer and compiles its pipelines (the
//! forward shader with a material graph's code composed in, the post chain), which took 10 s on
//! an RTX 3080 Ti and over a minute on lavapipe with the UI thread waiting. The Material Graph
//! preview and its node swatches, the animation editors' rig preview and the viewport's camera
//! preview warm-up now run their renders as jobs on one background thread, `regia-preview`: the
//! UI frame that opens the panel only queues the job and draws a placeholder (the last image,
//! the line rig, or a "compiling" note), and the finished image is uploaded by the first frame
//! after the job ends ([`AsyncPreview::take`](crate::preview_worker::AsyncPreview::take)).
//!
//! One thread, not a pool: the preview renderers are process-wide statics behind mutexes, so
//! two renders of one kind could not overlap anyway, and a single queue keeps the GPU work of
//! previews from competing with itself. Each panel owns an [`AsyncPreview`](crate::preview_worker::AsyncPreview): a request with the
//! key already requested is a no-op, a request while a job runs replaces the one waiting (the
//! latest wins), so a dragged orbit renders the newest pose, never a backlog.
//!
//! [`jobs_in_flight`](crate::preview_worker::jobs_in_flight) and [`wait_idle`](crate::preview_worker::wait_idle) let the agent and tests wait for the worker without
//! sleeping. `regia_render::pipeline_cache::pipelines_created_on_this_thread` is the counter the
//! tests read to check that a UI frame created no pipeline.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// A unit of work for the preview thread.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The preview thread's queue and its in-flight count.
struct Worker {
    /// The queue; `None` when the thread could not be spawned (jobs then run inline).
    sender: Mutex<Option<mpsc::Sender<Job>>>,
    /// Jobs queued or running, with a condition variable [`wait_idle`] waits on.
    in_flight: (Mutex<usize>, Condvar),
}

fn worker() -> &'static Worker {
    static WORKER: OnceLock<Worker> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("regia-preview".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let _span = tracing::info_span!(target: "regia_editor", "preview_worker_job").entered();
                    // A panicking job must not take the thread (and every later preview) down.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        tracing::error!(target: "regia_editor", "a preview job panicked");
                    }
                    finish_one();
                }
            });
        let sender = match spawned {
            Ok(_) => Some(sender),
            Err(err) => {
                tracing::error!(target: "regia_editor", "the preview thread did not start ({err}); previews render inline");
                None
            }
        };
        Worker {
            sender: Mutex::new(sender),
            in_flight: (Mutex::new(0), Condvar::new()),
        }
    })
}

fn finish_one() {
    let (count, changed) = &worker().in_flight;
    let mut n = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *n = n.saturating_sub(1);
    changed.notify_all();
}

/// Queues `job` on the preview thread (inline when the thread could not start).
fn run(job: Job) {
    let w = worker();
    {
        let mut n = w
            .in_flight
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *n += 1;
    }
    let sender = w
        .sender
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let job = match sender {
        Some(sender) => match sender.send(job) {
            Ok(()) => return,
            Err(mpsc::SendError(job)) => job,
        },
        None => job,
    };
    job();
    finish_one();
}

/// Preview jobs queued or running on the preview thread.
#[must_use]
pub fn jobs_in_flight() -> usize {
    *worker()
        .in_flight
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Blocks until the preview thread has no job queued or running (a job a finishing job queues,
/// the latest request of an [`AsyncPreview`], is waited for too). For the agent's captures and
/// tests; never call it from a UI frame.
pub fn wait_idle() {
    let (count, changed) = &worker().in_flight;
    let mut n = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    while *n > 0 {
        n = changed.wait(n).unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

/// A job an [`AsyncPreview`] holds until the running one ends.
type Pending<T> = (u64, Box<dyn FnOnce() -> T + Send + 'static>);

struct Slot<T> {
    /// The key of the job running now.
    running: Option<u64>,
    /// The job that runs next (the latest request made while one ran).
    queued: Option<Pending<T>>,
    /// The last finished job's key and output, until taken.
    done: Option<(u64, T)>,
    /// The key last requested (a request with it again is a no-op).
    requested: Option<u64>,
    /// Repainted when a job ends, so the UI takes the result without input.
    ctx: Option<egui::Context>,
    /// Jobs started by this preview.
    started: u64,
}

/// One preview's background render: the latest requested job runs on the preview thread and its
/// output waits for the UI ([`Self::take`]). Cloning shares the slot.
pub struct AsyncPreview<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T> Clone for AsyncPreview<T> {
    fn clone(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
        }
    }
}

impl<T> Default for AsyncPreview<T> {
    fn default() -> Self {
        Self {
            slot: Arc::new(Mutex::new(Slot {
                running: None,
                queued: None,
                done: None,
                requested: None,
                ctx: None,
                started: 0,
            })),
        }
    }
}

impl<T> std::fmt::Debug for AsyncPreview<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slot = self.lock();
        f.debug_struct("AsyncPreview")
            .field("running", &slot.running)
            .field("queued", &slot.queued.as_ref().map(|(k, _)| *k))
            .field("requested", &slot.requested)
            .field("started", &slot.started)
            .finish_non_exhaustive()
    }
}

impl<T> AsyncPreview<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slot<T>> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T: Send + 'static> AsyncPreview<T> {
    /// Asks for the output of `job` under `key`: a no-op when `key` was the last request;
    /// otherwise the job runs on the preview thread now, or after the running one (replacing a
    /// job still waiting). `ctx` is repainted when it ends.
    pub fn request(&self, key: u64, ctx: Option<&egui::Context>, job: impl FnOnce() -> T + Send + 'static) {
        let mut slot = self.lock();
        if slot.requested == Some(key) {
            return;
        }
        slot.requested = Some(key);
        if let Some(ctx) = ctx {
            slot.ctx = Some(ctx.clone());
        }
        if slot.running.is_some() {
            slot.queued = Some((key, Box::new(job)));
            return;
        }
        drop(slot);
        self.start(key, Box::new(job));
    }

    fn start(&self, key: u64, job: Box<dyn FnOnce() -> T + Send + 'static>) {
        {
            let mut slot = self.lock();
            slot.running = Some(key);
            slot.started += 1;
        }
        let this = self.clone();
        run(Box::new(move || {
            let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            let (next, ctx) = {
                let mut slot = this.lock();
                slot.running = None;
                match output {
                    Ok(output) => slot.done = Some((key, output)),
                    Err(_) => tracing::error!(target: "regia_editor", "a preview render panicked"),
                }
                (slot.queued.take(), slot.ctx.clone())
            };
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
            if let Some((key, job)) = next {
                this.start(key, job);
            }
        }));
    }

    /// The finished output not taken yet, with its key (the newest finished job's).
    pub fn take(&self) -> Option<(u64, T)> {
        self.lock().done.take()
    }

    /// Whether a job runs or waits.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        let slot = self.lock();
        slot.running.is_some() || slot.queued.is_some()
    }

    /// The key last requested.
    #[must_use]
    pub fn requested(&self) -> Option<u64> {
        self.lock().requested
    }

    /// Jobs this preview started (a request that only replaced a waiting job is not counted
    /// until it starts).
    #[must_use]
    pub fn jobs_started(&self) -> u64 {
        self.lock().started
    }

    /// Forgets the last request, so the same key renders again on the next request.
    pub fn forget(&self) {
        self.lock().requested = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn a_job_runs_off_the_calling_thread_and_its_output_is_taken_once() {
        let preview = AsyncPreview::<std::thread::ThreadId>::default();
        preview.request(1, None, || std::thread::current().id());
        wait_idle();
        let (key, thread) = preview.take().expect("finished");
        assert_eq!(key, 1);
        assert_ne!(
            thread,
            std::thread::current().id(),
            "the job ran on the preview thread"
        );
        assert!(preview.take().is_none(), "taken once");
        assert!(!preview.is_busy());
    }

    #[test]
    fn the_same_key_is_requested_once_and_the_latest_waiting_job_wins() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        let preview = AsyncPreview::<u64>::default();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let held = Arc::clone(&gate);
        // The first job holds the thread until the test lets it go.
        preview.request(1, None, move || {
            let (open, changed) = &*held;
            let mut open = open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*open {
                open = changed
                    .wait(open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            RAN.fetch_add(1, Ordering::SeqCst);
            1
        });
        preview.request(1, None, || unreachable!("the same key is not requested again"));
        preview.request(2, None, || {
            RAN.fetch_add(1, Ordering::SeqCst);
            2
        });
        preview.request(3, None, || {
            RAN.fetch_add(1, Ordering::SeqCst);
            3
        });
        assert!(preview.is_busy());
        {
            let (open, changed) = &*gate;
            *open.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            changed.notify_all();
        }
        wait_idle();
        assert_eq!(RAN.load(Ordering::SeqCst), 2, "job 2 was replaced by job 3");
        assert_eq!(preview.take(), Some((3, 3)), "the newest output");
        assert_eq!(preview.jobs_started(), 2);
        preview.forget();
        preview.request(3, None, || 33);
        wait_idle();
        assert_eq!(preview.take(), Some((3, 33)), "a forgotten key renders again");
    }
}
