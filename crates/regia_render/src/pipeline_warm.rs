// SPDX-License-Identifier: MPL-2.0
//! Background pipeline compilation in priority tiers (`docs/specs/13-rendering.md` § 2b).
//!
//! A [`PipelineWarmQueue`] runs warm jobs on worker threads, highest tier first: boot/branding,
//! menu, lobby, map(s), optional quality features. A job builds pipelines on the shared device —
//! typically by rendering a tiny representative frame with a throwaway [`crate::FrameRenderer`]
//! ([`frame_job`]), or by calling a subsystem's own prewarm. Every pipeline goes through the
//! process's persistent cache ([`crate::pipeline_cache`]), so when the frame path later creates
//! the same pipeline the driver finds it compiled: the frame does not wait for a compile it can
//! get from the cache, and material graphs it cannot draw yet fall back to the standard surface
//! (`graph_pipelines`).
//!
//! Rarely used pipeline variants (the forward set's blend modes and glass transmission passes)
//! are [`DeferredPipeline`]s. A warm ([`crate::FrameRenderer::warm_scene`] / `warm_extracted`,
//! on a worker or behind a loading screen) builds every variant a material of its frame can
//! draw (visible or not), side by side. The variants no loaded material uses are built by the
//! warm workers in the background once [`release_deferred`] is called after the first game
//! frame (the windowed runtime and [`RendererWarm::take`] call it). Mid-play a draw whose
//! variant is not built yet (a material spawned later) is skipped for that frame and its build
//! jumps the workers' queue ([`DeferredPipeline::try_get`]): the frame path never compiles one.
//! Without running warm workers they are built eagerly, as before.
//!
//! When the queue drains, the persistent cache is saved (off the main thread). Progress —
//! [`PipelineWarmProgress`] — is what the agent method `render.pipeline_warm` reports and what a
//! game's loading logic polls (`is_tier_ready`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use regia_app::prelude::*;

use crate::RenderDevice;

/// Warm-up priority. Lower tiers run first; a tier is ready when its jobs and every earlier
/// tier's jobs have finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WarmTier {
    /// The branding screen and the clear/overlay path (first present).
    Boot = 0,
    /// The main menu (UI, menu backdrop).
    Menu = 1,
    /// The lobby.
    Lobby = 2,
    /// The playable map(s): their frame configuration, material graphs, terrain, effects.
    Map = 3,
    /// Optional quality features (path tracer, high-end passes) no map needs on its first frame.
    Optional = 4,
}

impl WarmTier {
    /// Every tier in priority order.
    pub const ALL: [Self; 5] = [Self::Boot, Self::Menu, Self::Lobby, Self::Map, Self::Optional];

    /// Stable snake-case name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Menu => "menu",
            Self::Lobby => "lobby",
            Self::Map => "map",
            Self::Optional => "optional",
        }
    }

    /// The tier named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// A warm job: builds pipelines on the device. `Err` is reported in the progress, never fatal.
pub type WarmJob = Box<dyn FnOnce(&RenderDevice) -> Result<(), String> + Send>;

/// One tier's progress.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TierProgress {
    /// Tier name.
    pub tier: &'static str,
    /// Jobs queued for this tier so far.
    pub jobs_total: u32,
    /// Jobs finished (including failed).
    pub jobs_done: u32,
    /// Jobs that returned an error.
    pub jobs_failed: u32,
    /// Pipelines this tier's jobs created.
    pub pipelines: u64,
    /// Worker time spent on this tier's jobs.
    pub busy_ms: f32,
    /// Wall time from the queue's start until the tier (and every earlier one) finished.
    pub ready_after_ms: Option<f32>,
}

/// Progress of the whole warm-up (`render.pipeline_warm`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PipelineWarmProgress {
    /// Jobs finished.
    pub done: u32,
    /// Jobs queued.
    pub total: u32,
    /// Per tier, in priority order.
    pub tiers: Vec<TierProgress>,
    /// Pipelines created outside the queue's workers (frame path, level loads on the main
    /// thread). After warm-up a steady frame adds none.
    pub pipelines_outside: u64,
    /// Where those pipelines were created (`file:line` of the creation site, with a count),
    /// the first 32 distinct sites: what a game adds to its warm tiers.
    pub outside_sites: Vec<(String, u32)>,
    /// Workers are running (the device exists).
    pub started: bool,
    /// Last job error, if any.
    pub last_error: Option<String>,
    /// The persistent cache.
    pub cache: crate::pipeline_cache::PipelineCacheStatus,
    /// [`DeferredPipeline`]s waiting for a background build.
    pub deferred_pending: usize,
    /// Deferred pipelines the background built so far.
    pub deferred_built: u64,
    /// Whether [`release_deferred`] was called.
    pub deferred_released: bool,
}

impl PipelineWarmProgress {
    /// Whether `tier` and every earlier tier have finished every queued job.
    #[must_use]
    pub fn is_tier_ready(&self, tier: WarmTier) -> bool {
        self.tiers
            .iter()
            .take(tier as usize + 1)
            .all(|t| t.jobs_done == t.jobs_total)
    }

    /// Fraction of queued jobs finished (`1.0` with nothing queued).
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            1.0
        } else {
            self.done as f32 / self.total as f32
        }
    }
}

thread_local! {
    /// Tier of the warm job running on this thread.
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_TIER: std::cell::Cell<Option<WarmTier>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    /// The cancel state of the queue whose worker (or worker's [`join`] helper) this thread is.
    /// Read by [`warm_cancelled`] and before every pipeline creation ([`note_pipeline`]); unset
    /// on every other thread.
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static WARM_CANCEL: std::cell::RefCell<Option<Arc<Cancel>>> = const { std::cell::RefCell::new(None) };
}

/// A queue's cancel state, shared with its worker threads and their [`join`] helpers.
struct Cancel {
    /// Set by [`PipelineWarmQueue::shutdown`]: running jobs stop at their next check
    /// ([`warm_cancelled`]) and no new job starts.
    flag: AtomicBool,
    /// Where panics unwind, a running job also unwinds at its next pipeline creation
    /// ([`cancel_point`]). Off only in tests of the control-flow path (the one a
    /// `panic = "abort"` build has).
    #[cfg_attr(panic = "abort", allow(dead_code))]
    unwind: AtomicBool,
    /// Pipelines this queue's warm threads created (tests).
    created: AtomicU64,
    /// Of those, the ones created after `flag` was set: the compiles a cancelled job ran
    /// before it reached a check (tests).
    late: AtomicU64,
    /// Tests: a quit that lands at an exact pipeline of a running job ([`TestPause`]).
    #[cfg(test)]
    pause: TestPause,
}

/// What a warm job tells the test that armed its queue's [`TestPause`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseEvent {
    /// A warm thread is about to create the pipeline the test asked for, and waits for the test.
    Reached,
    /// The job is over: it returned, failed, panicked or was dropped unrun.
    JobEnded,
}

/// Tests: the warm thread about to create pipeline number `at` of its queue (1-based; 0 =
/// never) reports [`PauseEvent::Reached`], then waits while the test signals the shutdown, so the
/// cancel lands between two known pipelines without polling a clock (ADR-2585). The test waits on
/// channels, never on a rendezvous only the job can complete: a job that ends first reports that
/// instead, and the test dropping its end (it failed) lets the job go on (ADR-2681).
#[cfg(test)]
struct TestPause {
    at: AtomicU64,
    /// The warm thread's end of the channels: where to report, and what to wait on. Taken by the
    /// one thread that gets number `at`.
    meeting: Mutex<Option<TestPauseMeeting>>,
}

/// [`TestPause::meeting`].
#[cfg(test)]
type TestPauseMeeting = (std::sync::mpsc::Sender<PauseEvent>, std::sync::mpsc::Receiver<()>);

#[cfg(test)]
impl TestPause {
    /// Called with the 1-based number of the pipeline about to be created.
    fn meet(&self, created: u64) {
        if created != self.at.load(Ordering::Relaxed) {
            return;
        }
        let meeting = self
            .meeting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((events, resume)) = meeting {
            let _ = events.send(PauseEvent::Reached);
            // The test releases by dropping its sender, on purpose or by unwinding.
            let _ = resume.recv();
        }
    }
}

impl Cancel {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            unwind: AtomicBool::new(true),
            created: AtomicU64::new(0),
            late: AtomicU64::new(0),
            #[cfg(test)]
            pause: TestPause {
                at: AtomicU64::new(0),
                meeting: Mutex::new(None),
            },
        }
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

/// The error text of a warm job (or frame warm) that stopped because its queue shut down. A custom
/// job that stops early at a [`warm_cancelled`] check can return it (the queue reports any error
/// of a job that ends while it shuts down as this text anyway).
pub const CANCELLED_AT_SHUTDOWN: &str = "canceled at shutdown";

/// Whether the warm job running on this thread should stop now: its [`PipelineWarmQueue`] is
/// shutting down (the app is quitting). A pipeline compile cannot be interrupted, so a long
/// warm checks this between its compiles and returns early (with an `Err`), and the worker
/// exits after the compile in flight. Shipping builds (`panic = "abort"`) stop a warm only
/// this way.
///
/// The engine's own warms check it: frame warms ([`crate::FrameRenderer::warm_scene`],
/// `warm_extracted`, every frame they render, between its passes), `prewarm`, material-graph
/// settles and the overlay layers; the background builds of [`DeferredPipeline`]s stop between
/// variants (one per worker iteration). A custom
/// warm job that builds many pipelines itself (a loop over materials, several frame
/// configurations) should check it between them too.
///
/// Always `false` off warm worker threads: on the frame thread, in inline warms
/// ([`PipelineWarmQueue::run_inline`]) and before shutdown.
#[must_use]
pub fn warm_cancelled() -> bool {
    WARM_CANCEL.with(|c| c.borrow().as_ref().is_some_and(|cancel| cancel.is_set()))
}

/// `Err` once [`warm_cancelled`]: a frame warm stops here, between pipeline compiles. The
/// renderer is left mid-frame (the warm is being abandoned at exit); the frame's segments
/// already on encode workers are finished or waited for when the early return drops them
/// (`parallel_finish::Segments`), so no encoder outlives the cancelled job.
pub(crate) fn warm_checkpoint() -> Result<(), crate::RenderError> {
    if warm_cancelled() {
        Err(crate::RenderError::Encode(CANCELLED_AT_SHUTDOWN.into()))
    } else {
        Ok(())
    }
}

/// Unwind payload that ends a warm job at its next pipeline creation once its queue shuts down
/// ([`cancel_point`]). Raised with [`std::panic::resume_unwind`], so no panic hook (log, crash
/// report) runs; [`run_job`] and [`run_deferred`] catch it.
#[cfg_attr(panic = "abort", allow(dead_code))]
struct WarmCancelled;

/// Called before every pipeline creation on a warm thread. Once its queue is shutting down it
/// counts the creation as late and, where panics unwind, ends the running warm job here: the
/// pipeline about to be created is not compiled, and the worker returns after unwinding out of
/// the job. Only warm worker threads (and their [`join`] helpers) carry a cancel state; on
/// every other thread this is a no-op.
///
/// Under `panic = "abort"` (the shipping profile) an unwind would abort the process, so there
/// the job stops only at its next [`warm_cancelled`] check (the engine's warms check between
/// their compiles); shutdown waits two seconds for it, then the exit hook joins it.
fn cancel_point() {
    let cancel = WARM_CANCEL.with(|c| c.borrow().clone());
    let Some(cancel) = cancel else {
        return;
    };
    let created = cancel.created.fetch_add(1, Ordering::Relaxed) + 1;
    #[cfg(test)]
    cancel.pause.meet(created);
    #[cfg(not(test))]
    let _ = created;
    if !cancel.is_set() {
        return;
    }
    #[cfg(panic = "unwind")]
    if cancel.unwind.load(Ordering::Relaxed) {
        std::panic::resume_unwind(Box::new(WarmCancelled));
    }
    cancel.late.fetch_add(1, Ordering::Relaxed);
}

/// Whether this thread runs a warm job or is a background pipeline builder (a warm worker, a
/// material-graph worker): its pipelines are off the frame path (ADR-7885, the after-warm-up
/// report's `warm_worker`).
pub(crate) fn on_warm_thread() -> bool {
    CURRENT_TIER.with(std::cell::Cell::get).is_some()
}

/// Marks this thread as a background pipeline builder of `tier` for its lifetime: the engine's
/// material-graph workers (`graph_pipelines`), which build graph sets off the frame thread while
/// the frame draws the standard surface. Their pipelines count for `tier`
/// ([`PipelineWarmProgress::tiers`]), not as created outside the warm queue (frame path, main
/// thread loads), and the thread may wait for deferred compiles ([`may_block`]).
pub(crate) fn mark_background_builder(tier: WarmTier) {
    CURRENT_TIER.with(|c| c.set(Some(tier)));
}

/// A background builder ([`mark_background_builder`]) finished a job: the time until its next
/// creation (an idle wait) is not charged to the job's last creation site.
pub(crate) fn end_background_job() {
    close_site(Instant::now());
}

static TIER_PIPELINES: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static OUTSIDE_PIPELINES: AtomicU64 = AtomicU64::new(0);
static OUTSIDE_SITES: Mutex<Vec<(&'static std::panic::Location<'static>, u32)>> = Mutex::new(Vec::new());

/// Attributes one pipeline creation to the running warm job's tier (or to "outside", with the
/// creation site). Called by [`crate::pipeline_cache::cache_for`] at every creation site.
pub(crate) fn note_pipeline(site: &'static std::panic::Location<'static>) {
    if let Some(tier) = CURRENT_TIER.with(std::cell::Cell::get) {
        // A shutdown stops the job between compiles: the one in flight finishes, no new one
        // starts (unwinding builds; elsewhere the job's next `warm_cancelled` check stops it).
        cancel_point();
        TIER_PIPELINES[tier as usize].fetch_add(1, Ordering::Relaxed);
        JOB_PIPELINES.with(|n| n.set(n.get() + 1));
        let now = Instant::now();
        close_site(now);
        LAST_SITE.with(|c| c.set(Some((now, site))));
        return;
    }
    OUTSIDE_PIPELINES.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(target: "regia_render", "pipeline created outside the warm queue at {site}");
    if let Ok(mut sites) = OUTSIDE_SITES.lock() {
        if let Some(entry) = sites.iter_mut().find(|(s, _)| *s == site) {
            entry.1 += 1;
        } else if sites.len() < 32 {
            sites.push((site, 1));
        }
    }
}

thread_local! {
    /// The last creation site this warm thread passed and when (per-site cost attribution).
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static LAST_SITE: std::cell::Cell<Option<(Instant, &'static std::panic::Location<'static>)>> =
        const { std::cell::Cell::new(None) };
}

/// Per creation site: pipelines created on warm workers, and the worker time from each
/// creation site to the next one (the compile dominates it on a cold driver).
static SITE_COST: Mutex<Vec<(&'static std::panic::Location<'static>, u32, f64, f64)>> =
    Mutex::new(Vec::new());

/// Charges the time since this thread's last creation site to that site.
fn close_site(now: Instant) {
    let Some((at, site)) = LAST_SITE.with(std::cell::Cell::take) else {
        return;
    };
    let ms = now.duration_since(at).as_secs_f64() * 1000.0;
    if let Ok(mut sites) = SITE_COST.lock() {
        if let Some(entry) = sites.iter_mut().find(|(s, ..)| *s == site) {
            entry.1 += 1;
            entry.2 += ms;
            entry.3 = entry.3.max(ms);
        } else if sites.len() < 512 {
            sites.push((site, 1, ms, ms));
        }
    }
}

/// Runs `a` on this thread and `b` on a scoped helper thread at the same time, and returns both
/// results: independent pipeline compiles (the driver compiles each on its calling thread) then
/// take the longer one's time instead of the sum. The helper inherits this thread's warm tier,
/// and its pipelines count for this thread's warm job and frame-path counters, exactly as if
/// they had been created here. A panic in `b` resumes on this thread.
pub fn join<A: Send, B: Send>(a: impl FnOnce() -> A + Send, b: impl FnOnce() -> B + Send) -> (A, B) {
    let tier = CURRENT_TIER.with(std::cell::Cell::get);
    let cancel = WARM_CANCEL.with(|c| c.borrow().clone());
    let axes = crate::frame_stats::current_key_axes();
    let (ra, (rb, job, creates)) = std::thread::scope(|s| {
        let helper = s.spawn(move || {
            CURRENT_TIER.with(|c| c.set(tier));
            // The helper's pipelines belong to the same forward set (ADR-7885).
            let _axes = crate::frame_stats::key_axes_scope(axes);
            WARM_CANCEL.with(|c| *c.borrow_mut() = cancel);
            let job0 = JOB_PIPELINES.with(std::cell::Cell::get);
            let creates0 = crate::frame_stats::thread_pipeline_creates();
            let rb = b();
            close_site(Instant::now());
            (
                rb,
                JOB_PIPELINES.with(std::cell::Cell::get) - job0,
                crate::frame_stats::thread_pipeline_creates() - creates0,
            )
        });
        let ra = a();
        match helper.join() {
            Ok(rb) => (ra, rb),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
    JOB_PIPELINES.with(|n| n.set(n.get() + job));
    crate::frame_stats::adopt_pipeline_creates(creates);
    (ra, rb)
}

/// [`join`] for any number of jobs: the first runs on this thread, the others on scoped helper
/// threads, all at once. Results come back in `jobs` order.
pub fn join_all<T: Send, F: FnOnce() -> T + Send>(jobs: Vec<F>) -> Vec<T> {
    let mut jobs = jobs.into_iter();
    let Some(first) = jobs.next() else {
        return Vec::new();
    };
    let rest: Vec<F> = jobs.collect();
    if rest.is_empty() {
        return vec![first()];
    }
    let (head, tail) = join(first, move || {
        // Each level runs one job and hands the rest to a helper: n jobs on n threads at once.
        join_all(rest)
    });
    let mut out = Vec::with_capacity(tail.len() + 1);
    out.push(head);
    out.extend(tail);
    out
}

/// One pipeline creation site's warm cost ([`warm_site_costs`]).
#[derive(Clone, Debug, PartialEq)]
pub struct WarmSiteCost {
    /// `file:line` of the creation site.
    pub site: String,
    /// Pipelines that site created on warm workers.
    pub pipelines: u32,
    /// Worker time from each of its creations to the thread's next creation (or job end).
    pub total_ms: f32,
    /// The slowest single creation.
    pub max_ms: f32,
}

/// The `n` costliest pipeline creation sites of the warm-up so far, slowest first: where a
/// cold warm spends its time (driver compiles), for deciding what to defer or split.
#[must_use]
pub fn warm_site_costs(n: usize) -> Vec<WarmSiteCost> {
    let mut sites: Vec<WarmSiteCost> = SITE_COST
        .lock()
        .map(|sites| {
            sites
                .iter()
                .map(|(site, count, total, max)| WarmSiteCost {
                    site: site.to_string(),
                    pipelines: *count,
                    total_ms: *total as f32,
                    max_ms: *max as f32,
                })
                .collect()
        })
        .unwrap_or_default();
    sites.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
    sites.truncate(n);
    sites
}

// ---------------------------------------------------------------------------------------------
// Deferred pipelines.
// ---------------------------------------------------------------------------------------------

struct DeferredInner<P> {
    cell: std::sync::OnceLock<P>,
    build: Box<dyn Fn() -> P + Send + Sync>,
    label: &'static str,
    /// A frame needed it before it was built: queued at the front, ahead of the release.
    urgent: std::sync::atomic::AtomicBool,
}

/// A pipeline variant built on first use, or by a warm worker in the background after
/// [`release_deferred`] (see the module docs). Cloning shares the pipeline.
pub struct DeferredPipeline<P>(Arc<DeferredInner<P>>);

impl<P> Clone for DeferredPipeline<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<P> std::fmt::Debug for DeferredPipeline<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredPipeline")
            .field("label", &self.0.label)
            .field("built", &self.0.cell.get().is_some())
            .finish()
    }
}

/// One registered background build (a weak handle to a [`DeferredPipeline`]).
trait DeferredBuild: Send + Sync {
    /// Builds it if still wanted and unbuilt; `true` when this call built it.
    fn build(&self) -> bool;
    /// Whether the pipeline still exists and is unbuilt.
    fn wanted(&self) -> bool;
}

impl<P: Send + Sync + 'static> DeferredBuild for std::sync::Weak<DeferredInner<P>> {
    fn build(&self) -> bool {
        let Some(inner) = self.upgrade() else {
            return false;
        };
        let mut built = false;
        inner.cell.get_or_init(|| {
            built = true;
            (inner.build)()
        });
        built
    }

    fn wanted(&self) -> bool {
        self.upgrade().is_some_and(|inner| inner.cell.get().is_none())
    }
}

/// Background builds waiting for [`release_deferred`]: weak, so a renderer dropped meanwhile
/// (and its device) is not kept alive by the queue.
static DEFERRED: Mutex<VecDeque<Box<dyn DeferredBuild>>> = Mutex::new(VecDeque::new());
/// Builds a frame asked for ([`DeferredPipeline::try_get`]): taken first, released or not.
static URGENT: Mutex<VecDeque<Box<dyn DeferredBuild>>> = Mutex::new(VecDeque::new());
/// Set by [`release_deferred`]: warm workers may build deferred pipelines.
static DEFERRED_RELEASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Deferred builds running on a worker right now.
static DEFERRED_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
/// Deferred pipelines built in the background so far.
static DEFERRED_BUILT: AtomicU64 = AtomicU64::new(0);
/// Started queues that are not shut down: woken when deferred work is released, asked whether
/// warm workers exist, shut down by the exit hook. The registry holds each queue's [`Hub`],
/// never the queue itself, so asking (every frame does) never holds a queue a `shutdown` is
/// releasing.
static QUEUES: Mutex<Vec<Arc<Hub>>> = Mutex::new(Vec::new());

/// What [`QUEUES`] keeps of a queue.
struct Hub {
    /// The workers wait on it (with the queue's state lock).
    wake: Condvar,
    /// Started with workers, and not shut down.
    available: AtomicBool,
    /// The queue, for the exit hook.
    queue: std::sync::Weak<Shared>,
}

/// Removes `hub` from [`QUEUES`].
fn unregister(hub: &Arc<Hub>) {
    QUEUES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|h| !Arc::ptr_eq(h, hub));
}

impl<P: Send + Sync + 'static> DeferredPipeline<P> {
    /// A variant that `build` creates on first [`Self::get`], or in the background once released.
    ///
    /// Without running warm workers (tools, tests, a host without a [`PipelineWarmQueue`]) it is
    /// built right here, as an eager pipeline would be: nothing could build it later off the
    /// frame path.
    pub fn new(label: &'static str, build: impl Fn() -> P + Send + Sync + 'static) -> Self {
        let inner = Arc::new(DeferredInner {
            cell: std::sync::OnceLock::new(),
            build: Box::new(build),
            label,
            urgent: std::sync::atomic::AtomicBool::new(false),
        });
        if !background_available() {
            inner.cell.get_or_init(|| (inner.build)());
            return Self(inner);
        }
        if let Ok(mut queue) = DEFERRED.lock() {
            if queue.len() >= 64 {
                queue.retain(|d| d.wanted());
            }
            queue.push_back(Box::new(Arc::downgrade(&inner)));
        }
        wake_queues();
        Self(inner)
    }

    /// A variant built only when a frame first asks for it ([`Self::get`], [`Self::try_get`]):
    /// never eagerly and never queued for the background warm, so a rarely drawn variant (the
    /// x-ray render override) costs no compile until something draws with it. Mid-play the first
    /// frame that needs it skips the draw while a warm worker builds it, like any deferred
    /// variant.
    pub fn on_demand(label: &'static str, build: impl Fn() -> P + Send + Sync + 'static) -> Self {
        Self(Arc::new(DeferredInner {
            cell: std::sync::OnceLock::new(),
            build: Box::new(build),
            label,
            urgent: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// The pipeline, built now if neither a frame nor the background built it yet (another
    /// thread building it is waited for, never duplicated).
    pub fn get(&self) -> &P {
        self.0.cell.get_or_init(|| (self.0.build)())
    }

    /// The pipeline if it is built, or built now where this thread may wait for a compile
    /// ([`may_block`]: a warm job, a [`blocking_scope`], or no warm workers). Otherwise `None`:
    /// the caller skips the draw this frame, and the build moves to the front of the warm
    /// workers' queue (it starts at once, released or not). The frame path never compiles.
    pub fn try_get(&self) -> Option<&P> {
        if let Some(p) = self.0.cell.get() {
            return Some(p);
        }
        if may_block() {
            return Some(self.get());
        }
        if !self.0.urgent.swap(true, Ordering::AcqRel)
            && let Ok(mut urgent) = URGENT.lock()
        {
            urgent.push_back(Box::new(Arc::downgrade(&self.0)));
            tracing::debug!(target: "regia_render", "deferred pipeline {} needed mid-frame; built in the background", self.0.label);
        }
        notify_queues();
        None
    }

    /// Whether it has been built.
    #[must_use]
    pub fn is_built(&self) -> bool {
        self.0.cell.get().is_some()
    }

    /// Whether `self` and `other` are the same pipeline.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

thread_local! {
    /// Open [`blocking_scope`]s on this thread.
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static BLOCKING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// While alive, this thread may wait for deferred pipeline compiles
/// ([`DeferredPipeline::try_get`] builds instead of skipping): a loading screen, a synchronous
/// warm ([`crate::FrameRenderer::warm_scene`] opens one), a capture that must be complete.
#[must_use = "the scope ends when dropped"]
pub struct BlockingScope(());

impl Drop for BlockingScope {
    fn drop(&mut self) {
        BLOCKING.with(|n| n.set(n.get().saturating_sub(1)));
    }
}

/// Opens a [`BlockingScope`] on this thread.
pub fn blocking_scope() -> BlockingScope {
    BLOCKING.with(|n| n.set(n.get() + 1));
    BlockingScope(())
}

/// Whether this thread may wait for a deferred compile: it runs a warm job, holds a
/// [`blocking_scope`], or no warm workers exist to build it elsewhere.
#[must_use]
pub fn may_block() -> bool {
    CURRENT_TIER.with(std::cell::Cell::get).is_some()
        || BLOCKING.with(std::cell::Cell::get) > 0
        || !background_available()
}

/// Whether this thread runs a warm job or holds a [`blocking_scope`]: the frame load budget
/// (`load_budget`, ADR-7884) never defers work there, since a warm must cover the whole frame.
pub(crate) fn in_warm_or_blocking_scope() -> bool {
    CURRENT_TIER.with(std::cell::Cell::get).is_some() || BLOCKING.with(std::cell::Cell::get) > 0
}

/// Whether a started warm queue with workers exists to build deferred pipelines.
fn background_available() -> bool {
    // The frame path asks every frame: one lock and a flag per queue, no queue handle.
    QUEUES
        .lock()
        .is_ok_and(|queues| queues.iter().any(|hub| hub.available.load(Ordering::Acquire)))
}

/// Lets the warm workers build the [`DeferredPipeline`]s nothing has drawn yet, in the
/// background. Call after the first game frame (the windowed runtime does; so does
/// [`RendererWarm::take`]). Idempotent.
pub fn release_deferred() {
    DEFERRED_RELEASED.store(true, Ordering::Release);
    wake_queues();
}

/// Deferred pipelines still waiting for (or in) a background build.
#[must_use]
pub fn deferred_pending() -> usize {
    let queued = DEFERRED
        .lock()
        .map_or(0, |q| q.iter().filter(|d| d.wanted()).count());
    let urgent = URGENT
        .lock()
        .map_or(0, |q| q.iter().filter(|d| d.wanted()).count());
    queued + urgent + DEFERRED_IN_FLIGHT.load(Ordering::Acquire) as usize
}

fn wake_queues() {
    if DEFERRED_RELEASED.load(Ordering::Acquire) {
        notify_queues();
    }
}

fn notify_queues() {
    if let Ok(queues) = QUEUES.lock() {
        for hub in queues.iter() {
            hub.wake.notify_all();
        }
    }
}

/// The next deferred build, once released.
fn take_deferred() -> Option<Box<dyn DeferredBuild>> {
    if let Ok(mut urgent) = URGENT.lock() {
        while let Some(next) = urgent.pop_front() {
            if next.wanted() {
                DEFERRED_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
                return Some(next);
            }
        }
    }
    if !DEFERRED_RELEASED.load(Ordering::Acquire) {
        return None;
    }
    let mut queue = DEFERRED.lock().ok()?;
    while let Some(next) = queue.pop_front() {
        if next.wanted() {
            DEFERRED_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
            return Some(next);
        }
    }
    None
}

/// Runs one deferred build on a warm worker (Optional tier); saves the cache when the last one
/// built something.
fn run_deferred(shared: &Arc<Shared>, build: &dyn DeferredBuild) {
    let _span = tracing::info_span!("pipeline_warm_deferred").entered();
    CURRENT_TIER.with(|c| c.set(Some(WarmTier::Optional)));
    // A shutdown mid-build unwinds out with `WarmCancelled` (unbuilt; a frame builds it on use;
    // unwinding builds only, see `cancel_point`). Under the shipping profile's `panic = "abort"` a
    // build that panics ends the process instead of returning unbuilt (audit v034 net L6).
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build.build())).unwrap_or(false);
    close_site(Instant::now());
    CURRENT_TIER.with(|c| c.set(None));
    DEFERRED_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    if built {
        DEFERRED_BUILT.fetch_add(1, Ordering::Relaxed);
        shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deferred_dirty = true;
    }
    let drained = deferred_pending() == 0;
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let idle = state.pending.iter().all(VecDeque::is_empty) && state.in_flight.iter().all(|&n| n == 0);
    // Not while shutting down: the exit path saves on its own thread, and the worker must not
    // hold the device (or race that save) any longer than its last compile.
    let save = drained && idle && state.deferred_dirty && shared.save_cache && !state.shutdown;
    if save {
        state.deferred_dirty = false;
    }
    drop(state);
    shared.done.notify_all();
    if save && let Some(Err(err)) = crate::pipeline_cache::save_installed() {
        tracing::warn!(target: "regia_render", "pipeline cache save failed: {err}");
    }
}

#[derive(Default)]
struct TierStats {
    total: u32,
    done: u32,
    failed: u32,
    busy: Duration,
    ready_after: Option<Duration>,
}

struct Queued {
    id: u64,
    label: String,
    job: WarmJob,
    queued_at: Instant,
}

/// A job a worker is running (for [`PipelineWarmQueue::describe`]).
struct Running {
    id: u64,
    tier: WarmTier,
    label: String,
    since: Instant,
}

#[derive(Default)]
struct State {
    pending: [VecDeque<Queued>; 5],
    in_flight: [u32; 5],
    stats: [TierStats; 5],
    device: Option<RenderDevice>,
    started_at: Option<Instant>,
    workers: usize,
    /// Workers wanted ([`PipelineWarmQueue::set_threads`]); one above this retires after its job.
    target: usize,
    /// Workers spawned so far (thread names).
    spawned: usize,
    last_error: Option<String>,
    shutdown: bool,
    /// The renderer queues its own boot-tier job when the device starts.
    no_engine_jobs: bool,
    /// Load sets whose `Pipeline` items are warm job labels.
    load_sets: Option<regia_asset::LoadSets>,
    /// Pipelines created since the last cache save (saved when the queue drains).
    dirty: bool,
    /// Deferred pipelines built since the last cache save.
    deferred_dirty: bool,
    /// Renderers [`frame_job_handoff`] jobs warmed, for [`crate::RuntimeRenderer`] to adopt
    /// their pipelines ([`PipelineWarmQueue::take_warmed_renderer`]).
    warmed_renderers: VecDeque<crate::FrameRenderer>,
    /// Id of the next queued job.
    next_id: u64,
    /// Jobs running on a worker (or inline) now.
    running: Vec<Running>,
}

struct Shared {
    state: Mutex<State>,
    /// This queue's [`QUEUES`] entry; the workers wait on its condvar.
    hub: Arc<Hub>,
    /// Workers inside a job now (taken under the state lock, so none starts one after the
    /// shutdown flag). [`shutdown_shared`] joins idle workers without a time bound.
    busy: AtomicUsize,
    /// Signalled whenever a job finishes (tier waits).
    done: Condvar,
    save_cache: bool,
    /// Workers call [`regia_platform::background::lower_current_thread_priority`] as they
    /// start. Set by [`PipelineWarmQueue::set_background_priority`] before [`PipelineWarmQueue::start`]
    /// (the install prepare step). Play-time queues leave it false.
    background: AtomicBool,
    /// Set by the first [`PipelineWarmQueue::note_scene_frame`]: the queue switched to Play once.
    scene_seen: AtomicBool,
    /// Running workers, joined by [`PipelineWarmQueue::shutdown`].
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Set by [`PipelineWarmQueue::shutdown`]: a running job stops at its next check
    /// ([`warm_cancelled`]), or where panics unwind before its next pipeline compile
    /// ([`cancel_point`]).
    cancel: Arc<Cancel>,
    /// Tests: how long a worker sleeps between seeing the shutdown and returning (a worker the
    /// scheduler leaves off-CPU under load, ms).
    #[cfg(test)]
    exit_delay_ms: AtomicU64,
    /// Tests: [`PipelineWarmQueue::isolated`].
    #[cfg(test)]
    isolated: AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // A queue whose workers never started (or never shut down) leaves the registry here.
        unregister(&self.hub);
    }
}

/// How long [`PipelineWarmQueue::shutdown`] waits for a worker to finish its current job (was
/// 2 s; ADR-276 extends ADR-269's wait to the warm workers). A compile cannot be interrupted and
/// a worker left inside the NVIDIA shader compiler at process exit crashes the exit, so this only
/// bounds a driver that hangs: the wait ends when the compile does. Once the device is lost it is
/// [`crate::gpu_health::DEVICE_LOST_WORKER_WAIT`]. [`SHUTDOWN_WAIT_ENV`] overrides it.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(30);

/// Overrides [`SHUTDOWN_WAIT`] (milliseconds).
const SHUTDOWN_WAIT_ENV: &str = "REGIA_WARM_SHUTDOWN_WAIT_MS";

/// [`SHUTDOWN_WAIT`] or its override, short once the device is lost.
fn shutdown_wait() -> Duration {
    crate::gpu_health::worker_wait(parse_shutdown_wait(
        regia_core::brand::env_var(SHUTDOWN_WAIT_ENV).ok().as_deref(),
    ))
}

fn parse_shutdown_wait(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(SHUTDOWN_WAIT, Duration::from_millis)
}

/// The warm-up queue. Cloning shares the queue.
#[derive(Resource, Clone)]
pub struct PipelineWarmQueue {
    shared: Arc<Shared>,
}

/// How many warm workers a [`PipelineWarmQueue`] runs ([`PipelineWarmQueue::set_mode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmMode {
    /// Before the menu, while the host boots or prepares: nothing else needs the cores, so the
    /// queue takes all but two of them ([`boot_threads`]).
    Boot,
    /// While the game plays: a quarter of the cores, 1..=4 ([`default_threads`]); the frame
    /// thread and the asset workers keep the rest.
    Play,
}

impl WarmMode {
    /// The worker count of this mode on this machine.
    #[must_use]
    pub fn threads(self) -> usize {
        match self {
            Self::Boot => boot_threads(),
            Self::Play => default_threads(),
        }
    }
}

impl Default for PipelineWarmQueue {
    /// A queue in [`WarmMode::Boot`]; the first game frame ([`GameFrameWarm`]) or the host
    /// ([`PipelineWarmQueue::set_mode`]) switches it to [`WarmMode::Play`].
    fn default() -> Self {
        Self::new(boot_threads())
    }
}

/// Play-time worker count: a quarter of the cores, 1..=4 (pipeline compiles are CPU-heavy in
/// the driver; the main thread and the asset workers keep the rest). [`WarmMode::Play`].
#[must_use]
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 4).clamp(1, 4))
}

/// Pre-menu worker count: all cores but two, 1..=12. [`WarmMode::Boot`].
///
/// Capped at 4 on Android and on machines with under 8 GiB of RAM (the physical RAM the
/// platform probe reports on Linux, Windows and macOS): N concurrent driver compiles cost
/// memory and heat there.
#[must_use]
pub fn boot_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    boot_threads_for(
        cores,
        cfg!(target_os = "android"),
        crate::low_memory::system_ram_mib(),
    )
}

/// [`boot_threads`] for a machine with `cores` hardware threads and `ram_mib` of RAM (`None`
/// when unknown, which does not cap).
fn boot_threads_for(cores: usize, android: bool, ram_mib: Option<u64>) -> usize {
    let low_ram = ram_mib.is_some_and(|mib| mib < 8 * 1024);
    let cap = if android || low_ram { 4 } else { 12 };
    cores.saturating_sub(2).clamp(1, cap)
}

impl PipelineWarmQueue {
    /// A queue with `threads` workers (started by [`Self::start`]).
    #[must_use]
    pub fn new(threads: usize) -> Self {
        Self {
            shared: Arc::new_cyclic(|queue| Shared {
                state: Mutex::new(State {
                    target: threads.max(1),
                    ..State::default()
                }),
                hub: Arc::new(Hub {
                    wake: Condvar::new(),
                    available: AtomicBool::new(false),
                    queue: queue.clone(),
                }),
                busy: AtomicUsize::new(0),
                done: Condvar::new(),
                save_cache: true,
                background: AtomicBool::new(false),
                scene_seen: AtomicBool::new(false),
                handles: Mutex::new(Vec::new()),
                cancel: Arc::new(Cancel::new()),
                #[cfg(test)]
                exit_delay_ms: AtomicU64::new(0),
                #[cfg(test)]
                isolated: AtomicBool::new(false),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues `job` in `tier`. Jobs queued before [`Self::start`] wait for the device.
    pub fn push(
        &self,
        tier: WarmTier,
        label: impl Into<String>,
        job: impl FnOnce(&RenderDevice) -> Result<(), String> + Send + 'static,
    ) {
        self.push_job(tier, label.into(), Box::new(job));
    }

    /// [`Self::push`], returning the job's id ([`Self::cancel_queued`]).
    fn push_job(&self, tier: WarmTier, label: String, job: WarmJob) -> u64 {
        let mut state = self.lock();
        let id = state.next_id;
        state.next_id += 1;
        state.pending[tier as usize].push_back(Queued {
            id,
            label,
            job,
            queued_at: Instant::now(),
        });
        state.stats[tier as usize].total += 1;
        state.stats[tier as usize].ready_after = None;
        drop(state);
        self.shared.hub.wake.notify_one();
        id
    }

    /// Removes job `id` if no worker has taken it yet: it counts as a failed job ("cancelled")
    /// and its closure is dropped unrun (a [`Self::warm_renderer`] job hands its renderer back
    /// through its handle). Returns whether the job was removed.
    pub(crate) fn cancel_queued(&self, id: u64) -> bool {
        let mut state = self.lock();
        let found = state
            .pending
            .iter()
            .enumerate()
            .find_map(|(tier, queue)| queue.iter().position(|q| q.id == id).map(|index| (tier, index)));
        let Some((tier, index)) = found else {
            return false;
        };
        let Some(queued) = state.pending[tier].remove(index) else {
            return false;
        };
        let stats = &mut state.stats[tier];
        stats.done += 1;
        stats.failed += 1;
        state.last_error = Some(format!("{}: canceled", queued.label));
        note_ready_tiers(&mut state);
        drop(state);
        self.shared.done.notify_all();
        // The closure (and a warm job's return guard) drops outside the queue lock.
        drop(queued);
        true
    }

    /// The queue's state in one line for logs: done/total, then each running and pending job
    /// with its tier and age (`running [map] game frame 16.2 s; pending [map] x 3.0 s`).
    #[must_use]
    pub fn describe(&self) -> String {
        let state = self.lock();
        let now = Instant::now();
        let done: u32 = state.stats.iter().map(|s| s.done).sum();
        let total: u32 = state.stats.iter().map(|s| s.total).sum();
        let mut out = format!("done {done}/{total}, {} workers", state.workers);
        if state.device.is_none() {
            out.push_str(", not started");
        }
        for job in &state.running {
            out.push_str(&format!(
                "; running [{}] {} (#{}) {:.1} s",
                job.tier.name(),
                job.label,
                job.id,
                (now - job.since).as_secs_f32()
            ));
        }
        for (tier, queue) in WarmTier::ALL.iter().zip(&state.pending) {
            for job in queue {
                out.push_str(&format!(
                    "; pending [{}] {} (#{}) {:.1} s",
                    tier.name(),
                    job.label,
                    job.id,
                    (now - job.queued_at).as_secs_f32()
                ));
            }
        }
        out
    }

    /// The [`RenderDevice::device_id`] the workers run on (`None` before [`Self::start`]).
    #[must_use]
    pub fn device_id(&self) -> Option<u64> {
        self.lock().device.as_ref().map(RenderDevice::device_id)
    }

    /// Starts the workers on `device` (idempotent; the first device wins).
    pub fn start(&self, device: &RenderDevice) {
        let mut state = self.lock();
        if state.device.is_some() || state.shutdown {
            return;
        }
        state.device = Some(device.clone());
        state.started_at = Some(Instant::now());
        register_exit_hook();
        let to_spawn = state.target.saturating_sub(state.workers);
        state.workers += to_spawn;
        drop(state);
        QUEUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.shared.hub.clone());
        self.spawn_workers(to_spawn);
        let state = self.lock();
        if state.workers == 0 {
            tracing::warn!(target: "regia_render", "no pipeline warm workers; jobs run on the caller's thread");
        }
        // Under the state lock: a concurrent `shutdown` clears it after this.
        self.shared.hub.available.store(
            state.workers > 0 && !state.shutdown && builds_deferred(&self.shared),
            Ordering::Release,
        );
    }

    /// Spawns `count` workers (already counted in `workers`).
    fn spawn_workers(&self, count: usize) {
        for _ in 0..count {
            let i = {
                let mut state = self.lock();
                state.spawned += 1;
                state.spawned - 1
            };
            let shared = self.shared.clone();
            // Registered (ADR-287): the end of the process waits for a worker in a compile, and
            // its stop shuts the queue down when no clean exit did.
            let weak = Arc::downgrade(&self.shared);
            let spawned = regia_platform::GpuWorkerBuilder::new(format!("regia-pipeline-warm-{i}"))
                .on_stop(move || {
                    if let Some(shared) = weak.upgrade() {
                        signal_shutdown(&shared);
                    }
                })
                .spawn(move |gpu| worker(&shared, gpu));
            match spawned {
                Ok(handle) => {
                    self.shared
                        .handles
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(handle);
                    // A `shutdown` between the caller's check and the push joined the handles
                    // without this one; the worker saw the flag and exits at once.
                    if self.lock().shutdown {
                        let late: Vec<_> = std::mem::take(
                            &mut *self
                                .shared
                                .handles
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        for handle in late {
                            let _ = handle.join();
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "regia_render", "pipeline warm worker {i} did not start: {err}");
                    self.lock().workers -= 1;
                }
            }
        }
    }

    /// Sets the worker count (at least 1) without touching queued jobs. Growing spawns workers
    /// at once (once [`Self::start`] ran; before it, [`Self::start`] spawns the new count);
    /// shrinking lets the extra workers retire after the job each is running. A no-op after
    /// [`Self::shutdown`].
    pub fn set_threads(&self, threads: usize) {
        let threads = threads.max(1);
        let mut state = self.lock();
        state.target = threads;
        if state.shutdown || state.device.is_none() {
            return;
        }
        let grow = threads.saturating_sub(state.workers);
        state.workers += grow;
        drop(state);
        // Prune finished handles of retired workers.
        {
            let mut handles = self
                .shared
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (done, live): (Vec<_>, Vec<_>) = std::mem::take(&mut *handles)
                .into_iter()
                .partition(std::thread::JoinHandle::is_finished);
            *handles = live;
            drop(handles);
            for handle in done {
                let _ = handle.join();
            }
        }
        self.spawn_workers(grow);
        // Idle extra workers wake to retire.
        self.shared.hub.wake.notify_all();
    }

    /// [`Self::set_threads`] with the count of `mode`. A host's `--prepare` and boot leave the
    /// queue in [`WarmMode::Boot`] (the default); the first game frame switches it to
    /// [`WarmMode::Play`].
    pub fn set_mode(&self, mode: WarmMode) {
        self.set_threads(mode.threads());
    }

    /// When `enabled`, each worker lowers its priority as it starts
    /// ([`regia_platform::background::lower_current_thread_priority`]: nice 19 on Linux and
    /// Android, background mode on Windows). Call before [`Self::start`]. The install prepare
    /// step turns this on so a launcher's shader warm yields to the desktop. A playing game
    /// leaves it off.
    pub fn set_background_priority(&self, enabled: bool) {
        self.shared.background.store(enabled, Ordering::Release);
    }

    /// A scene frame was drawn (window, offscreen texture view or headless host): the first call
    /// switches the queue to [`WarmMode::Play`], later calls do nothing, so a host's own
    /// `set_mode(Boot)` around a later loading screen is not undone. Cheap (one atomic).
    pub fn note_scene_frame(&self) {
        if !self.shared.scene_seen.swap(true, Ordering::AcqRel) {
            self.set_mode(WarmMode::Play);
        }
    }

    /// The worker count wanted now ([`Self::set_threads`]).
    #[must_use]
    pub fn threads(&self) -> usize {
        self.lock().target
    }

    /// Workers alive now (a shrunk queue still counts those finishing their job).
    #[must_use]
    pub fn live_workers(&self) -> usize {
        self.lock().workers
    }

    /// Turns the engine's own warm jobs (the canonical frame at boot) on or off. A game that
    /// queues its own boot-tier frame turns them off. Call before the first resume.
    pub fn set_engine_jobs(&self, enabled: bool) {
        self.lock().no_engine_jobs = !enabled;
    }

    /// Whether the engine queues its own warm jobs ([`Self::set_engine_jobs`]).
    #[must_use]
    pub fn engine_jobs(&self) -> bool {
        !self.lock().no_engine_jobs
    }

    /// The next renderer a [`frame_job_handoff`] job warmed, in completion order, each once (the
    /// runtime renderer adopts its pipelines with
    /// [`crate::FrameRenderer::adopt_warm_pipelines`]).
    pub fn take_warmed_renderer(&self) -> Option<crate::FrameRenderer> {
        self.lock().warmed_renderers.pop_front()
    }

    /// Marks each finished job's label resident as a [`regia_asset::LoadKind::Pipeline`] item
    /// of `sets` (a load set lists its warm jobs by label).
    pub fn attach_load_sets(&self, sets: regia_asset::LoadSets) {
        self.lock().load_sets = Some(sets);
    }

    /// Whether [`Self::shutdown`] was called: long-running jobs (the quality probe) stop at
    /// their next check.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.lock().shutdown
    }

    /// Tests of other crates' warm jobs: `false` makes a shutdown stop running jobs only at their
    /// [`warm_cancelled`] checks, never by unwinding at a pipeline creation: the path of a
    /// `panic = "abort"` (shipping) build. Default `true`.
    #[doc(hidden)]
    pub fn set_cancel_unwinds(&self, unwind: bool) {
        self.shared.cancel.unwind.store(unwind, Ordering::Relaxed);
    }

    /// Tests: the pipelines this queue's warm threads created, and of those the ones created
    /// after [`Self::shutdown`] was called (the compiles a cancelled job ran before its check).
    #[doc(hidden)]
    #[must_use]
    pub fn cancel_counts(&self) -> (u64, u64) {
        (
            self.shared.cancel.created.load(Ordering::Relaxed),
            self.shared.cancel.late.load(Ordering::Relaxed),
        )
    }

    /// Whether [`Self::start`] has run.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.lock().device.is_some()
    }

    /// Runs every queued job of `tier` and earlier on this thread (tools and tests; a game
    /// never calls this on the frame path). Returns the number of jobs run.
    pub fn run_inline(&self, device: &RenderDevice, tier: WarmTier) -> usize {
        let mut ran = 0;
        loop {
            let next = {
                let mut state = self.lock();
                take_next(&mut state, Some(tier))
            };
            let Some((t, queued)) = next else {
                break;
            };
            run_job(&self.shared, device, t, queued);
            ran += 1;
        }
        ran
    }

    /// Blocks until `tier` is ready or `timeout` passes (tools, level-load screens; never the
    /// frame path). Returns whether the tier is ready.
    pub fn wait_tier(&self, tier: WarmTier, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if tier_ready(&state, tier) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .done
                .wait_timeout(state, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    /// Whether `tier` and every earlier tier have finished.
    #[must_use]
    pub fn is_tier_ready(&self, tier: WarmTier) -> bool {
        tier_ready(&self.lock(), tier)
    }

    /// Current progress.
    #[must_use]
    pub fn progress(&self) -> PipelineWarmProgress {
        let state = self.lock();
        let tiers: Vec<TierProgress> = WarmTier::ALL
            .iter()
            .map(|&tier| {
                let s = &state.stats[tier as usize];
                TierProgress {
                    tier: tier.name(),
                    jobs_total: s.total,
                    jobs_done: s.done,
                    jobs_failed: s.failed,
                    pipelines: TIER_PIPELINES[tier as usize].load(Ordering::Relaxed),
                    busy_ms: s.busy.as_secs_f32() * 1000.0,
                    ready_after_ms: s.ready_after.map(|d| d.as_secs_f32() * 1000.0),
                }
            })
            .collect();
        PipelineWarmProgress {
            done: tiers.iter().map(|t| t.jobs_done).sum(),
            total: tiers.iter().map(|t| t.jobs_total).sum(),
            tiers,
            pipelines_outside: OUTSIDE_PIPELINES.load(Ordering::Relaxed),
            outside_sites: OUTSIDE_SITES
                .lock()
                .map(|sites| sites.iter().map(|(s, n)| (s.to_string(), *n)).collect())
                .unwrap_or_default(),
            started: state.device.is_some(),
            last_error: state.last_error.clone(),
            cache: crate::pipeline_cache::status(),
            deferred_pending: deferred_pending(),
            deferred_built: DEFERRED_BUILT.load(Ordering::Relaxed),
            deferred_released: DEFERRED_RELEASED.load(Ordering::Acquire),
        }
    }

    /// Stops the workers (pending jobs are dropped; a running job stops at its next
    /// [`warm_cancelled`] check, which the engine's warms make between their pipeline
    /// compiles, and where panics unwind also before its next pipeline compile, so about only
    /// the compile in flight finishes), waits for them (at most 30 s, a quarter second once the
    /// device is lost; ADR-276), and releases the queue's device and warmed renderers, so the device can be destroyed
    /// before the process exits (the app calls this on `WillTerminate`; a Vulkan device still
    /// alive at exit can deadlock NVIDIA's Linux driver in its exit handlers, and a worker still
    /// compiling there can crash it). A worker still in its compile after the wait is joined at
    /// process exit instead (Linux and Android: an `atexit` hook, bounded too).
    pub fn shutdown(&self) {
        shutdown_shared(&self.shared, shutdown_wait(), true);
    }
}

/// [`PipelineWarmQueue::shutdown`] with a bounded `wait` for the workers inside a job; idle
/// workers are always joined. Workers still in a job after it go to [`LEFTOVER_WORKERS`] for
/// the exit hook. `log`: false from the exit hook, where the logger's thread-locals may already
/// be destroyed (a panic there would abort a `panic = "abort"` build at every quit).
///
/// On return, nothing but the queue's handles and those leftover workers holds the queue: the
/// registry keeps only its [`Hub`], and every joined worker has dropped its handle and device.
fn shutdown_shared(shared: &Arc<Shared>, wait: Duration, log: bool) {
    signal_shutdown(shared);
    let handles = std::mem::take(
        &mut *shared
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let leftover = join_workers(handles, &shared.busy, Instant::now() + wait);
    if !leftover.is_empty() {
        if log {
            tracing::warn!(
                target: "regia_render",
                "{} pipeline warm worker(s) still compiling at shutdown; joined at exit",
                leftover.len()
            );
        }
        LEFTOVER_WORKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(leftover);
    }
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !log && cfg!(target_os = "macos") {
        // The exit hook runs after `main` returned, when the main thread's thread-locals are
        // gone. Dropping the last handle of a Metal queue waits for idle inside an autorelease
        // pool kept in one, and that panics (an abort in a `panic = "abort"` build). The process
        // is ending: leak the handles (ADR-1625).
        std::mem::forget(state.device.take());
        std::mem::forget(std::mem::take(&mut state.warmed_renderers));
        return;
    }
    state.device = None;
    state.warmed_renderers.clear();
}

/// The non-blocking half of [`shutdown_shared`]: sets the shutdown and cancel flags, drops the
/// pending jobs and wakes the idle workers, which return. Also the registered workers' stop
/// (ADR-287), for a host that ends the process without the clean exit.
fn signal_shutdown(shared: &Arc<Shared>) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.shutdown = true;
    shared.hub.available.store(false, Ordering::Release);
    shared.cancel.flag.store(true, Ordering::Release);
    // The queued closures drop after the lock is released: a closure whose drop calls this
    // queue would otherwise deadlock the shutdown (audit-v035-conc L1).
    let dropped = std::mem::take(&mut state.pending);
    drop(state);
    drop(dropped);
    unregister(&shared.hub);
    shared.hub.wake.notify_all();
}

/// Joins one queue's workers after its shutdown flag: each one that has finished, and all of
/// them as soon as none is inside a job (`busy` is 0). An idle worker only has to wake and
/// return; it never enters the driver again, so it is waited for however long the scheduler
/// keeps it off the CPU (a loaded machine can, for longer than `deadline`). Returns the workers
/// still in a job at `deadline`. Never joins the calling thread; touches no thread-local (the
/// exit hook calls it).
fn join_workers(
    handles: Vec<std::thread::JoinHandle<()>>,
    busy: &AtomicUsize,
    deadline: Instant,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut pending = handles;
    loop {
        let mut running = Vec::new();
        for handle in pending {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                running.push(handle);
            }
        }
        pending = running;
        if pending.is_empty() {
            return pending;
        }
        // The caller is not one of them: a worker's own job would keep `busy` above 0.
        if busy.load(Ordering::Acquire) == 0 {
            for handle in pending {
                let _ = handle.join();
            }
            return Vec::new();
        }
        if Instant::now() >= deadline {
            return pending;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Joins every thread of `handles` that finishes before `deadline`; returns the others. A
/// thread is joined only once it has finished, so this never waits on the calling thread (and
/// it touches no thread-local: the exit hook calls it).
fn join_bounded(
    handles: Vec<std::thread::JoinHandle<()>>,
    deadline: Instant,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut leftover = Vec::new();
    for handle in handles {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            leftover.push(handle);
        }
    }
    leftover
}

/// Workers [`PipelineWarmQueue::shutdown`] left in a compile, joined by the exit hook.
static LEFTOVER_WORKERS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Joins the warm workers a [`PipelineWarmQueue::shutdown`] left in a compile, waiting at most
/// `timeout`; returns how many are still running (they stay for the exit hook). Each holds a
/// device clone: the clean exit joins them before it drops the device (`crate::exit`).
pub fn join_leftover_workers(timeout: Duration) -> usize {
    let leftover = std::mem::take(
        &mut *LEFTOVER_WORKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    if leftover.is_empty() {
        return 0;
    }
    let still = join_bounded(leftover, Instant::now() + timeout);
    let n = still.len();
    LEFTOVER_WORKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(still);
    n
}

/// How long the exit hook waits for warm workers in all (was 5 s; a cold driver compile under
/// load can take seconds, and past this the process exits with a worker in the driver). Once the
/// device is lost it is [`crate::gpu_health::DEVICE_LOST_WORKER_WAIT`] (ADR-276).
const EXIT_WAIT: Duration = Duration::from_secs(30);

/// Registers [`exit_hook`] once per process (on the first [`PipelineWarmQueue::start`]; Linux,
/// Android, macOS and Windows). The GPU driver registered its own exit handlers when it loaded, before this,
/// and exit handlers run in reverse order, so the hook runs before the driver tears down.
fn register_exit_hook() {
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| {
        if !crate::platform::at_exit(exit_hook)
            && cfg!(any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                windows
            ))
        {
            tracing::warn!(target: "regia_render", "pipeline warm exit hook not registered");
        }
    });
}

/// Process exit (`atexit`): shuts down every warm queue still running — a custom runner that
/// never sent `WillTerminate`, or (Linux, Android, macOS) a game that called `std::process::exit` — and joins their
/// workers and the ones a shutdown left compiling, all within [`EXIT_WAIT`]. A worker holds a
/// device clone; it must not be inside the driver while the driver's own exit handlers run.
///
/// Touches no thread-local and logs nothing: on glibc the exiting thread's thread-locals are
/// destroyed before `atexit` handlers run, and a panic here aborts a `panic = "abort"` build.
/// The unit tests' device release runs it too, before it destroys their devices (ADR-1584).
pub(crate) extern "C" fn exit_hook() {
    let _ = std::panic::catch_unwind(|| {
        let deadline = Instant::now() + crate::gpu_health::worker_wait(EXIT_WAIT);
        let queues: Vec<Arc<Shared>> = QUEUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|hub| hub.queue.upgrade())
            .collect();
        for shared in &queues {
            shutdown_shared(shared, deadline.saturating_duration_since(Instant::now()), false);
        }
        drop(queues);
        let leftover = std::mem::take(
            &mut *LEFTOVER_WORKERS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let _ = join_bounded(leftover, deadline);
    });
}

fn tier_ready(state: &State, tier: WarmTier) -> bool {
    (0..=tier as usize).all(|t| state.stats[t].done == state.stats[t].total)
}

/// Records when each tier became ready.
fn note_ready_tiers(state: &mut State) {
    let since = state.started_at.map_or(Duration::ZERO, |t| t.elapsed());
    for t in WarmTier::ALL {
        if state.stats[t as usize].ready_after.is_none()
            && tier_ready(state, t)
            && state.stats[t as usize].total > 0
        {
            state.stats[t as usize].ready_after = Some(since);
        }
    }
}

/// Pops the highest-priority job (at most `max_tier`).
fn take_next(state: &mut State, max_tier: Option<WarmTier>) -> Option<(WarmTier, Queued)> {
    let limit = max_tier.map_or(WarmTier::ALL.len(), |t| t as usize + 1);
    for (i, tier) in WarmTier::ALL.iter().enumerate().take(limit) {
        if let Some(job) = state.pending[i].pop_front() {
            state.in_flight[i] += 1;
            state.running.push(Running {
                id: job.id,
                tier: *tier,
                label: job.label.clone(),
                since: Instant::now(),
            });
            return Some((*tier, job));
        }
    }
    None
}

/// Whether this queue's workers build the process's [`DeferredPipeline`]s (every queue does,
/// except the unit tests' [`PipelineWarmQueue::isolated`] ones).
fn builds_deferred(shared: &Shared) -> bool {
    #[cfg(test)]
    {
        !shared.isolated.load(Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        let _ = shared;
        true
    }
}

/// Counts a worker out of [`Shared::busy`] when its job ends (also by unwinding).
struct Busy<'a>(&'a AtomicUsize);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn worker(shared: &Arc<Shared>, gpu: &regia_platform::GpuWorker) {
    WARM_CANCEL.with(|c| *c.borrow_mut() = Some(shared.cancel.clone()));
    if shared.background.load(Ordering::Acquire) {
        let _ = regia_platform::background::lower_current_thread_priority();
    }
    loop {
        let job = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if state.shutdown {
                    #[cfg(test)]
                    {
                        drop(state);
                        std::thread::sleep(Duration::from_millis(
                            shared.exit_delay_ms.load(Ordering::Relaxed),
                        ));
                    }
                    return;
                }
                if state.workers > state.target {
                    // Shrunk by `set_threads`: this worker retires between jobs.
                    state.workers -= 1;
                    return;
                }
                if let Some(device) = state.device.clone() {
                    if let Some((tier, queued)) = take_next(&mut state, None) {
                        shared.busy.fetch_add(1, Ordering::AcqRel);
                        break Ok((device, tier, queued));
                    }
                    // Nothing queued: deferred variants, once released.
                    if builds_deferred(shared)
                        && let Some(build) = take_deferred()
                    {
                        shared.busy.fetch_add(1, Ordering::AcqRel);
                        break Err(build);
                    }
                }
                state = shared
                    .hub
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let _busy = Busy(&shared.busy);
        let _call = gpu.driver_call();
        match job {
            Ok((device, tier, queued)) => run_job(shared, &device, tier, queued),
            Err(build) => run_deferred(shared, &*build),
        }
    }
}

fn run_job(shared: &Arc<Shared>, device: &RenderDevice, tier: WarmTier, queued: Queued) {
    let _span = tracing::info_span!("pipeline_warm_job", tier = tier.name(), label = %queued.label).entered();
    let start = Instant::now();
    let before = TIER_PIPELINES[tier as usize].load(Ordering::Relaxed);
    CURRENT_TIER.with(|c| c.set(Some(tier)));
    let job = queued.job;
    let id = queued.id;
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(device))).unwrap_or_else(|panic| {
            Err(if panic.is::<WarmCancelled>() {
                CANCELLED_AT_SHUTDOWN.to_string()
            } else {
                format!(
                    "warm job panicked: {}",
                    crate::graph_pipelines::panic_message(&*panic)
                )
            })
        });
    // A job that stopped early at a `warm_cancelled` check returns its own error; report it
    // as what it is.
    let result = match result {
        Err(_) if shared.cancel.is_set() => Err(CANCELLED_AT_SHUTDOWN.to_string()),
        other => other,
    };
    close_site(Instant::now());
    CURRENT_TIER.with(|c| c.set(None));
    let built = TIER_PIPELINES[tier as usize].load(Ordering::Relaxed) - before;
    let elapsed = start.elapsed();
    tracing::debug!(
        target: "regia_render",
        "pipeline warm {} [{}]: {built} pipelines in {:.1} ms",
        queued.label,
        tier.name(),
        elapsed.as_secs_f32() * 1000.0
    );
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let sets = state.load_sets.clone();
    let stats = &mut state.stats[tier as usize];
    stats.done += 1;
    stats.busy += elapsed;
    // The load-set marks run their progress callbacks, which may call this queue: they run
    // after the state lock is released (audit-v035-conc M2).
    let failed = match &result {
        Ok(()) => None,
        Err(err) => {
            stats.failed += 1;
            tracing::warn!(target: "regia_render", "pipeline warm job {} failed: {err}", queued.label);
            state.last_error = Some(format!("{}: {err}", queued.label));
            Some(err.clone())
        }
    };
    state.in_flight[tier as usize] -= 1;
    state.running.retain(|r| r.id != id);
    state.dirty |= built > 0;
    note_ready_tiers(&mut state);
    let drained = state.pending.iter().all(VecDeque::is_empty) && state.in_flight.iter().all(|&n| n == 0);
    // Not while shutting down (see `run_deferred`).
    let save = drained && state.dirty && shared.save_cache && !state.shutdown;
    if save {
        state.dirty = false;
    }
    drop(state);
    if let Some(sets) = &sets {
        match failed {
            None => sets.mark_resident(regia_asset::LoadKind::Pipeline, &queued.label),
            Some(err) => sets.mark_failed(regia_asset::LoadKind::Pipeline, &queued.label, err),
        }
    }
    shared.done.notify_all();
    if save && let Some(Err(err)) = crate::pipeline_cache::save_installed() {
        tracing::warn!(target: "regia_render", "pipeline cache save failed: {err}");
    }
}

/// A warm job that renders `frame`'s views once, offscreen at 64×64 in `format`, with a
/// throwaway [`crate::FrameRenderer`]: every pipeline that frame configuration uses (passes,
/// post, shadows, material graphs — built inline) is compiled into the shared cache.
///
/// Build `frame` like the game's real one (same post settings, quality, outdoor/terrain on or
/// off, a mesh per material kind) so the warmed set is the set the game draws.
pub fn frame_job(frame: crate::ExtractedFrame, format: wgpu::TextureFormat) -> WarmJob {
    Box::new(move |device: &RenderDevice| warm_frame(device, &frame, format).map(drop))
}

/// [`frame_job`], keeping the warmed renderer in `queue` so the runtime renderer adopts its
/// pipelines instead of creating them again on its first frame (the engine's boot-tier job; a
/// game's menu frame). Use it for the frames the window draws first, not for whole maps: the
/// warmed renderer holds its frame's meshes until the runtime renderer takes it.
pub fn frame_job_handoff(
    frame: crate::ExtractedFrame,
    format: wgpu::TextureFormat,
    queue: PipelineWarmQueue,
) -> WarmJob {
    Box::new(move |device: &RenderDevice| {
        let renderer = warm_frame(device, &frame, format)?;
        queue.lock().warmed_renderers.push_back(renderer);
        Ok(())
    })
}

fn warm_frame(
    device: &RenderDevice,
    frame: &crate::ExtractedFrame,
    format: wgpu::TextureFormat,
) -> Result<crate::FrameRenderer, String> {
    let mut renderer = crate::FrameRenderer::new();
    renderer
        .warm_extracted(device, frame, format, true)
        .map_err(|e| e.to_string())?;
    Ok(renderer)
}

/// Side of the offscreen target a warm frame renders into ([`crate::FrameRenderer::warm_scene`]).
/// Pipelines do not depend on the target size, only on its format and the frame configuration.
pub const WARM_FRAME_SIZE: u32 = 64;

/// The offscreen target of a warm frame.
struct WarmTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    format: wgpu::TextureFormat,
    readable: bool,
}

impl WarmTarget {
    fn new(device: &RenderDevice, format: wgpu::TextureFormat, readable: bool) -> Self {
        Self::sized(device, format, readable, (WARM_FRAME_SIZE, WARM_FRAME_SIZE))
    }

    fn sized(
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        readable: bool,
        (width, height): (u32, u32),
    ) -> Self {
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if readable {
            usage |= wgpu::TextureUsages::COPY_SRC;
        }
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pipeline warm target"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            format,
            readable,
        }
    }

    /// The rectangle `rect` of this target (a window-sized target, one view's viewport).
    fn rect_target(&self, rect: [u32; 4], load_color: bool) -> crate::FrameTarget<'_> {
        let size = self.texture.size();
        crate::FrameTarget::rect(&self.view, self.format, size.width, size.height, rect, load_color)
            .with_color(self.readable.then_some(&self.texture))
    }

    fn frame_target(&self, load_color: bool) -> crate::FrameTarget<'_> {
        let side = WARM_FRAME_SIZE;
        crate::FrameTarget::rect(
            &self.view,
            self.format,
            side,
            side,
            [0, 0, side, side],
            load_color,
        )
        .with_color(self.readable.then_some(&self.texture))
    }
}

impl crate::FrameRenderer {
    /// Builds every pipeline the frame `scene` draws with, on this renderer: renders it once
    /// offscreen at [`WARM_FRAME_SIZE`]² in `format` (the format the real frames target;
    /// `readable` when the real target is passed with [`crate::FrameTarget::with_color`]),
    /// settles the material-graph builds and waits for the GPU. The forward and material
    /// pipelines [`Self::prewarm`] builds, plus everything only a frame builds: the post stack,
    /// shadow cascades, sky and clouds, volumetric fog, SSAO/SSGI/SSR, TAAU/TSR, decals and the
    /// final pass in `format`.
    ///
    /// The next frame at the real size allocates its targets but compiles nothing, so
    /// "0 pipeline creates after warm-up" (`specs/27` § 2.4) holds from the first game frame.
    /// Slow cold (seconds: it is the whole first-frame compile), so call it where a frame
    /// is not expected: on a [`PipelineWarmQueue`] worker
    /// ([`PipelineWarmQueue::warm_renderer`], which the windowed runtime does by itself through
    /// [`GameFrameWarm`]) or behind a loading screen. The first real frame starts a new
    /// temporal history (its size differs), exactly like a fresh renderer's first frame.
    ///
    /// # Errors
    ///
    /// The warm frame failed to render or the GPU wait failed.
    #[tracing::instrument(skip_all)]
    pub fn warm_scene(
        &mut self,
        device: &RenderDevice,
        scene: &crate::FrameScene<'_>,
        format: wgpu::TextureFormat,
        readable: bool,
    ) -> Result<(), crate::RenderError> {
        // A warm may wait for every deferred variant its frame's materials can draw.
        let _blocking = blocking_scope();
        let target = WarmTarget::new(device, format, readable);
        let mut small = scene.clone();
        small.width = WARM_FRAME_SIZE;
        small.height = WARM_FRAME_SIZE;
        // Twice: passes that only run with a temporal history (TAA sharpen) are built by the
        // second frame, not the second game frame.
        for _ in 0..2 {
            self.render_into(device, &small, target.frame_target(false))?;
        }
        // The host's other targets (ADR-7885).
        self.warm_registered_targets(
            device,
            (format, scene.quality.msaa_samples),
            std::slice::from_ref(&small),
        )?;
        // Variants the shared draw (terrain) can switch to mid-play (a hole cut).
        self.warm_shared_draw();
        self.prewarm_double_sided_sets(device)?;
        self.prewarm_level_graphs(device)?;
        self.settle_graphs_unless_cancelled(None)?;
        self.prewarm_lightmap(device)?;
        // The next frame starts without the warm frame's history, like a fresh renderer's.
        self.camera_cut();
        warm_checkpoint()?;
        let _span = tracing::info_span!("warm_wait_idle").entered();
        device.wait_idle()
    }

    /// Renders the warm frame `scenes` (one per window view, warm-frame sized) into every
    /// registered target ([`registered_warm_targets`]) other than `drawn` (the format and
    /// sample count the warm itself rendered): twice with the frame's post stack (passes that
    /// need a history), once without (a render-to-texture camera that draws without post). The
    /// forward sets for that sample count, the post chain's output pipelines for that format and
    /// whatever the frame's passes key on them are then built. Returns the targets warmed.
    fn warm_registered_targets(
        &mut self,
        device: &RenderDevice,
        drawn: (wgpu::TextureFormat, u32),
        scenes: &[crate::FrameScene<'_>],
    ) -> Result<usize, crate::RenderError> {
        let mut warmed = 0;
        for key in registered_warm_targets() {
            if (key.format, key.samples) == (drawn.0, drawn.1.max(1)) || scenes.is_empty() {
                continue;
            }
            let _span =
                tracing::info_span!("warm_registered_target", format = ?key.format, samples = key.samples)
                    .entered();
            warm_checkpoint()?;
            let target = WarmTarget::new(device, key.format, false);
            for with_post in [true, true, false] {
                for (index, scene) in scenes.iter().enumerate() {
                    let mut scene = scene.clone();
                    scene.quality.msaa_samples = key.samples;
                    if !with_post {
                        scene.post = None;
                    }
                    self.render_into(device, &scene, target.frame_target(index > 0))?;
                }
            }
            warmed += 1;
        }
        Ok(warmed)
    }

    /// Waits for the material-graph builds the warm frame queued, at most `budget` (`None`:
    /// all of them), in short slices so a queue shutdown ([`warm_cancelled`]) ends the wait
    /// at once. Returns whether every build landed.
    fn settle_graphs_unless_cancelled(
        &mut self,
        budget: Option<Duration>,
    ) -> Result<bool, crate::RenderError> {
        /// A shutdown is noticed within this.
        const SLICE: Duration = Duration::from_millis(20);
        let deadline = budget.map(|b| Instant::now() + b);
        loop {
            warm_checkpoint()?;
            let now = Instant::now();
            let slice_end = deadline.map_or(now + SLICE, |d| d.min(now + SLICE));
            if self.settle_material_graphs_for(slice_end.saturating_duration_since(now)) {
                return Ok(true);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(false);
            }
        }
    }

    /// [`Self::warm_scene`] for an ECS frame: applies its per-frame renderer state (outdoor,
    /// glass, mesh occluders, temporal upscaler, gizmos) like the window path does and warms
    /// every window view. It also builds what effects can need later in the map: the decal
    /// receiver when translucent layers are installed ([`Self::prewarm_decals`]), the
    /// glass-instance sets when the frame has a transmissive standard material or glass
    /// instances ([`Self::prewarm_glass_instances`]; always with a `window`, see
    /// [`Self::warm_extracted_at`]), and one reflection-probe face when the renderer captures
    /// probes ([`Self::set_warm_probe_capture`]).
    ///
    /// Unlike [`Self::warm_scene`], it waits for the frame's async material-graph builds only
    /// until [`warm_graph_budget`] after its start ([`GameFrameWarm`]'s warm returns in about
    /// the time of its own pipelines, or the budget if that is longer): graph sets still building after that keep building on the workers and
    /// land on the frames that follow, drawing the standard surface until then, exactly as on
    /// the frame path. A loading screen that wants every graph first calls
    /// [`Self::settle_material_graphs`] afterwards. Renderers with inline graph builds (the
    /// boot-tier [`frame_job`]s) have built every graph inside the frame.
    ///
    /// # Errors
    ///
    /// The frame has no window view, a view failed to render or the GPU wait failed.
    pub fn warm_extracted(
        &mut self,
        device: &RenderDevice,
        frame: &crate::ExtractedFrame,
        format: wgpu::TextureFormat,
        readable: bool,
    ) -> Result<(), crate::RenderError> {
        self.warm_extracted_at(device, frame, format, readable, None)
    }

    /// The game-frame warm of a frame with no window view (a headless host's render-to-texture
    /// cameras, ADR-8122): renders every texture view of `frame` twice at its own size, as
    /// `render_texture_views` will, so its meshes are uploaded, its pipelines created and its
    /// targets allocated here.
    ///
    /// # Errors
    ///
    /// The frame has no texture view, or a view failed to render.
    pub fn warm_texture_views(
        &mut self,
        device: &RenderDevice,
        frame: &crate::ExtractedFrame,
    ) -> Result<(), crate::RenderError> {
        let _blocking = blocking_scope();
        let views = frame.texture_views();
        if views.is_empty() {
            return Err(crate::RenderError::Encode(
                "frame has no window or texture view".into(),
            ));
        }
        let default_settings = crate::TextureViewSettings::default();
        // Twice, as the window warm: the passes that need a temporal history are built too.
        for _ in 0..2 {
            for (view_index, view) in &views {
                let crate::runtime::ExtractedTarget::Texture { asset_id } = view.target else {
                    continue;
                };
                let settings = frame
                    .texture_view_settings
                    .get(&asset_id)
                    .unwrap_or(&default_settings);
                let (width, height) = settings.extent();
                let scene = frame.frame_scene(*view_index, width, height);
                let _timing = self.render_texture_view(device, asset_id, scene, settings)?;
            }
        }
        Ok(())
    }

    /// [`Self::warm_extracted`], then, with `window`, one more frame of every window view at
    /// the window's size (ADR-173): the full-size render targets and temporal history
    /// resources are allocated here, on the warm worker, instead of on the first visible frame
    /// (a 28–36 ms one-off hitch on an RTX 5080). The renderer still starts the next frame
    /// with a camera cut, so that frame draws exactly as it would after the 64×64 warm alone.
    /// With a `window` (a game-frame warm, [`GameFrameWarm`]) the glass-instance sets are
    /// built whatever the frame holds: a host adds glass instances mid-play.
    ///
    /// # Errors
    ///
    /// As [`Self::warm_extracted`].
    #[tracing::instrument(skip_all)]
    pub fn warm_extracted_at(
        &mut self,
        device: &RenderDevice,
        frame: &crate::ExtractedFrame,
        format: wgpu::TextureFormat,
        readable: bool,
        window: Option<(u32, u32)>,
    ) -> Result<(), crate::RenderError> {
        // A warm may wait for every deferred variant its frame's materials can draw.
        let _blocking = blocking_scope();
        let started = Instant::now();
        self.set_outdoor(frame.outdoor.clone());
        // The state the window render applies to each frame it draws (ADR-5131): the GI tier sizes
        // the probe volume, so a warm at the default tier left an Ultra frame to rebuild the probes.
        self.set_valley_mist(frame.valley_mist.clone());
        self.set_dynamic_gi_tier(frame.dynamic_gi_tier);
        self.set_radiance_cache(frame.radiance_cache);
        self.set_two_phase_occlusion(frame.two_phase_occlusion);
        self.set_glass(frame.glass.clone());
        self.set_mask_decals(&frame.mask_decals);
        self.set_mesh_occluders(frame.mesh_occluders.clone());
        self.set_temporal_upscaler(frame.temporal_upscaler);
        self.set_gizmos(Some(frame.gizmos.clone()));
        self.set_gi_proxy_disabled(frame.gi_proxy_disabled_keys());
        // Morphed instances: the warm builds the morph pre-pass pipeline.
        self.set_morph_instances(&frame.morphs);
        let views = frame.window_views();
        if views.is_empty() {
            return Err(crate::RenderError::Encode("frame has no window view".into()));
        }
        let target = WarmTarget::new(device, format, readable);
        // Twice: passes that only run with a temporal history (TAA sharpen) are built by the
        // second frame, not the second game frame.
        for _ in 0..2 {
            for (index, (view_index, _)) in views.iter().enumerate() {
                let scene = frame.frame_scene(*view_index, WARM_FRAME_SIZE, WARM_FRAME_SIZE);
                self.render_into(device, &scene, target.frame_target(index > 0))?;
            }
        }
        // The host's other targets (render-to-texture cameras, a second window; ADR-7885): before
        // the effects below, which build glass and double-sided twins for every set it made.
        let scenes: Vec<crate::FrameScene<'_>> = views
            .iter()
            .map(|(view_index, _)| frame.frame_scene(*view_index, WARM_FRAME_SIZE, WARM_FRAME_SIZE))
            .collect();
        let samples = scenes.first().map_or(1, |scene| scene.quality.msaa_samples);
        self.warm_registered_targets(device, (format, samples), &scenes)?;
        drop(scenes);
        // A reflection-probe capture's configuration (no post stack, the readback target) when
        // this renderer captures probes: the host's next capture compiles nothing (ADR-210).
        if let Some((view_index, _)) = views.first() {
            let scene = frame.frame_scene(*view_index, WARM_FRAME_SIZE, WARM_FRAME_SIZE);
            self.warm_probe_face(device, &scene)?;
        }
        // Effects the map can start mid-play compile here, not on their first frame: the decal
        // receiver when effect layers are installed, the glass-instance sets when a transmissive
        // standard material can carry one (an own canopy entered, a pane cracked). A game-frame
        // warm (`window`) builds the glass sets in any case: the warm frame is one moment of the
        // map, and a vehicle with a glass canopy spawned after it compiled them on the frame
        // path in DO27's v0.1.10 smoke (ADR-210).
        let glass = window.is_some()
            || !frame.glass.is_empty()
            || frame
                .meshes
                .iter()
                .any(|mesh| mesh.material.transmission > 0.0 && mesh.material.surface_graph.is_none());
        self.prewarm_frame_effects(device, glass)?;
        // What play can switch on after a game-frame warm: TSR, the depth match, the outdoor
        // passes of a map with clouds or fog (ADR-220).
        if window.is_some() {
            // Optional: a failure here must not skip the window-size frame, the level graphs or
            // the camera cut, so the renderer never comes back with a warm frame's history.
            optional_warm_step("prewarm_mid_play", self.prewarm_mid_play(device))?;
            // The blended and transmission variants of every set the warm built (ADR-7885): the
            // first glass, blended or additive draw of play finds them built, instead of a warm
            // worker building them after warm-up while the frame skips the draw.
            optional_warm_step("prewarm_deferred_variants", self.prewarm_deferred_variants())?;
        }
        // The level's other graphs (spawned mid-play) start building next to the frame's.
        self.prewarm_level_graphs(device)?;
        // The graph sets have been building on their workers since the frame met them; wait
        // until the budget (counted from the warm's start) is spent, then leave the rest async.
        let left = warm_graph_budget().saturating_sub(started.elapsed());
        if !self.settle_graphs_unless_cancelled(Some(left))? {
            let stats = self.material_graph_stats();
            tracing::info!(
                target: "regia_render",
                "warm frame: {} material-graph sets ready, {} still building (async, standard surface until they land)",
                stats.ready,
                stats.pending
            );
        }
        // The visibility path's targets and lists at the window size (ADR-2689): after the frames
        // above noted the profile's selection (the lists' reserve), before the window-size frame,
        // which leaves the path's active flag and the LOD hand-off as they were.
        self.warm_visibility_targets(device, frame, (format, readable), window)?;
        if let Some((width, height)) = window.filter(|(w, h)| *w > 0 && *h > 0) {
            // One frame at the window's size: allocations only (the pipelines are built).
            let _span = tracing::info_span!("warm_window_size_frame").entered();
            let target = WarmTarget::sized(device, format, readable, (width, height));
            for (index, (view_index, view)) in views.iter().enumerate() {
                let rect = crate::surface::pixel_viewport(view.viewport, width, height);
                let scene = frame.frame_scene(*view_index, rect[2], rect[3]);
                self.render_into(device, &scene, target.rect_target(rect, index > 0))?;
            }
            // The window-size HDR target's refraction copy (a pooled target, ADR-2018).
            if glass {
                self.prewarm_refraction_copies(&device.tracked());
            }
        }
        // The next frame starts without the warm frame's history, like a fresh renderer's.
        self.camera_cut();
        warm_checkpoint()?;
        let _span = tracing::info_span!("warm_wait_idle").entered();
        device.wait_idle()
    }
}

/// Result of an optional warm step: a failure is logged and the warm continues, except the
/// shutdown cancellation, which still ends it (`Err`). Returns the step's count (0 on failure).
pub(crate) fn optional_warm_step(
    name: &str,
    result: Result<usize, crate::RenderError>,
) -> Result<usize, crate::RenderError> {
    match result {
        Ok(n) => Ok(n),
        Err(err) if warm_cancelled() => Err(err),
        Err(err) => {
            tracing::warn!(
                target: "regia_render",
                "optional warm step {name} failed ({err}); the warm continues"
            );
            Ok(0)
        }
    }
}

/// Until when [`crate::FrameRenderer::warm_extracted`] waits for its frame's async
/// material-graph builds, counted from the warm's start: 2 s, `REGIA_WARM_GRAPH_BUDGET_MS`
/// overrides (`0`: install what has landed, never wait).
/// A big level's graphs take longer than that to build even on many workers; waiting for all
/// of them held a DO27 map's game-frame warm for ~50 s while its own pipelines took ~2 s.
#[must_use]
pub fn warm_graph_budget() -> Duration {
    static BUDGET: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        warm_graph_budget_from(
            regia_core::brand::env_var("REGIA_WARM_GRAPH_BUDGET_MS")
                .ok()
                .as_deref(),
        )
    })
}

/// [`warm_graph_budget`] for an environment value: 2 s unless `value` is a whole number of
/// milliseconds.
fn warm_graph_budget_from(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_secs(2), Duration::from_millis)
}

thread_local! {
    /// Pipelines this thread created while running a warm job (per-job counts).
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static JOB_PIPELINES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// What a [`PipelineWarmQueue::warm_renderer`] job handed back.
pub struct WarmedRenderer {
    /// The renderer, warmed. `None` when the job panicked (the renderer's state is unknown
    /// and it was dropped); keep using the stand-in.
    pub renderer: Option<crate::FrameRenderer>,
    /// The warm closure's error, if any (the renderer is still returned).
    pub error: Option<String>,
    /// Wall time of the warm on the worker.
    pub ms: f32,
    /// Pipelines the warm created.
    pub pipelines: u64,
}

/// A copy of a renderer being warmed for offscreen captures on its own thread
/// ([`OffscreenWarm::start`], ADR-1965).
pub struct OffscreenWarm {
    slot: Arc<Mutex<Option<WarmedRenderer>>>,
    thread: std::thread::JoinHandle<()>,
}

impl OffscreenWarm {
    /// Warms a copy of `runtime` for offscreen captures, for a host that draws only through them
    /// (a headless render-out host), which no window warms: a fresh renderer with `runtime`'s
    /// installs (translucent layers, GI field, baked GI, interior scene, reflection probes) renders
    /// `frame` on a thread of its own in the offscreen capture format, ending with a frame at
    /// `size`. `runtime` is not touched, and no [`PipelineWarmQueue`] worker is started (a running
    /// queue changes how the frame thread builds deferred variants and material graphs). When
    /// [`Self::take`] returns the copy, hand it to [`crate::FrameRenderer::adopt_warm_pipelines`]:
    /// the runtime renderer takes the pipeline sets it has not built instead of compiling them on
    /// the frame thread.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub fn start(
        device: &RenderDevice,
        runtime: &crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        size: (u32, u32),
    ) -> std::io::Result<Self> {
        let mut copy = crate::FrameRenderer::new();
        runtime.share_installs_with(&mut copy);
        Self::spawn(device, copy, frame, size)
    }

    /// [`Self::start`] on `runtime` itself: the renderer moves to the warm thread and a stand-in
    /// with its installs takes its place (draw nothing that matters with it meanwhile). Besides the
    /// pipelines, the warm does the renderer's first-frame work there (the scene's uploads, the
    /// size's targets), which a copy cannot hand over. Put the warmed renderer back with
    /// [`Self::put_back`]. If startup fails, the original renderer remains installed.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned or its renderer handoff closed before delivery.
    pub fn start_in_place(
        device: &RenderDevice,
        runtime: &mut crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        size: (u32, u32),
    ) -> std::io::Result<Self> {
        Self::start_in_place_with(runtime, |receiver| {
            Self::spawn_from(
                device,
                move || receiver.recv().ok(),
                frame,
                size,
                Arc::new(Mutex::new(None)),
            )
        })
    }

    /// The in-place handoff with an injectable spawn operation for deterministic failure tests.
    fn start_in_place_with(
        runtime: &mut crate::FrameRenderer,
        spawn: impl FnOnce(std::sync::mpsc::Receiver<crate::FrameRenderer>) -> std::io::Result<Self>,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let warm = spawn(receiver)?;
        let mut stand_in = crate::FrameRenderer::new();
        runtime.share_installs_with(&mut stand_in);
        let renderer = std::mem::replace(runtime, stand_in);
        if let Err(error) = sender.send(renderer) {
            *runtime = error.0;
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the offscreen warm renderer handoff closed before delivery",
            ));
        }
        Ok(warm)
    }

    /// Puts a renderer warmed by [`Self::start_in_place`] back in `runtime`'s place; what was
    /// installed on the stand-in meanwhile is taken over, as if it had been made on the renderer.
    pub fn put_back(runtime: &mut crate::FrameRenderer, mut warmed: crate::FrameRenderer) {
        let mut stand_in = std::mem::replace(runtime, crate::FrameRenderer::new());
        warmed.absorb_stand_in(&mut stand_in);
        *runtime = warmed;
    }

    fn spawn(
        device: &RenderDevice,
        copy: crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        size: (u32, u32),
    ) -> std::io::Result<Self> {
        Self::spawn_filling(device, copy, frame, size, Arc::new(Mutex::new(None)))
    }

    /// [`Self::spawn`], with the slot the thread fills as its last step.
    fn spawn_filling(
        device: &RenderDevice,
        copy: crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        size: (u32, u32),
        slot: Arc<Mutex<Option<WarmedRenderer>>>,
    ) -> std::io::Result<Self> {
        Self::spawn_from(device, move || Some(copy), frame, size, slot)
    }

    fn spawn_from(
        device: &RenderDevice,
        renderer: impl FnOnce() -> Option<crate::FrameRenderer> + Send + 'static,
        frame: &crate::ExtractedFrame,
        size: (u32, u32),
        slot: Arc<Mutex<Option<WarmedRenderer>>>,
    ) -> std::io::Result<Self> {
        let (frame, device) = (frame.clone(), device.clone());
        let filled = Arc::clone(&slot);
        // A GPU worker (ADR-287): it compiles pipelines and drops a device clone when it ends,
        // so the end of the process waits for it like every other GPU thread instead of tearing
        // the driver down under it (audit-v035-conc H1).
        let thread = regia_platform::GpuWorkerBuilder::new("regia-offscreen-warm").spawn(move |_| {
            let Some(mut copy) = renderer() else {
                return;
            };
            let _span = tracing::info_span!(target: "regia_render", "offscreen_warm").entered();
            let start = Instant::now();
            let before = crate::pipeline_cache::pipelines_created();
            let result = copy.warm_extracted_at(
                &device,
                &frame,
                crate::frame::PROBE_CAPTURE_FORMAT,
                true,
                Some(size),
            );
            let warmed = WarmedRenderer {
                renderer: Some(copy),
                error: result.err().map(|e| e.to_string()),
                ms: start.elapsed().as_secs_f32() * 1000.0,
                pipelines: crate::pipeline_cache::pipelines_created().saturating_sub(before),
            };
            *filled.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(warmed);
        })?;
        Ok(Self { slot, thread })
    }

    /// The warmed copy once its thread is done (`None` while it runs); it is handed out once. A
    /// thread that ended without one (it panicked), and every call after the copy was taken,
    /// answers with no renderer and an error.
    #[must_use]
    pub fn take(&self) -> Option<WarmedRenderer> {
        // Read before the slot: a thread that finished has filled it, unless it panicked.
        let finished = self.thread.is_finished();
        let warmed = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match warmed {
            Some(warmed) => Some(warmed),
            None if finished => Some(WarmedRenderer {
                renderer: None,
                error: Some("the offscreen warm thread ended without a renderer".into()),
                ms: 0.0,
                pipelines: 0,
            }),
            None => None,
        }
    }
}

/// Handle to a renderer moved onto a warm worker ([`PipelineWarmQueue::warm_renderer`]).
#[derive(Clone)]
pub struct RendererWarm {
    slot: Arc<Mutex<Option<WarmedRenderer>>>,
    /// The queue job's id ([`Self::cancel`]).
    job: u64,
}

impl RendererWarm {
    /// Whether the job finished (the renderer is waiting in [`Self::take`]).
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Cancels the job if no worker has started it: the renderer comes back unwarmed through
    /// [`Self::take`] (with the error "cancelled"). Returns false once the job is running or
    /// done; a running job cannot be stopped.
    pub fn cancel(&self, queue: &PipelineWarmQueue) -> bool {
        queue.cancel_queued(self.job)
    }

    /// The warmed renderer, once the job finished (each handle yields it once). Taking it
    /// releases the deferred pipeline variants to the background ([`release_deferred`]): the
    /// host draws its first game frame next.
    pub fn take(&self) -> Option<WarmedRenderer> {
        let warmed = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if warmed.is_some() {
            release_deferred();
        }
        warmed
    }
}

/// Hands the renderer back when the job ends, also when it panicked or was dropped unrun.
struct ReturnGuard {
    slot: Arc<Mutex<Option<WarmedRenderer>>>,
    renderer: Option<crate::FrameRenderer>,
    error: Option<String>,
    start: Instant,
    pipelines_before: u64,
}

impl Drop for ReturnGuard {
    fn drop(&mut self) {
        let panicked = std::thread::panicking();
        let warmed = WarmedRenderer {
            renderer: if panicked { None } else { self.renderer.take() },
            error: if panicked {
                Some("warm job panicked".into())
            } else {
                self.error.take()
            },
            ms: self.start.elapsed().as_secs_f32() * 1000.0,
            pipelines: JOB_PIPELINES.with(std::cell::Cell::get) - self.pipelines_before,
        };
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(warmed);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Registered render targets (ADR-7885).
// ---------------------------------------------------------------------------------------------

/// A colour target a host renders frames into besides the window: a render-to-texture camera,
/// a second window, an offscreen capture (ADR-7885). The frame warms render their warm frame
/// into every registered target too, so the forward sets, the post chain's output pipelines and
/// the effects' sets for its format and sample count are built before play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WarmTargetKey {
    /// Colour format of the target the frames are rendered into.
    pub format: wgpu::TextureFormat,
    /// MSAA sample count the frames into it render with (`FrameQuality::msaa_samples`; 1 = none).
    pub samples: u32,
}

/// Registered targets ([`register_warm_target`]); process-wide like the warm queues, small.
static WARM_TARGETS: Mutex<Vec<WarmTargetKey>> = Mutex::new(Vec::new());

/// Registers a target the host renders into besides the window ([`WarmTargetKey`]): every frame
/// warm that starts afterwards ([`GameFrameWarm`], [`crate::FrameRenderer::warm_scene`],
/// [`crate::FrameRenderer::warm_extracted`]) also renders its warm frame into it, and the
/// effects' warm formats include its format. Call it where the target is created, before the
/// warm (`samples` 0 counts as 1). Returns whether it was new. Frames are unchanged.
pub fn register_warm_target(format: wgpu::TextureFormat, samples: u32) -> bool {
    let key = WarmTargetKey {
        format,
        samples: samples.max(1),
    };
    let mut targets = WARM_TARGETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if targets.contains(&key) {
        return false;
    }
    targets.push(key);
    true
}

/// The targets [`register_warm_target`] registered, in registration order.
#[must_use]
pub fn registered_warm_targets() -> Vec<WarmTargetKey> {
    WARM_TARGETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The distinct colour formats of [`registered_warm_targets`] (the effects' and terrain's warm
/// formats add these).
#[must_use]
pub fn registered_warm_formats() -> Vec<wgpu::TextureFormat> {
    let mut formats: Vec<wgpu::TextureFormat> = Vec::new();
    for key in registered_warm_targets() {
        if !formats.contains(&key.format) {
            formats.push(key.format);
        }
    }
    formats
}

/// Drops every registered target (a host that tears its offscreen targets down, tests).
pub fn clear_warm_targets() {
    WARM_TARGETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

impl PipelineWarmQueue {
    /// Registers a target the host will render into ([`register_warm_target`]): the frame
    /// warms that start afterwards on this queue (the engine's [`GameFrameWarm`] included) build
    /// its pipelines with the rest. For a target created after the game-frame warm already ran,
    /// call this, then [`GameFrameWarm::request`] (the engine re-warms the window renderer, off
    /// the frame thread) or warm the renderer that draws into it ([`Self::warm_renderer`]).
    /// Returns whether the target was new.
    pub fn warm_target(&self, format: wgpu::TextureFormat, samples: u32) -> bool {
        register_warm_target(format, samples)
    }
}

impl PipelineWarmQueue {
    /// Moves `renderer` onto a warm worker, runs `warm` on it there — typically
    /// [`crate::FrameRenderer::warm_scene`] on the frame the game draws first — and hands it
    /// back through the returned handle. The host keeps presenting (splash, loading screen)
    /// and draws nothing with this renderer until [`RendererWarm::take`] returns it.
    ///
    /// This is how a host that drives its own [`crate::FrameRenderer`] gets "0 pipeline creates
    /// after warm-up" without blocking its main thread; the windowed runtime does the same for
    /// [`crate::RuntimeRenderer`] through [`GameFrameWarm`]. Progress is the tier's progress
    /// ([`Self::progress`], agent `render.pipeline_warm`).
    pub fn warm_renderer(
        &self,
        tier: WarmTier,
        label: impl Into<String>,
        renderer: crate::FrameRenderer,
        warm: impl FnOnce(&mut crate::FrameRenderer, &RenderDevice) -> Result<(), String> + Send + 'static,
    ) -> RendererWarm {
        let slot = Arc::new(Mutex::new(None));
        let mut guard = ReturnGuard {
            slot: slot.clone(),
            renderer: Some(renderer),
            // Kept when the job is dropped unrun (cancelled, or the queue shut down).
            error: Some("canceled".into()),
            start: Instant::now(),
            pipelines_before: 0,
        };
        let job = self.push_job(
            tier,
            label.into(),
            Box::new(move |device: &RenderDevice| {
                guard.start = Instant::now();
                guard.pipelines_before = JOB_PIPELINES.with(std::cell::Cell::get);
                let result = match guard.renderer.as_mut() {
                    Some(renderer) => warm(renderer, device),
                    None => Ok(()),
                };
                guard.error = result.as_ref().err().cloned();
                drop(guard);
                result
            }),
        );
        RendererWarm { slot, job }
    }
}

/// The windowed runtime's warm of its own [`crate::RuntimeRenderer`] before a game frame
/// (`docs/specs/13-rendering.md` § 2c).
///
/// The first frame of a level compiles every pipeline its configuration uses (post stack,
/// shadow cascades, sky, fog, SSAO/SSGI/SSR, material graphs): seconds cold. With a warm
/// requested, the window path instead moves the runtime renderer onto a
/// [`PipelineWarmQueue`] worker ([`PipelineWarmQueue::warm_renderer`], tier
/// [`WarmTier::Map`]), renders the next renderable extracted frame there once at
/// [`WARM_FRAME_SIZE`]² in the window's draw format, and takes the renderer back: the game
/// frame after it compiles nothing. While the warm runs the window keeps presenting: the
/// [`crate::BootSplash`] (its fade-out waits for the warm, up to the splash's cap
/// [`crate::BootSplash::max_duration`], ADR-173) or the clear colour plus the UI overlay (a
/// loading screen, or the host's UI-only lobby once the cap has ended the splash). The scene
/// draws once the renderer is back; nothing compiles on the frame path meanwhile. Hosts read
/// [`crate::BootBackground::warm_done`] (or [`Self::is_pending`]) to know it is back. The warm
/// ends with one frame at the window's size, so the first visible frame allocates nothing.
///
/// **Host scene prepare (ADR-7883):** after spawning a map behind its loading page, a host calls
/// [`crate::SceneReadiness::prepare`]. The window then holds the scene ([`Self::holds_scene`])
/// while warm rounds cover the whole extracted frame, re-warming when the frame grows while a
/// round runs (no once-per-stretch lock-out while the prepare is open), and the
/// [`crate::SceneReady`] message is sent once the first frame that drew the scene was presented.
/// See [`crate::scene_ready`].
///
/// **Automatic at boot:** with a [`crate::BootSplash`], the first [`crate::BootSplash::finish`]
/// requests a warm ([`Self::on_splash_finish`], default on), so a game that shows a splash and
/// finishes it when its first scene is spawned gets a hitch-free first frame with no other call.
/// Later level loads call [`Self::request`] once the level is spawned.
///
/// **Automatic for a large new scene on a scene-less window (ADR-190):** when a window that
/// drew no scene on its last tick (a UI-only menu or lobby, a loading screen) gets a renderable
/// frame whose meshes the runtime renderer has not uploaded yet (at least
/// [`Self::new_scene_min_bytes`] or [`Self::new_scene_min_meshes`]: a map spawned behind the
/// menu), the frame is warmed on a worker first ([`Self::on_new_scene`], default on). The
/// window keeps presenting what it showed (clear colour plus the UI) instead of freezing while
/// the frame thread compiles pipelines and uploads the map; no visible scene is blanked. At
/// most one such warm per scene-less stretch; small scenes and scene changes on a window that
/// already shows a scene draw at once, as before.
///
/// While the renderer is away, systems that touch [`crate::RuntimeRenderer`] see a stand-in:
/// translucent layers (VFX) and the shared pass draw (terrain) installed on it are moved over,
/// and so are (since v0.1.8) a GI field, reflection probes, an interior scene, DDGI settings
/// and switch, and `reserve_instances` set on it (the last call wins). Other renderer settings
/// made on the stand-in are not; per-frame state is set again by the next frame.
/// The stand-in starts with copies of the renderer's VFX layers, GI field and reflection probes.
///
/// **Robust to the host's call order (ADR-166):** a request never blocks drawing until its
/// warm starts, and a warm starts only on a renderable frame (a view and a mesh). Once a game
/// frame can be drawn, the splash's warm never blanks the window: a splash request that has
/// not started when the splash is gone is dropped (the scene draws unwarmed); only an explicit
/// [`Self::request`] (behind a loading screen) may hold the renderer. Any request expires
/// after [`Self::request_grace`] without a renderable frame. A warm is skipped when the [`PipelineWarmQueue`] runs on another device than the current
/// [`RenderDevice`] (a host that replaced the device). A **watchdog** bounds a started warm to
/// [`Self::timeout`]: a job still queued is cancelled and the renderer comes back unwarmed; a
/// running job is abandoned and the stand-in keeps drawing, compiling what its first frames
/// need. When the abandoned job returns, its shared pass draw (terrain) moves to the stand-in
/// (unless the host set one on it meanwhile) and the rest of its renderer is dropped. Either
/// way a `warn` names the queue's state.
#[derive(Resource)]
pub struct GameFrameWarm {
    /// Request a warm when the boot splash starts finishing (default `true`).
    pub on_splash_finish: bool,
    /// Longest a started warm may take, queued plus running, before the watchdog gives up
    /// (default 15 s; `REGIA_GAME_WARM_TIMEOUT_S` overrides it). A running warm that is
    /// still building pipelines then gets more time ([`Self::progress_grace`], up to
    /// [`Self::max_timeout`]).
    pub timeout: Duration,
    /// A running warm past [`Self::timeout`] that built a pipeline within this time is still
    /// making progress (a slow or loaded machine), not hung: the watchdog lets it finish, up to
    /// [`Self::max_timeout`]. Abandoning it would make the stand-in compile the same pipelines
    /// on the frame thread. Default 8 s; `Duration::ZERO` restores the plain timeout.
    pub progress_grace: Duration,
    /// Longest a running warm may take while it keeps making progress (default 60 s;
    /// `REGIA_GAME_WARM_MAX_S` overrides it). Never below [`Self::timeout`].
    pub max_timeout: Duration,
    /// How long a request waits for a renderable frame before it expires (default 10 s).
    pub request_grace: Duration,
    /// Warm a large new scene that appears on a window that drew no scene on its last tick
    /// before drawing it (default `true`; `REGIA_SCENE_WARM=0`, `false`, `no` or `off` turns it
    /// off). See the type's docs. On an integrated GPU
    /// ([`crate::low_memory::integrated_limits_apply`]) the warm is skipped unless
    /// `REGIA_SCENE_WARM=1`, `true`, `yes` or `on` (ADR-261: it uploads the whole map at once, which a
    /// shared-memory GPU may not survive); setting this `false` turns it off everywhere.
    pub on_new_scene: bool,
    /// Mesh bytes the runtime renderer has not uploaded that make a new scene "large"
    /// (default [`NEW_SCENE_MIN_BYTES`]).
    pub new_scene_min_bytes: u64,
    /// Distinct meshes the runtime renderer has not uploaded that make a new scene "large"
    /// (default [`NEW_SCENE_MIN_MESHES`]).
    pub new_scene_min_meshes: usize,
    /// `REGIA_SCENE_WARM=1` (or `true`, `yes`, `on`): warm new scenes on integrated GPUs too.
    scene_warm_forced: bool,
    /// The warm queue's device gets the integrated-GPU defaults, with the adapter it was read for
    /// (re-read when the queue's device is another adapter).
    integrated: Option<(wgpu::AdapterInfo, bool)>,
    /// The integrated-GPU skip was logged.
    noted_integrated: bool,
    /// The window path drew a scene on its last tick.
    scene_shown: bool,
    /// A warm already ran (or was requested) in the current scene-less stretch.
    scene_warm_spent: bool,
    /// New-scene warms requested so far.
    scene_warms: u64,
    requested: bool,
    /// When the pending request was made.
    requested_at: Option<Instant>,
    /// The pending request came from the splash finishing (it expires with the splash).
    request_from_splash: bool,
    splash_requested: bool,
    in_flight: Option<InFlight>,
    /// The watchdog logged that it waits for a slow, progressing warm (once per warm).
    noted_slow: bool,
    /// A running warm the watchdog abandoned: when it returns, its shared pass draw (terrain)
    /// moves to the renderer that took over ([`crate::FrameRenderer::salvage_shared_draw`]).
    abandoned: Option<RendererWarm>,
    last: Option<GameWarmReport>,
    first_frame_ms: Option<f32>,
    /// The device-mismatch warning was logged.
    warned_device: bool,
    /// Explicit [`Self::request`] calls so far.
    explicit_requests: u64,
    /// Warms started, and warms finished (or abandoned) with a report.
    started: u64,
    finished: u64,
    /// The host's open scene prepare ([`crate::SceneReadiness::prepare`], ADR-7883).
    pub(crate) prepare: Option<crate::scene_ready::PrepareRun>,
    /// The pending request is the prepare's next warm round.
    prepare_request: bool,
    /// Window-path render ticks so far (the readiness's frame numbers).
    ticks: u64,
    /// Texture-view frames submitted so far on the frame thread ([`Self::note_offscreen_frame`]).
    offscreen_frames: u64,
    /// A headless prepare turned resident: the offscreen frame count then and when (ADR-8122).
    headless_resident_at: Option<(u64, Instant)>,
}

/// A started [`GameFrameWarm`].
struct InFlight {
    handle: RendererWarm,
    since: Instant,
    /// [`map_tier_pipelines`] when the warm started, and the last count the watchdog saw with
    /// when it changed: the warm's progress.
    pipelines_at_start: u64,
    progress: (u64, Instant),
    /// Mesh data the renderer did not hold when the warm started: `(bytes, meshes)`.
    uploads: (u64, usize),
    /// A round of the host's scene prepare (ADR-7883).
    for_prepare: bool,
}

/// What [`GameFrameWarm::host_tick`] decided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostTick {
    /// The scene may be drawn this tick (no warm runs and no prepare holds it).
    pub draw: bool,
    /// Distinct meshes of the frame the runtime renderer does not hold yet (counted on the first
    /// scene frame after a warm or of a prepare; 0 otherwise).
    pub pending_meshes: usize,
}

/// Default [`GameFrameWarm::request_grace`].
pub const GAME_WARM_REQUEST_GRACE: Duration = Duration::from_secs(10);

/// Default [`GameFrameWarm::timeout`].
pub const GAME_WARM_TIMEOUT: Duration = Duration::from_secs(15);

/// Default [`GameFrameWarm::progress_grace`].
pub const GAME_WARM_PROGRESS_GRACE: Duration = Duration::from_secs(8);

/// Default [`GameFrameWarm::max_timeout`].
pub const GAME_WARM_MAX_TIMEOUT: Duration = Duration::from_secs(30);

/// Pipelines created by map-tier warm jobs so far in this process (the game-frame warm's
/// tier): the [`GameFrameWarm`] watchdog's progress signal.
fn map_tier_pipelines() -> u64 {
    TIER_PIPELINES[WarmTier::Map as usize].load(Ordering::Relaxed)
}

/// Drops `renderer` on a short-lived helper thread (inline when none can start): a renderer
/// with a map's uploads takes long to free.
/// Stand-in renderers a finished warm's swap handed to a worker to free (process total, tests).
static STAND_INS_DROPPED_OFF_THREAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`STAND_INS_DROPPED_OFF_THREAD`] now.
#[cfg(test)]
fn stand_ins_dropped_off_thread() -> u64 {
    STAND_INS_DROPPED_OFF_THREAD.load(std::sync::atomic::Ordering::Relaxed)
}

fn drop_off_thread(renderer: crate::FrameRenderer) {
    let slot = Arc::new(Mutex::new(Some(renderer)));
    let theirs = slot.clone();
    // Registered (ADR-287): the end of the process waits for the renderer's frees.
    let spawned = regia_platform::GpuWorkerBuilder::new("regia-renderer-drop").spawn(move |_| {
        let renderer = theirs.lock().ok().and_then(|mut s| s.take());
        drop(renderer);
    });
    if spawned.is_err() {
        let renderer = slot.lock().ok().and_then(|mut s| s.take());
        drop(renderer);
    }
}

/// A positive number of seconds from the environment variable `name`.
fn env_seconds(name: &str) -> Option<Duration> {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(Duration::from_secs_f32)
}

/// Default [`GameFrameWarm::new_scene_min_bytes`]: 32 MiB of mesh data not yet uploaded.
pub const NEW_SCENE_MIN_BYTES: u64 = 32 << 20;

/// Default [`GameFrameWarm::new_scene_min_meshes`]: 256 distinct meshes not yet uploaded.
pub const NEW_SCENE_MIN_MESHES: usize = 256;

impl Default for GameFrameWarm {
    fn default() -> Self {
        let timeout = env_seconds("REGIA_GAME_WARM_TIMEOUT_S").unwrap_or(GAME_WARM_TIMEOUT);
        let max_timeout = env_seconds("REGIA_GAME_WARM_MAX_S").unwrap_or(GAME_WARM_MAX_TIMEOUT);
        let scene_warm = regia_core::brand::env_var("REGIA_SCENE_WARM").ok();
        let scene_warm = crate::low_memory::parse_env_flag(scene_warm.as_deref());
        let on_new_scene = scene_warm != Some(false);
        let scene_warm_forced = scene_warm == Some(true);
        Self {
            on_splash_finish: true,
            timeout,
            progress_grace: GAME_WARM_PROGRESS_GRACE,
            max_timeout,
            request_grace: GAME_WARM_REQUEST_GRACE,
            on_new_scene,
            new_scene_min_bytes: NEW_SCENE_MIN_BYTES,
            new_scene_min_meshes: NEW_SCENE_MIN_MESHES,
            scene_warm_forced,
            integrated: None,
            noted_integrated: false,
            scene_shown: false,
            scene_warm_spent: false,
            scene_warms: 0,
            requested: false,
            requested_at: None,
            request_from_splash: false,
            splash_requested: false,
            in_flight: None,
            noted_slow: false,
            abandoned: None,
            last: None,
            first_frame_ms: None,
            warned_device: false,
            explicit_requests: 0,
            started: 0,
            finished: 0,
            prepare: None,
            prepare_request: false,
            ticks: 0,
            offscreen_frames: 0,
            headless_resident_at: None,
        }
    }
}

/// The last finished [`GameFrameWarm`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GameWarmReport {
    /// Wall time of the warm on the worker.
    pub ms: f32,
    /// Pipelines it created.
    pub pipelines: u64,
    /// Its error, if any (the frame then compiles what is missing itself).
    pub error: Option<String>,
    /// The watchdog gave up on it ([`GameFrameWarm::timeout`]).
    pub timed_out: bool,
    /// Megabytes of mesh data the warm uploaded (the frame's meshes the renderer did not hold
    /// when the warm started).
    pub mb_uploaded: f32,
    /// Distinct meshes among them.
    pub meshes: usize,
    /// Which warm this is (1 for the first one that finished): a reader that polls tells a new
    /// report from the one it saw.
    pub seq: u64,
    /// The host's map id when the warm was a round of [`crate::SceneReadiness::prepare`]
    /// (ADR-7883); `None` for the splash's, an explicit or a new-scene warm.
    pub map_id: Option<String>,
    /// Whether the warm covered the scene: `Some(true)` when the first scene frame after it found
    /// every mesh resident and created no pipeline, `Some(false)` when that frame still uploaded
    /// or compiled, when another warm started before a frame was drawn, or when the watchdog gave
    /// up on it. `None` until known (telemetry's `scene_load_end` waits for it).
    pub complete: Option<bool>,
}

impl GameFrameWarm {
    /// Warms the runtime renderer on the next renderable extracted frame before drawing it.
    ///
    /// Call it behind a loading screen once the level is spawned: the window draws no scene
    /// while the warm runs (at most [`Self::timeout`]). A request that sees no renderable
    /// frame within [`Self::request_grace`] expires.
    pub fn request(&mut self) {
        self.requested = true;
        self.requested_at = Some(Instant::now());
        self.request_from_splash = false;
        self.explicit_requests += 1;
    }

    /// How many explicit [`Self::request`] calls were made (level loads, a preset change's
    /// re-warm); the splash's own request is not counted. The first-launch quality probe
    /// aborts when this moves after it started (ADR-173).
    #[must_use]
    pub fn explicit_requests(&self) -> u64 {
        self.explicit_requests
    }

    /// Drops the pending request (not a running warm).
    fn drop_request(&mut self, why: &str) {
        if self.requested {
            tracing::info!(target: "regia_render", "game frame warm request dropped: {why}");
            crate::boot::mark_named("game_warm_request_dropped");
        }
        self.requested = false;
        self.requested_at = None;
        self.request_from_splash = false;
        self.prepare_request = false;
    }

    /// Whether a warm is requested or running. Only a running warm ([`Self::is_warming`])
    /// keeps the window from drawing the scene; a request waits for a renderable frame.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.requested || self.in_flight.is_some()
    }

    /// Whether the runtime renderer is on a warm worker now.
    #[must_use]
    pub fn is_warming(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The last finished warm.
    #[must_use]
    pub fn last_report(&self) -> Option<&GameWarmReport> {
        self.last.as_ref()
    }

    /// CPU time of the window path's first scene frame (after the last warm when there was
    /// one): the first-game-frame hitch, `None` until such a frame is drawn.
    #[must_use]
    pub fn first_frame_ms(&self) -> Option<f32> {
        self.first_frame_ms
    }

    /// A request made while a warm ran (a map queued behind a slow boot warm: the lobby is up
    /// at 1.5 s, ADR-173) gets its grace from the moment the renderer is back, not from the
    /// request, so the watchdog never drops it on the tick the earlier warm returns.
    fn restart_grace(&mut self) {
        if self.requested {
            self.requested_at = Some(Instant::now());
        }
    }

    /// `Render` (window path): returns a finished warm's renderer to `runtime`.
    pub(crate) fn poll(&mut self, runtime: &mut crate::FrameRenderer) {
        if let Some(late) = self.abandoned.as_ref().and_then(RendererWarm::take) {
            self.abandoned = None;
            if let Some(mut renderer) = late.renderer {
                // The pipeline sets the stand-in has not built yet (a stand-in that drew no
                // scene meanwhile) are taken instead of compiled on the frame thread.
                let adopted = runtime.adopt_warm_pipelines(&mut renderer);
                let salvaged = runtime.salvage_shared_draw(&mut renderer);
                tracing::info!(
                    target: "regia_render",
                    "abandoned game frame warm returned after {:.1} s ({} pipelines){}{}",
                    late.ms / 1000.0,
                    late.pipelines,
                    if salvaged { "; its shared pass draw (terrain) moved to the renderer" } else { "" },
                    if adopted { "; the renderer adopted its pipelines" } else { "" }
                );
                // The rest holds the map's uploads as the warm made them: freeing them costs
                // the frame thread up to a second on a big map.
                drop_off_thread(renderer);
            }
        }
        let Some(done) = self.in_flight.as_ref().and_then(|f| f.handle.take()) else {
            return;
        };
        let flight = self.in_flight.take();
        self.restart_grace();
        let for_prepare = flight.as_ref().is_some_and(|f| f.for_prepare);
        if for_prepare && let Some(p) = self.prepare.as_mut() {
            let uploads = flight.as_ref().map_or((0, 0), |f| f.uploads);
            p.absorb(
                done.ms,
                done.pipelines,
                uploads,
                done.error.as_deref(),
                Instant::now(),
            );
        }
        let map_id = self
            .prepare
            .as_ref()
            .filter(|_| for_prepare)
            .map(|p| p.map_id.clone());
        if let Some(mut warmed) = done.renderer {
            // A frame-thread span (v0.5.1): a hitch on this frame names the swap, not a worker.
            let _span = tracing::info_span!("game_warm_swap").entered();
            let mut stand_in = std::mem::replace(runtime, crate::FrameRenderer::new());
            warmed.absorb_stand_in(&mut stand_in);
            *runtime = warmed;
            // The stand-in drew the splash and menus while the warm ran: its window-sized targets
            // and caches are freed on a worker, not on the frame that swaps (v0.5.1, DO27 launch
            // hitch; the abandoned-warm path above already did this).
            drop_off_thread(stand_in);
            STAND_INS_DROPPED_OFF_THREAD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(err) = &done.error {
            tracing::warn!(target: "regia_render", "game frame warm failed: {err}");
        }
        tracing::info!(
            target: "regia_render",
            "game frame warm: {} pipelines in {:.1} ms",
            done.pipelines,
            done.ms
        );
        crate::boot::mark_named("game_warm_done");
        self.finished += 1;
        let uploads = flight.map_or((0, 0), |f| f.uploads);
        self.last = Some(GameWarmReport {
            ms: done.ms,
            pipelines: done.pipelines,
            error: done.error,
            timed_out: false,
            mb_uploaded: (uploads.0 as f64 / f64::from(1u32 << 20)) as f32,
            meshes: uploads.1,
            seq: self.finished,
            map_id,
            complete: None,
        });
        self.first_frame_ms = None;
    }

    /// `Render` (window path): the watchdog. A warm started more than [`Self::timeout`] ago is
    /// given up: cancelled with the renderer handed back when still queued, abandoned (the
    /// stand-in keeps drawing) when running.
    pub(crate) fn watchdog(&mut self, queue: &PipelineWarmQueue, runtime: &mut crate::FrameRenderer) {
        self.watchdog_at(queue, runtime, Instant::now());
    }

    pub(crate) fn watchdog_at(
        &mut self,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        now: Instant,
    ) {
        self.watchdog_with(queue, runtime, now, map_tier_pipelines());
    }

    /// [`Self::watchdog`] at `now`, with `pipelines` the map-tier pipeline count (the running
    /// warm's progress).
    fn watchdog_with(
        &mut self,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        now: Instant,
        pipelines: u64,
    ) {
        if let Some(flight) = self.in_flight.as_mut()
            && pipelines != flight.progress.0
        {
            flight.progress = (pipelines, now);
        }
        if self.requested
            && self.in_flight.is_none()
            && self
                .requested_at
                .is_some_and(|at| now.saturating_duration_since(at) >= self.request_grace)
        {
            let grace = self.request_grace.as_secs_f32();
            self.drop_request(&format!("no renderable frame within {grace:.1} s"));
        }
        let Some(flight) = self.in_flight.as_ref() else {
            return;
        };
        let age = now.saturating_duration_since(flight.since);
        if age < self.timeout {
            return;
        }
        if flight.handle.is_done() {
            self.poll(runtime);
            return;
        }
        let for_prepare = flight.for_prepare;
        let state = queue.describe();
        let secs = age.as_secs_f32();
        let built = flight.progress.0.saturating_sub(flight.pipelines_at_start);
        let idle = now.saturating_duration_since(flight.progress.1);
        let progressing = !self.progress_grace.is_zero()
            && idle < self.progress_grace
            && age < self.max_timeout.max(self.timeout);
        if flight.handle.cancel(queue) {
            tracing::warn!(
                target: "regia_render",
                "game frame warm still queued after {secs:.1} s: canceled, renderer returned unwarmed \
                 (its first frames compile); warm queue: {state}"
            );
            self.poll(runtime);
            crate::boot::mark_named("game_warm_cancelled");
            if let Some(last) = self.last.as_mut() {
                last.timed_out = true;
                last.error = Some(format!("canceled by the watchdog after {secs:.1} s in the queue"));
                last.complete = Some(false);
            }
            if for_prepare && let Some(p) = self.prepare.as_mut() {
                p.give_up(format!(
                    "its warm was canceled by the watchdog after {secs:.1} s in the queue"
                ));
            }
        } else if progressing {
            // Slow, not hung (a loaded machine): abandoning now would only move the same
            // compiles onto the frame thread. The window keeps presenting meanwhile.
            if !self.noted_slow {
                self.noted_slow = true;
                tracing::info!(
                    target: "regia_render",
                    "game frame warm still building after {secs:.1} s ({built} pipelines so far, the \
                     last {:.1} s ago): past its {:.0} s timeout but progressing, so the watchdog waits \
                     (up to {:.0} s in all, GameFrameWarm::max_timeout / REGIA_GAME_WARM_MAX_S)",
                    idle.as_secs_f32(),
                    self.timeout.as_secs_f32(),
                    self.max_timeout.max(self.timeout).as_secs_f32()
                );
            }
        } else {
            let why = if age >= self.max_timeout.max(self.timeout) && !self.progress_grace.is_zero() {
                format!(
                    "it was still building pipelines but reached GameFrameWarm::max_timeout ({:.0} s, \
                     REGIA_GAME_WARM_MAX_S)",
                    self.max_timeout.max(self.timeout).as_secs_f32()
                )
            } else {
                format!(
                    "no pipeline built for {:.1} s past its {:.0} s timeout (GameFrameWarm::timeout, \
                     REGIA_GAME_WARM_TIMEOUT_S)",
                    idle.as_secs_f32(),
                    self.timeout.as_secs_f32()
                )
            };
            tracing::warn!(
                target: "regia_render",
                "game frame warm abandoned by its watchdog after {secs:.1} s ({built} pipelines built): \
                 {why}. The scene now draws with a stand-in renderer (it has the GI field, reflection \
                 probes and VFX): pipelines the warm did not finish are compiled on the frame thread \
                 by its first frames (pipeline-cache hits for those the warm built), and the terrain \
                 comes back when the job returns; warm queue: {state}"
            );
            let uploads = self.in_flight.as_ref().map_or((0, 0), |f| f.uploads);
            self.abandoned = self.in_flight.take().map(|flight| flight.handle);
            let map_id = if for_prepare && let Some(p) = self.prepare.as_mut() {
                p.give_up(format!(
                    "its warm was abandoned by the watchdog after {secs:.1} s"
                ));
                Some(p.map_id.clone())
            } else {
                None
            };
            self.restart_grace();
            crate::boot::mark_named("game_warm_abandoned");
            self.finished += 1;
            self.last = Some(GameWarmReport {
                ms: secs * 1000.0,
                pipelines: 0,
                error: Some(format!("abandoned by the watchdog after {secs:.1} s running")),
                timed_out: true,
                mb_uploaded: 0.0,
                meshes: uploads.1,
                seq: self.finished,
                map_id,
                complete: Some(false),
            });
            self.first_frame_ms = None;
        }
    }

    /// `Render` (window path): turns a finishing splash into a request (once).
    ///
    /// `gone`: no splash covers the window any more (none, or it has faded). A splash request
    /// that has not started by then is dropped: once a game frame can be drawn, a warm must
    /// never blank the window (ADR-166); only an explicit [`Self::request`] may.
    pub(crate) fn note_splash(&mut self, finishing: bool, gone: bool) {
        if finishing && !gone && self.on_splash_finish && !self.splash_requested {
            self.splash_requested = true;
            self.requested = true;
            self.requested_at = Some(Instant::now());
            self.request_from_splash = true;
        }
        if gone && self.requested && self.request_from_splash {
            self.drop_request("the splash is gone before a renderable frame (the scene draws unwarmed)");
        }
    }

    /// `Render` (window path): starts a requested warm on `frame`, moving `runtime` to a worker
    /// and leaving a stand-in.
    ///
    /// Nothing starts until `frame` is renderable (a view and at least one mesh: an empty frame
    /// has nothing to warm). `device_id` is the current [`RenderDevice`]'s: when the queue's
    /// workers run on another device (the host replaced the device), the request is dropped
    /// with a warning instead of warming across devices. `window` is the window's size: the
    /// warm ends with one frame at that size, so the first visible frame allocates nothing
    /// (ADR-173; `None` = the 64×64 frames only).
    pub(crate) fn start(
        &mut self,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        (format, readable): (wgpu::TextureFormat, bool),
        device_id: u64,
        window: Option<(u32, u32)>,
    ) {
        if !self.requested || self.in_flight.is_some() || !frame.is_renderable() {
            return;
        }
        let Some(queue_device) = queue.device_id() else {
            return;
        };
        if queue_device != device_id {
            if self.prepare_request
                && let Some(p) = self.prepare.as_mut()
            {
                p.give_up("the warm queue runs on another device".into());
            }
            self.drop_request("the warm queue runs on another device");
            if !self.warned_device {
                self.warned_device = true;
                tracing::warn!(
                    target: "regia_render",
                    "game frame warm skipped: the pipeline warm queue runs on another device than the \
                     current RenderDevice (a host that replaces the device must recreate the \
                     PipelineWarmQueue)"
                );
            }
            return;
        }
        self.requested = false;
        self.requested_at = None;
        self.request_from_splash = false;
        // An older abandoned warm's terrain belongs to the map it warmed, not to this one.
        self.abandoned = None;
        crate::boot::mark_named("game_warm_start");
        // A warm that returned with no scene frame drawn since did not cover what comes next.
        if let Some(last) = self.last.as_mut()
            && last.complete.is_none()
        {
            last.complete = Some(false);
        }
        let for_prepare = std::mem::take(&mut self.prepare_request) && self.prepare.is_some();
        if for_prepare && let Some(p) = self.prepare.as_mut() {
            p.rounds += 1;
            p.cover = crate::scene_ready::SceneCover::of(frame);
            p.phase = crate::scene_ready::PreparePhase::Warming;
        }
        let uploads = runtime.pending_mesh_uploads(frame, (u64::MAX, usize::MAX));
        self.started += 1;
        let mut stand_in = crate::RuntimeRenderer::default().0;
        runtime.share_installs_with(&mut stand_in);
        let renderer = std::mem::replace(runtime, stand_in);
        let frame = frame.clone();
        let handle = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |renderer, device| {
            if frame.window_views().is_empty() {
                // A headless host's render-to-texture cameras (ADR-8122).
                return renderer
                    .warm_texture_views(device, &frame)
                    .map_err(|e| e.to_string());
            }
            renderer
                .warm_extracted_at(device, &frame, format, readable, window)
                .map_err(|e| e.to_string())
        });
        let pipelines = map_tier_pipelines();
        self.noted_slow = false;
        self.in_flight = Some(InFlight {
            handle,
            since: Instant::now(),
            pipelines_at_start: pipelines,
            progress: (pipelines, Instant::now()),
            uploads,
            for_prepare,
        });
    }

    /// The clean exit: takes the renderers a finished warm left in its handles (one in flight
    /// that finished, an abandoned one that returned), so they are freed with the runtime
    /// renderer, off the event-loop thread. A warm still running keeps its renderer.
    pub(crate) fn take_parked_renderers(&mut self) -> Vec<crate::FrameRenderer> {
        let handles = [self.in_flight.take().map(|f| f.handle), self.abandoned.take()];
        handles
            .into_iter()
            .flatten()
            .filter_map(|handle| handle.take().and_then(|w| w.renderer))
            .collect()
    }

    /// Warms started so far (telemetry's `scene_load_start`).
    #[must_use]
    pub fn warms_started(&self) -> u64 {
        self.started
    }

    /// New-scene warms requested so far ([`Self::on_new_scene`]).
    #[must_use]
    pub fn scene_warms(&self) -> u64 {
        self.scene_warms
    }

    /// Whether a new scene would be warmed on a device whose integrated-GPU defaults are
    /// `integrated` ([`crate::low_memory::integrated_limits_apply`]): [`Self::on_new_scene`], and
    /// on an integrated GPU only with `REGIA_SCENE_WARM=1`.
    #[must_use]
    pub fn new_scene_warm_on(&self, integrated: bool) -> bool {
        self.on_new_scene && (!integrated || self.scene_warm_forced)
    }

    /// [`Self::new_scene_warm_on`] for the started `queue`'s device (read once), logging the
    /// integrated-GPU skip once.
    fn new_scene_warm_allowed(&mut self, queue: &PipelineWarmQueue) -> bool {
        let info = queue
            .lock()
            .device
            .as_ref()
            .map(|device| (device.adapter.get_info(), device.adapter.clone()));
        let integrated = match info {
            None => false,
            Some((info, adapter)) => match &self.integrated {
                Some((seen, integrated)) if *seen == info => *integrated,
                _ => {
                    let integrated = crate::low_memory::integrated_limits_apply_to(
                        &info,
                        crate::quality::AdapterProfile::from_adapter(&adapter).vram_mb,
                    );
                    self.integrated = Some((info, integrated));
                    integrated
                }
            },
        };
        let on = self.new_scene_warm_on(integrated);
        if !on && !self.noted_integrated {
            self.noted_integrated = true;
            tracing::info!(
                target: "regia_render",
                "integrated GPU: the new-scene warm is off (it uploads a whole map at once); \
                 REGIA_SCENE_WARM=1 turns it on"
            );
        }
        on
    }

    /// `Render` (window path), every tick before [`Self::start`]: requests a warm for a large
    /// new scene on a window that drew no scene on its last tick ([`Self::on_new_scene`]).
    ///
    /// `queue` is the started warm queue (`None`: no warm possible, nothing is requested),
    /// `frame` this tick's extracted frame, `splash_gone` whether no splash covers the window
    /// (while one does, the splash's own request applies).
    pub(crate) fn note_scene(
        &mut self,
        queue: Option<&PipelineWarmQueue>,
        runtime: &crate::FrameRenderer,
        frame: Option<&crate::ExtractedFrame>,
        splash_gone: bool,
    ) {
        let shown = std::mem::replace(&mut self.scene_shown, false);
        if self
            .prepare
            .as_ref()
            .is_some_and(crate::scene_ready::PrepareRun::holds_scene)
        {
            // The host's prepare decides this stretch (no new-scene lock-out while it is open).
            self.drive_prepare(queue, runtime, frame);
            return;
        }
        if shown {
            // A scene was on screen: the next scene-less stretch may warm again.
            self.scene_warm_spent = false;
            return;
        }
        if let Some(flight) = self.in_flight.as_ref() {
            // Another warm covers this stretch (a level-load request, the splash's), if it warmed
            // a large scene itself. One that started with nothing to upload (DO27 tester A2: a
            // backdrop already resident) does not: the map arriving meanwhile is warmed after it.
            let (bytes, meshes) = flight.uploads;
            if bytes >= self.new_scene_min_bytes || meshes >= self.new_scene_min_meshes {
                self.scene_warm_spent = true;
            }
            return;
        }
        if self.requested {
            // A pending request warms the frame itself once it is renderable; one that expires
            // instead (the splash's, dropped when the splash is gone: DO27's order) must not
            // use up the stretch.
            return;
        }
        if self.scene_warm_spent || !self.on_new_scene || !splash_gone {
            return;
        }
        let (Some(queue), Some(frame)) = (queue, frame) else {
            return;
        };
        if !queue.is_started() || queue.is_shutting_down() || !frame.is_renderable() {
            return;
        }
        if !self.new_scene_warm_allowed(queue) {
            return;
        }
        let stop = (self.new_scene_min_bytes, self.new_scene_min_meshes);
        let (bytes, meshes) = runtime.pending_mesh_uploads(frame, stop);
        if bytes < stop.0 && meshes < stop.1 {
            return;
        }
        self.scene_warm_spent = true;
        self.scene_warms += 1;
        self.requested = true;
        self.requested_at = Some(Instant::now());
        self.request_from_splash = false;
        tracing::info!(
            target: "regia_render",
            "new scene on a scene-less window ({:.0} MB in {meshes} meshes not uploaded yet, or more): \
             warming it on a worker; the window keeps presenting the UI",
            bytes as f64 / f64::from(1u32 << 20)
        );
        crate::boot::mark_named("scene_warm_request");
    }

    /// One window-path tick of the warm for a host that draws the scene into its own target
    /// instead of a window (a benchmark such as `boot_gpu`, an embedding host): takes the newest
    /// prepare from `readiness`, then polls, runs the watchdog, the new-scene check and the
    /// start, as the window path does before its draw. `target` is the draw format, whether it
    /// can be read back, and its size. After drawing and presenting a tick whose
    /// [`HostTick::draw`] is true, call [`Self::host_presented`].
    pub fn host_tick(
        &mut self,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        (format, readable, window): (wgpu::TextureFormat, bool, (u32, u32)),
        device: &RenderDevice,
        readiness: Option<&mut crate::SceneReadiness>,
    ) -> HostTick {
        if let Some(readiness) = readiness
            && let Some((seq, map_id)) = readiness.take_request()
        {
            self.open_prepare(seq, map_id, Instant::now());
        }
        self.begin_tick();
        self.poll(runtime);
        self.watchdog(queue, runtime);
        self.note_scene(Some(queue), runtime, Some(frame), true);
        self.start(
            queue,
            runtime,
            frame,
            (format, readable),
            device.device_id(),
            Some(window),
        );
        let draw = frame.is_renderable() && !self.holds_scene();
        let pending_meshes = if draw && self.wants_frame_check() {
            runtime.pending_mesh_uploads(frame, (u64::MAX, usize::MAX)).1
        } else {
            0
        };
        HostTick { draw, pending_meshes }
    }

    /// After the host drew and presented the scene of a [`Self::host_tick`] that allowed it:
    /// `tick` is that call's answer, `pipelines` the pipelines the draw created on this thread
    /// ([`crate::frame_path_counters`]). Publishes the prepare into `readiness` and returns its
    /// [`crate::SceneReady`] on the presented edge.
    pub fn host_presented(
        &mut self,
        tick: HostTick,
        pipelines: u64,
        readiness: Option<&mut crate::SceneReadiness>,
    ) -> Option<crate::SceneReady> {
        self.note_frame_with(
            0.0,
            crate::scene_ready::SceneFrameCheck {
                pending_meshes: tick.pending_meshes,
                pipelines,
                present: crate::scene_ready::PresentNote::Presented,
            },
        );
        readiness.and_then(|r| crate::scene_ready::publish(r, self))
    }

    /// Whether the next drawn scene frame should report what it still uploads: the last warm's
    /// coverage is not known yet, or an open prepare waits for its first frame.
    #[must_use]
    pub(crate) fn wants_frame_check(&self) -> bool {
        self.last.as_ref().is_some_and(|r| r.complete.is_none())
            || self
                .prepare
                .as_ref()
                .is_some_and(|p| p.phase == crate::scene_ready::PreparePhase::Resident)
    }

    /// Opens the host's scene prepare `seq` for `map_id` ([`crate::SceneReadiness::prepare`]):
    /// replaces one still open.
    pub(crate) fn open_prepare(&mut self, seq: u64, map_id: String, now: Instant) {
        tracing::info!(target: "regia_render", "scene prepare \"{map_id}\" ({seq}): the scene is held until it is warmed");
        crate::boot::mark_named("scene_prepare");
        self.prepare = Some(crate::scene_ready::PrepareRun::new(seq, map_id, now));
        self.prepare_request = false;
        self.headless_resident_at = None;
    }

    /// `Render` (`render_texture_views`): the frame thread rendered and submitted texture-view
    /// frames this tick.
    pub(crate) fn note_offscreen_frame(&mut self) {
        self.offscreen_frames += 1;
    }

    /// The host's open (or last) scene prepare's state and numbers
    /// ([`crate::SceneReadiness::prepare`]; the resource holds the same once published).
    #[must_use]
    pub fn prepare_status(&self) -> Option<crate::SceneReadyStatus> {
        self.prepare.as_ref().map(crate::scene_ready::PrepareRun::status)
    }

    /// Whether the window must not draw the scene this tick: a warm runs, or the host's prepare
    /// has not decided that the scene is resident yet (it keeps presenting the UI).
    #[must_use]
    pub fn holds_scene(&self) -> bool {
        self.in_flight.is_some()
            || self
                .prepare
                .as_ref()
                .is_some_and(crate::scene_ready::PrepareRun::holds_scene)
    }

    /// `Render` (window path), first thing every tick: counts the tick and moves a prepare whose
    /// first frame is queued on the frame generation present thread to `presented` once that
    /// thread has presented it.
    pub(crate) fn begin_tick(&mut self) {
        let (done, flushes) = crate::scene_ready::present_progress();
        self.begin_tick_with(done, flushes, Instant::now());
    }

    /// [`Self::begin_tick`] with the present thread's `(presented, flushes)` counts.
    pub(crate) fn begin_tick_with(&mut self, done: u64, flushes: u64, now: Instant) {
        self.ticks += 1;
        #[cfg(not(any(feature = "frame-gen", test)))]
        let _ = (done, flushes, now);
        #[cfg(any(feature = "frame-gen", test))]
        let ticks = self.ticks;
        #[cfg(any(feature = "frame-gen", test))]
        if let Some(p) = self.prepare.as_mut()
            && let crate::scene_ready::PreparePhase::Queued { ticket, flushes: at } = p.phase
        {
            if done >= ticket {
                p.presented(ticks, true, now);
            } else if flushes != at {
                // The image was dropped (a suspend): the next drawn frame is the first.
                p.phase = crate::scene_ready::PreparePhase::Resident;
            }
        }
    }

    /// `Render` (window path) on a tick that presents nothing (hidden window): a resident prepare
    /// whose scene could draw is `presented`, with nothing presented to the window.
    pub(crate) fn note_unpresented_scene(&mut self, renderable: bool) {
        if renderable
            && !self.holds_scene()
            && let Some(p) = self.prepare.as_mut()
            && p.phase == crate::scene_ready::PreparePhase::Resident
        {
            p.presented(self.ticks, false, Instant::now());
        }
    }

    /// `Render` on a headless app (ADR-8122). With a render device and a started warm queue
    /// (`warm`), an open prepare runs the same warm rounds as on the window path (a frame with no
    /// window view warms its texture views, `FrameRenderer::warm_texture_views`). Once it is
    /// resident it is `presented` (with nothing presented to a window) on the first tick after
    /// that on which the frame thread submitted a texture-view frame
    /// ([`Self::note_offscreen_frame`]), or at once when the frame has no texture view (nothing
    /// renders it on the frame thread; the warm round drew and submitted it). A resident prepare
    /// that sees no submitted frame within [`Self::request_grace`] gives up (`timed_out`).
    /// Without a device nothing warms or draws: [`Self::note_headless_tick`].
    pub(crate) fn headless_tick(
        &mut self,
        warm: Option<(&PipelineWarmQueue, &mut crate::FrameRenderer, &RenderDevice)>,
        frame: Option<&crate::ExtractedFrame>,
        now: Instant,
    ) {
        let Some((queue, runtime, device)) = warm else {
            self.note_headless_tick(frame.is_some_and(crate::ExtractedFrame::is_renderable), now);
            return;
        };
        self.ticks += 1;
        // A round (or an abandoned one) returns its renderer whatever the prepare's state.
        self.poll(runtime);
        self.watchdog(queue, runtime);
        if self
            .prepare
            .as_ref()
            .is_some_and(crate::scene_ready::PrepareRun::holds_scene)
        {
            self.drive_prepare(Some(queue), runtime, frame);
            if self.prepare_request
                && let Some(frame) = frame
            {
                // The texture views' target format (`render_texture_view`), never read back.
                self.start(
                    queue,
                    runtime,
                    frame,
                    (wgpu::TextureFormat::Rgba8UnormSrgb, false),
                    device.device_id(),
                    None,
                );
            }
        }
        let (ticks, grace, frames) = (self.ticks, self.request_grace, self.offscreen_frames);
        let Some(p) = self.prepare.as_mut() else {
            return;
        };
        if p.phase != crate::scene_ready::PreparePhase::Resident {
            return;
        }
        if frame.is_none_or(|f| f.texture_views().is_empty()) {
            p.presented(ticks, false, now);
            return;
        }
        match self.headless_resident_at {
            None => self.headless_resident_at = Some((frames, now)),
            Some((at, _)) if frames > at => p.presented(ticks, false, now),
            Some((_, since)) if now.saturating_duration_since(since) >= grace => {
                p.give_up(format!(
                    "no offscreen frame submitted within {:.1} s of the scene being resident",
                    grace.as_secs_f32()
                ));
                p.presented(ticks, false, now);
            }
            Some(_) => {}
        }
    }

    /// `Render` on a headless app with no render device: nothing warms or draws there, so an
    /// open prepare is `presented` on the first tick its frame is renderable; one that sees no
    /// renderable frame within [`Self::request_grace`] gives up.
    pub(crate) fn note_headless_tick(&mut self, renderable: bool, now: Instant) {
        self.ticks += 1;
        let (ticks, grace) = (self.ticks, self.request_grace);
        let Some(p) = self.prepare.as_mut() else {
            return;
        };
        if p.phase == crate::scene_ready::PreparePhase::Presented {
            return;
        }
        if renderable {
            p.resident();
            p.presented(ticks, false, now);
        } else if now.saturating_duration_since(p.waiting_since) >= grace {
            p.give_up(format!("no renderable frame within {:.1} s", grace.as_secs_f32()));
            p.presented(ticks, false, now);
        }
    }

    /// `Render` (window path), from [`Self::note_scene`] while a prepare holds the scene: starts
    /// its next warm round on `frame` when the frame is new or grew since the last round, else
    /// marks the scene resident.
    fn drive_prepare(
        &mut self,
        queue: Option<&PipelineWarmQueue>,
        runtime: &crate::FrameRenderer,
        frame: Option<&crate::ExtractedFrame>,
    ) {
        let (ticks, grace) = (self.ticks, self.request_grace);
        let Some(p) = self.prepare.as_mut() else {
            return;
        };
        if p.phase != crate::scene_ready::PreparePhase::Waiting || self.in_flight.is_some() {
            // A round (or another warm) runs: decide when it is back.
            return;
        }
        if self.requested {
            // A pending request (the host's own, the splash's) becomes this prepare's round.
            self.prepare_request = true;
            return;
        }
        let now = Instant::now();
        let Some(frame) = frame.filter(|f| f.is_renderable()) else {
            if now.saturating_duration_since(p.waiting_since) >= grace {
                p.give_up(format!("no renderable frame within {:.1} s", grace.as_secs_f32()));
                p.presented(ticks, false, now);
            }
            return;
        };
        if !queue.is_some_and(|q| q.is_started() && !q.is_shutting_down()) {
            // Nothing can warm off the frame thread: the first frame builds what it needs.
            p.resident();
            return;
        }
        let grew =
            p.rounds == 0 || !p.cover.covers(frame) || runtime.pending_mesh_uploads(frame, (1, 1)) != (0, 0);
        if !grew {
            p.resident();
            return;
        }
        if p.rounds >= crate::scene_ready::SCENE_PREPARE_MAX_ROUNDS {
            tracing::warn!(
                target: "regia_render",
                "scene prepare \"{}\": the frame still grows after {} warm rounds; drawing it (what \
                 arrives later builds on the frame thread)",
                p.map_id,
                p.rounds
            );
            p.resident();
            return;
        }
        if p.rounds > 0 {
            tracing::info!(
                target: "regia_render",
                "scene prepare \"{}\": the frame grew during warm round {}; warming again",
                p.map_id,
                p.rounds
            );
        }
        self.requested = true;
        self.requested_at = Some(now);
        self.request_from_splash = false;
        self.prepare_request = true;
    }

    /// [`Self::note_frame_with`] for a frame presented on this thread that uploaded and compiled
    /// nothing.
    #[cfg(test)]
    pub(crate) fn note_frame(&mut self, ms: f32) -> bool {
        self.note_frame_with(
            ms,
            crate::scene_ready::SceneFrameCheck {
                pending_meshes: 0,
                pipelines: 0,
                present: crate::scene_ready::PresentNote::Presented,
            },
        )
    }

    /// `Render` (window path), after a frame that drew the scene was presented (or queued on the
    /// frame generation present thread, `check.present`): records the first scene frame's CPU
    /// time, whether the last warm covered the scene ([`GameWarmReport::complete`]) and the open
    /// prepare's present (ADR-7883). Returns whether this was the first scene frame.
    pub(crate) fn note_frame_with(&mut self, ms: f32, check: crate::scene_ready::SceneFrameCheck) -> bool {
        if let Some(last) = self.last.as_mut()
            && last.complete.is_none()
        {
            last.complete = Some(check.complete());
        }
        if let Some(p) = self.prepare.as_mut()
            && p.phase == crate::scene_ready::PreparePhase::Resident
            && !check.dropped()
        {
            p.note_check(check.complete());
            match check.present {
                crate::scene_ready::PresentNote::Presented => p.presented(self.ticks, true, Instant::now()),
                #[cfg(any(feature = "frame-gen", test))]
                crate::scene_ready::PresentNote::Queued { ticket, flushes } => {
                    p.phase = crate::scene_ready::PreparePhase::Queued { ticket, flushes };
                }
                // Filtered above: a dropped image leaves the prepare resident.
                #[cfg(any(feature = "frame-gen", test))]
                crate::scene_ready::PresentNote::Dropped => {}
            }
        }
        self.scene_shown = true;
        if self.first_frame_ms.is_none() {
            self.first_frame_ms = Some(ms);
            crate::boot::mark_named("first_game_frame");
            release_deferred();
            // Keep what the first frame compiled even if the app is closed before the warm
            // queue drains and saves (ADR-5857).
            drop(crate::pipeline_cache::save_installed_in_background(
                "first_game_frame",
            ));
            return true;
        }
        false
    }
}

/// The engine's canonical frame: one lit quad with the default material, the sun with
/// shadows, the default environment and the default post stack. Its pipelines are what an
/// untouched [`crate::RuntimeRenderer`] builds for a lit 3D view; the boot tier warms them.
#[must_use]
pub fn canonical_frame() -> crate::ExtractedFrame {
    use crate::runtime::{ExtractedCamera, ExtractedMesh, ExtractedTarget, ExtractedView};
    let corners = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let mesh = crate::CpuMesh {
        vertices: corners
            .into_iter()
            .zip(uvs)
            .map(|(position, uv)| crate::lab_frame::LabVertex {
                position,
                normal: [0.0, 1.0, 0.0],
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
    };
    let mut frame = crate::ExtractedFrame::default();
    frame.views.push(ExtractedView {
        camera: ExtractedCamera {
            eye: [0.0, 3.0, 4.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
        },
        viewport: [0.0, 0.0, 1.0, 1.0],
        order: 0,
        clear: true,
        target: ExtractedTarget::Window,
        layer_mask: u32::MAX,
        post: crate::PostSettings::default(),
    });
    frame.meshes.push(ExtractedMesh {
        is_static: false,
        entity: regia_ecs::prelude::Entity::PLACEHOLDER,
        mesh: Arc::new(mesh),
        material: Arc::new(crate::CpuMaterial::default()),
        layer: 0,
        model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
        joints: Vec::new(),
        decal_layer: 0,
        uv1: None,
        shadow: crate::ShadowCast::All,
        draw_indices: None,
        lod_key: 0,
        object_key_xor: 0,
        gi_proxy: true,
        lod_dither: 0.0,
    });
    frame.directional = Some(crate::DirectionalLightUniform::new(
        [0.3, -0.9, 0.2],
        [1.0, 1.0, 1.0],
        100_000.0,
    ));
    frame.directional_shadows = true;
    frame
}

impl crate::FrameRenderer {
    /// Game-frame warms of a profile that selects the visibility path by density (ADR-2689,
    /// `visbuffer/gpu/warm.rs`): two frames of the main view at the window's size through the path,
    /// so the path's targets (ID, material depth, Hi-Z), its parameter blocks, its lists (at least
    /// the selection's reserve) and their bind groups exist before the scene turns dense, and the
    /// frame it does creates none. Nothing on any other profile, without a window, or when no mesh
    /// of the frame can take the path (nothing is drawn through it then).
    fn warm_visibility_targets(
        &mut self,
        device: &RenderDevice,
        frame: &crate::ExtractedFrame,
        (format, readable): (wgpu::TextureFormat, bool),
        window: Option<(u32, u32)>,
    ) -> Result<(), crate::RenderError> {
        let dense = frame.quality.visibility_buffer
            && matches!(
                frame.quality.visibility_path.select,
                crate::VisibilitySelect::Dense(_)
            );
        let (Some((width, height)), true) = (window.filter(|(w, h)| *w > 0 && *h > 0), dense) else {
            return Ok(());
        };
        let views = frame.window_views();
        let Some((view_index, view)) = views.first() else {
            return Ok(());
        };
        let _span = tracing::info_span!("warm_visibility_targets").entered();
        let target = WarmTarget::sized(device, format, readable, (width, height));
        let rect = crate::surface::pixel_viewport(view.viewport, width, height);
        let mut scene = frame.frame_scene(*view_index, rect[2], rect[3]);
        // Through the path whatever the warm scene's density: it is the dense frame's stand-in.
        scene.quality.visibility_path.select = crate::VisibilitySelect::Always;
        // Twice: the first frame's targets come after its shadow cull built that cull's groups,
        // and creating them renews the path's generation, so the second frame rebuilds those two.
        for _ in 0..2 {
            self.render_into(device, &scene, target.rect_target(rect, false))?;
            warm_checkpoint()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit-v035-conc M2: a load set's progress callback ran under the queue's state lock, so
    /// a callback that reads the queue (a loading bar showing `progress()`) deadlocked the worker.
    #[test]
    fn a_load_set_progress_callback_may_read_the_warm_queue() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(1);
        let sets = regia_asset::LoadSets::default();
        sets.define(regia_asset::LoadSet::new("menu").with(
            regia_asset::LoadKind::Pipeline,
            "menu pipelines",
            0,
            0,
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = queue.clone();
        sets.on_progress(move |_| {
            let _ = tx.send(reader.progress());
        });
        queue.attach_load_sets(sets.clone());
        queue.push(WarmTier::Boot, "menu pipelines", |_| Ok(()));
        queue.start(&device);
        // Bounds a deadlock only; the callback runs as soon as the job ends.
        let seen = rx.recv_timeout(Duration::from_secs(120));
        assert!(seen.is_ok(), "the progress callback deadlocked on the queue lock");
        assert!(sets.item_resident(regia_asset::LoadKind::Pipeline, "menu pipelines"));
        queue.shutdown();
    }

    /// audit-v035-conc L1: shutdown dropped the queued closures under the state lock, so a
    /// closure whose drop reads the queue deadlocked the shutdown.
    #[test]
    fn a_queued_job_whose_drop_reads_the_queue_does_not_deadlock_shutdown() {
        struct ReadsQueueOnDrop(PipelineWarmQueue, std::sync::mpsc::Sender<()>);
        impl Drop for ReadsQueueOnDrop {
            fn drop(&mut self) {
                let _ = self.0.progress();
                let _ = self.1.send(());
            }
        }
        let queue = PipelineWarmQueue::isolated(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = ReadsQueueOnDrop(queue.clone(), tx);
        // Never started: the job stays queued until the shutdown drops it.
        queue.push(WarmTier::Boot, "never runs", move |_| {
            let _keep = &guard;
            Ok(())
        });
        let closer = queue.clone();
        std::thread::spawn(move || closer.shutdown());
        // Bounds a deadlock only.
        assert!(
            rx.recv_timeout(Duration::from_secs(120)).is_ok(),
            "the shutdown deadlocked dropping the queued job"
        );
    }

    #[test]
    fn a_failed_offscreen_spawn_keeps_the_original_renderer() {
        let mut runtime = crate::FrameRenderer::new();
        runtime.pin_keys([42, 77]);
        let result = OffscreenWarm::start_in_place_with(&mut runtime, |_| {
            Err(std::io::Error::other("injected spawn failure"))
        });
        assert!(result.is_err(), "the injected spawn must fail");
        assert_eq!(runtime.cache_stats().pinned, 2, "the original renderer was lost");
    }

    #[test]
    fn a_successful_offscreen_handoff_returns_the_original_renderer() {
        let mut runtime = crate::FrameRenderer::new();
        runtime.pin_keys([42, 77]);
        let warm = OffscreenWarm::start_in_place_with(&mut runtime, |receiver| {
            let slot = Arc::new(Mutex::new(None));
            let filled = Arc::clone(&slot);
            let thread =
                regia_platform::GpuWorkerBuilder::new("regia-offscreen-handoff-test").spawn(move |_| {
                    let renderer = receiver.recv().expect("renderer handoff");
                    *filled.lock().expect("slot") = Some(WarmedRenderer {
                        renderer: Some(renderer),
                        error: None,
                        ms: 0.0,
                        pipelines: 0,
                    });
                })?;
            Ok(OffscreenWarm { slot, thread })
        })
        .expect("spawn");
        assert_eq!(runtime.cache_stats().pinned, 0);
        warm.thread.join().expect("worker");
        let warmed = warm.slot.lock().expect("slot").take().expect("result");
        OffscreenWarm::put_back(&mut runtime, warmed.renderer.expect("original renderer"));
        assert_eq!(runtime.cache_stats().pinned, 2);
    }

    /// audit-v035-conc H1: the offscreen warm thread was a plain thread, invisible to the
    /// end-of-process GPU worker wait. While it runs, the process's registry lists it.
    ///
    /// `spawn_in` registers the worker before the thread starts, so there is nothing to wait for
    /// here. What can go wrong is the other end: the worker leaves the registry when it ends, and a
    /// warm of an empty frame can end before a loaded test thread gets to read. The slot is locked
    /// before the spawn, and filling it is the thread's last step, so it cannot end before the read.
    #[test]
    fn the_offscreen_warm_thread_is_a_registered_gpu_worker() {
        let device = crate::device::test_device();
        let runtime = crate::FrameRenderer::new();
        let frame = crate::ExtractedFrame::default();
        let mut copy = crate::FrameRenderer::new();
        runtime.share_installs_with(&mut copy);
        let slot = Arc::new(Mutex::new(None));
        let held = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let warm =
            OffscreenWarm::spawn_filling(&device, copy, &frame, (16, 16), Arc::clone(&slot)).expect("spawn");
        let names: Vec<String> = regia_platform::gpu_workers()
            .running()
            .into_iter()
            .map(|w| w.name)
            .collect();
        drop(held);
        assert!(
            names.iter().any(|n| n == "regia-offscreen-warm"),
            "the offscreen warm thread is not registered: {names:?}"
        );
        // It still ends and hands over its renderer.
        warm.thread_join_for_test().expect("the warm thread panicked");
        let warmed = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert!(
            warmed.is_some_and(|w| w.renderer.is_some()),
            "the slot holds the warmed renderer"
        );
    }

    impl OffscreenWarm {
        /// Waits for the thread (tests only).
        fn thread_join_for_test(self) -> std::thread::Result<()> {
            self.thread.join()
        }
    }

    #[test]
    fn shutdown_wait_reads_the_override_in_milliseconds() {
        assert_eq!(parse_shutdown_wait(None), SHUTDOWN_WAIT);
        assert_eq!(parse_shutdown_wait(Some("2500")), Duration::from_millis(2500));
        assert_eq!(parse_shutdown_wait(Some(" 0 ")), Duration::ZERO);
        assert_eq!(parse_shutdown_wait(Some("soon")), SHUTDOWN_WAIT);
    }

    /// Spec 13 § 2c: a game-frame warm waits 2 s for its material graphs unless
    /// `REGIA_WARM_GRAPH_BUDGET_MS` names another whole number of milliseconds (`0`: never wait).
    #[test]
    fn warm_graph_budget_is_two_seconds_unless_overridden() {
        assert_eq!(warm_graph_budget_from(None), Duration::from_secs(2));
        assert_eq!(warm_graph_budget_from(Some("750")), Duration::from_millis(750));
        assert_eq!(warm_graph_budget_from(Some("0")), Duration::ZERO);
        assert_eq!(warm_graph_budget_from(Some("later")), Duration::from_secs(2));
        assert_eq!(warm_graph_budget_from(Some("-5")), Duration::from_secs(2));
    }

    impl PipelineWarmQueue {
        /// A queue that runs only its own jobs: its workers take no deferred builds (the
        /// renderers of parallel tests queue those process-wide, and a lavapipe compile under
        /// load can outlast `shutdown`'s wait), and other renderers do not count on it to build
        /// theirs. For tests that require every worker to be idle at `shutdown`.
        fn isolated(threads: usize) -> Self {
            let queue = Self::new(threads);
            queue.shared.isolated.store(true, Ordering::Relaxed);
            queue
        }
    }

    /// Runs the window path's warm steps (poll, watchdog, start) for one tick.
    fn warm_tick(
        gw: &mut GameFrameWarm,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        device: &RenderDevice,
    ) {
        gw.poll(runtime);
        gw.watchdog(queue, runtime);
        gw.start(
            queue,
            runtime,
            frame,
            (wgpu::TextureFormat::Rgba8Unorm, true),
            device.device_id(),
            Some(WINDOW),
        );
    }

    /// The window size `warm_tick` warms at and `draw` draws at.
    const WINDOW: (u32, u32) = (64, 48);

    /// Draws `frame`'s first window view with `runtime`; returns its mesh cache size.
    fn draw(
        runtime: &mut crate::FrameRenderer,
        device: &RenderDevice,
        frame: &crate::ExtractedFrame,
    ) -> usize {
        let (view_index, _) = frame.window_views()[0];
        let scene = frame.frame_scene(view_index, WINDOW.0, WINDOW.1);
        runtime.render_offscreen(device, &scene).expect("scene frame");
        runtime.cache_sizes().0
    }

    /// A splash finishing before the scene is spawned, the scene arriving while it still
    /// covers the window (the normal boot order): the request neither starts on an empty frame
    /// nor blocks drawing, starts once the scene arrives, and the warmed runtime renderer draws
    /// the scene within a bounded number of ticks.
    #[test]
    fn a_splash_request_warms_the_scene_that_arrives_while_the_splash_covers() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        // The watchdog is not under test here (`a_stuck_boot_job_trips_the_watchdog_*` is): its
        // 15 s default is for a player's GPU, and lavapipe beside a loaded gate's other test
        // binaries compiled this warm in more than 15 s (P3b gate, 2026-10-10). Bound it by the
        // test's own deadline instead.
        let mut gw = GameFrameWarm {
            timeout: Duration::from_secs(300),
            ..GameFrameWarm::default()
        };
        gw.note_splash(true, false);
        assert!(gw.is_pending(), "the finish requested a warm");
        assert_eq!(
            gw.explicit_requests(),
            0,
            "the splash's request is not a level load"
        );

        // No scene yet: an empty frame, then a camera without meshes. Nothing to warm.
        let empty = crate::ExtractedFrame::default();
        let mut camera_only = canonical_frame();
        camera_only.meshes.clear();
        for frame in [&empty, &camera_only] {
            for _ in 0..3 {
                warm_tick(&mut gw, &queue, &mut runtime, frame, &device);
                assert!(!gw.is_warming(), "no warm starts without a renderable frame");
            }
        }
        assert!(gw.is_pending(), "the request waits for the scene");

        // The scene arrives: the warm starts, and the scene draws once the renderer is back.
        let scene = canonical_frame();
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut ticks = 0u32;
        let mut drawn = None;
        while Instant::now() < deadline {
            warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
            ticks += 1;
            if !gw.is_warming() && !gw.is_pending() {
                // The warm ended with a frame at the window's size (ADR-173): the first
                // visible frame allocates nothing.
                assert_eq!(
                    runtime.last_extent().map(|e| e.output),
                    Some(WINDOW),
                    "the warm's last frame was window-sized"
                );
                drawn = Some(draw(&mut runtime, &device, &scene));
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let meshes = drawn.expect("the scene was drawn before the deadline");
        assert!(meshes > 0, "the runtime renderer drew the scene's mesh");
        let report = gw.last_report().expect("the warm finished");
        assert_eq!(report.error, None);
        assert!(!report.timed_out);
        assert!(report.pipelines > 0, "the warm built the scene's pipelines");
        println!("scene drawn after {ticks} ticks ({:.0} ms warm)", report.ms);
        queue.shutdown();
    }

    /// A map requested while a slow boot warm runs (the lobby is up at 1.5 s, ADR-173) keeps its
    /// request when that warm returns, even if it waited longer than the grace: the grace runs
    /// from the renderer's return (review of lane v010-splash, item 1).
    #[test]
    fn a_request_queued_behind_a_running_warm_survives_its_return() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm {
            timeout: Duration::from_secs(300),
            ..GameFrameWarm::default()
        };
        let scene = canonical_frame();
        gw.request();
        warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
        assert!(gw.is_warming());
        // "Play" while the boot warm runs; it has been waiting longer than the grace.
        gw.request();
        gw.requested_at = Instant::now().checked_sub(gw.request_grace + Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(300);
        while gw.is_warming() {
            assert!(Instant::now() < deadline, "the first warm never returned");
            gw.poll(&mut runtime);
            std::thread::sleep(Duration::from_millis(5));
        }
        gw.watchdog(&queue, &mut runtime);
        assert!(
            gw.is_pending(),
            "the queued map request survived the warm's return"
        );
        warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
        assert!(gw.is_warming(), "the map's warm starts next");
        queue.shutdown();
    }

    /// A boot-tier job that never finishes holds the only worker: the game warm queued behind
    /// it is cancelled by the watchdog, the runtime renderer (not a stand-in) comes back and
    /// draws the scene, and the queue state is reported.
    #[test]
    fn a_stuck_boot_job_trips_the_watchdog_and_the_scene_draws() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        queue.push(WarmTier::Boot, "stuck boot job", move |_| {
            let _ = stuck.recv();
            Ok(())
        });
        queue.start(&device);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !queue.describe().contains("running [boot] stuck boot job") {
            assert!(Instant::now() < deadline, "the stuck job never started");
            std::thread::sleep(Duration::from_millis(2));
        }

        // The runtime renderer has drawn the scene before (its caches mark it).
        let scene = canonical_frame();
        let mut runtime = crate::RuntimeRenderer::default().0;
        let marked = draw(&mut runtime, &device, &scene);
        assert!(marked > 0);

        let mut gw = GameFrameWarm::default();
        gw.request();
        warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
        assert!(gw.is_warming(), "the warm is queued behind the stuck job");
        assert_eq!(
            runtime.cache_sizes().0,
            0,
            "a stand-in holds the renderer's place"
        );
        let described = queue.describe();
        assert!(described.contains("pending [map] game frame"), "{described}");

        // Before the timeout nothing happens; after it the job is cancelled.
        gw.watchdog_at(&queue, &mut runtime, Instant::now() + gw.timeout / 2);
        assert!(gw.is_warming());
        gw.watchdog_at(
            &queue,
            &mut runtime,
            Instant::now() + gw.timeout + Duration::from_secs(1),
        );
        assert!(!gw.is_warming() && !gw.is_pending(), "the watchdog gave up");
        assert_eq!(runtime.cache_sizes().0, marked, "the runtime renderer came back");
        let report = gw.last_report().expect("a report");
        assert!(report.timed_out);
        assert!(
            report.error.as_deref().is_some_and(|e| e.contains("canceled")),
            "{report:?}"
        );
        assert!(draw(&mut runtime, &device, &scene) > 0, "the scene draws");
        let progress = queue.progress();
        assert_eq!(
            (progress.done, progress.total),
            (1, 2),
            "the canceled job counts as done"
        );
        assert_eq!(progress.tiers[WarmTier::Map as usize].jobs_failed, 1);

        release.send(()).expect("release the stuck job");
        assert!(
            queue.wait_tier(WarmTier::Map, Duration::from_secs(30)),
            "the queue drains"
        );
        queue.shutdown();
    }

    /// A game warm that is running but never finishes is abandoned: the stand-in keeps the
    /// renderer's place and the job's late result is dropped.
    #[test]
    fn a_hung_running_warm_is_abandoned_and_its_late_result_dropped() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let (release, hung) = std::sync::mpsc::channel::<()>();
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let handle = queue.warm_renderer(
            WarmTier::Map,
            "game frame",
            crate::FrameRenderer::new(),
            move |_, _| {
                let _ = started_tx.send(());
                let _ = hung.recv();
                Ok(())
            },
        );
        started
            .recv_timeout(Duration::from_secs(30))
            .expect("the warm started");
        let mut gw = GameFrameWarm {
            in_flight: Some(InFlight {
                handle: handle.clone(),
                since: Instant::now(),
                pipelines_at_start: 0,
                progress: (0, Instant::now()),
                uploads: (0, 0),
                for_prepare: false,
            }),
            ..GameFrameWarm::default()
        };
        let mut runtime = crate::RuntimeRenderer::default().0;
        // The hung job builds nothing (a fixed map-tier count: other tests' warms run in
        // parallel in this process).
        gw.watchdog_with(
            &queue,
            &mut runtime,
            Instant::now() + gw.timeout + Duration::from_secs(1),
            0,
        );
        assert!(!gw.is_warming(), "abandoned");
        let report = gw.last_report().expect("a report");
        assert!(report.timed_out);
        assert!(
            report.error.as_deref().is_some_and(|e| e.contains("abandoned")),
            "{report:?}"
        );

        release.send(()).expect("release the hung warm");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        // The late result is only searched for a shared draw (this renderer had none); the report
        // stays timed out.
        gw.poll(&mut runtime);
        assert!(gw.last_report().is_some_and(|r| r.timed_out));
        queue.shutdown();
    }

    /// (c) of the DO27 first-frame-stall brief (ADR-7883): the watchdog abandons a running round
    /// of the host's scene prepare; the prepare still ends in `presented` (late) with `timed_out`
    /// once the stand-in's first scene frame is presented, and sends its `SceneReady` once.
    #[test]
    fn an_abandoned_prepare_round_still_ends_presented_with_timed_out() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let (handle, release) = running_warm(&queue);
        let mut readiness = crate::SceneReadiness::default();
        readiness.prepare("abandoned");
        let (seq, map_id) = readiness.take_request().expect("queued");
        let mut gw = GameFrameWarm::default();
        gw.open_prepare(seq, map_id, Instant::now());
        if let Some(p) = gw.prepare.as_mut() {
            p.rounds = 1;
            p.phase = crate::scene_ready::PreparePhase::Warming;
        }
        gw.in_flight = Some(InFlight {
            for_prepare: true,
            ..in_flight(handle, Instant::now())
        });
        assert!(gw.holds_scene());
        let mut runtime = crate::RuntimeRenderer::default().0;
        gw.watchdog_with(
            &queue,
            &mut runtime,
            Instant::now() + gw.timeout + Duration::from_secs(1),
            0,
        );
        assert!(
            !gw.is_warming() && !gw.holds_scene(),
            "abandoned: the stand-in draws"
        );
        assert!(
            crate::scene_ready::publish(&mut readiness, &gw).is_none(),
            "not before a present"
        );
        assert_eq!(readiness.state(), crate::SceneReadyState::Resident);
        let report = gw.last_report().expect("a report");
        assert_eq!(report.map_id.as_deref(), Some("abandoned"));
        assert_eq!(report.complete, Some(false));
        gw.note_frame(0.0);
        let ready = crate::scene_ready::publish(&mut readiness, &gw).expect("SceneReady, late");
        assert!(ready.timed_out, "{ready:?}");
        assert!(crate::scene_ready::publish(&mut readiness, &gw).is_none(), "once");
        release.send(()).expect("release the hung warm");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        queue.shutdown();
    }

    /// A warm job that runs until released; returns its handle and the release sender once
    /// a worker runs it.
    fn running_warm(queue: &PipelineWarmQueue) -> (RendererWarm, std::sync::mpsc::Sender<()>) {
        let (release, hung) = std::sync::mpsc::channel::<()>();
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let handle = queue.warm_renderer(
            WarmTier::Map,
            "game frame",
            crate::FrameRenderer::new(),
            move |_, _| {
                let _ = started_tx.send(());
                let _ = hung.recv();
                Ok(())
            },
        );
        started
            .recv_timeout(Duration::from_secs(30))
            .expect("the warm started");
        (handle, release)
    }

    fn in_flight(handle: RendererWarm, since: Instant) -> InFlight {
        InFlight {
            handle,
            since,
            pipelines_at_start: 0,
            progress: (0, since),
            uploads: (0, 0),
            for_prepare: false,
        }
    }

    /// DO27 v0.1.8 smoke on a loaded box: the game warm passed its 15 s timeout while still
    /// compiling and was abandoned at 20.6 s, so the stand-in compiled the same pipelines on the
    /// frame thread. A running warm that keeps building pipelines past its timeout is now let
    /// run; it is abandoned once it builds nothing for `progress_grace`.
    #[test]
    fn a_warm_still_building_past_its_timeout_is_abandoned_only_once_it_stalls() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let (handle, release) = running_warm(&queue);
        let t0 = Instant::now();
        let mut gw = GameFrameWarm {
            in_flight: Some(in_flight(handle, t0)),
            ..GameFrameWarm::default()
        };
        let mut runtime = crate::RuntimeRenderer::default().0;
        let past = t0 + gw.timeout + Duration::from_secs(1);
        // Seven pipelines since the start, seen now: progressing.
        gw.watchdog_with(&queue, &mut runtime, past, 7);
        assert!(
            gw.is_warming(),
            "a progressing warm past its timeout keeps running"
        );
        assert!(gw.last_report().is_none());
        // Nothing new, but within the grace.
        let within = past + gw.progress_grace - Duration::from_millis(500);
        gw.watchdog_with(&queue, &mut runtime, within, 7);
        assert!(gw.is_warming(), "still within the progress grace");
        // Nothing new for longer than the grace: stalled, abandoned.
        let stalled = past + gw.progress_grace + Duration::from_secs(1);
        gw.watchdog_with(&queue, &mut runtime, stalled, 7);
        assert!(!gw.is_warming(), "a stalled warm is abandoned");
        let report = gw.last_report().expect("a report");
        assert!(report.timed_out);
        assert!(
            report.error.as_deref().is_some_and(|e| e.contains("abandoned")),
            "{report:?}"
        );
        release.send(()).expect("release the warm");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        gw.poll(&mut runtime);
        queue.shutdown();
    }

    /// A warm that keeps building forever is still abandoned at `max_timeout`; with
    /// `progress_grace` zero the plain timeout applies.
    #[test]
    fn a_progressing_warm_is_abandoned_at_max_timeout_and_a_zero_grace_keeps_the_plain_timeout() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;

        let (handle, release) = running_warm(&queue);
        let t0 = Instant::now();
        let mut gw = GameFrameWarm {
            in_flight: Some(in_flight(handle, t0)),
            ..GameFrameWarm::default()
        };
        let mut count = 0;
        let mut at = t0 + gw.timeout + Duration::from_secs(1);
        while at < t0 + gw.max_timeout {
            count += 1;
            gw.watchdog_with(&queue, &mut runtime, at, count);
            assert!(gw.is_warming(), "progressing at {:?}", at - t0);
            at += Duration::from_secs(1);
        }
        gw.watchdog_with(
            &queue,
            &mut runtime,
            t0 + gw.max_timeout + Duration::from_secs(1),
            count + 1,
        );
        assert!(!gw.is_warming(), "abandoned at the cap");
        assert!(gw.last_report().is_some_and(|r| r.timed_out));
        release.send(()).expect("release the warm");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        gw.poll(&mut runtime);

        let (handle, release) = running_warm(&queue);
        let t0 = Instant::now();
        let mut gw = GameFrameWarm {
            in_flight: Some(in_flight(handle, t0)),
            progress_grace: Duration::ZERO,
            ..GameFrameWarm::default()
        };
        gw.watchdog_with(&queue, &mut runtime, t0 + gw.timeout + Duration::from_secs(1), 5);
        assert!(!gw.is_warming(), "zero grace: abandoned at the timeout");
        release.send(()).expect("release the warm");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        gw.poll(&mut runtime);
        queue.shutdown();
    }

    /// End to end on the device: a real game warm that runs past its timeout (1 ms here, a slow
    /// machine) while it builds pipelines is not abandoned. It comes back warmed and the first
    /// game frame compiles nothing on the frame thread: no burst of inline creates.
    #[test]
    fn a_slow_warm_past_its_timeout_finishes_and_the_game_frame_compiles_nothing() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm {
            timeout: Duration::from_millis(1),
            // Lavapipe under a loaded gate: minutes between pipelines would be a hang.
            progress_grace: Duration::from_secs(240),
            max_timeout: Duration::from_secs(600),
            ..GameFrameWarm::default()
        };
        let scene = canonical_frame();
        gw.request();
        warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
        assert!(gw.is_warming());
        // A queued warm past its timeout is cancelled (that rule is unchanged): tick once the
        // worker runs it.
        let deadline = Instant::now() + Duration::from_secs(120);
        while !queue.describe().contains("running [map] game frame") && gw.is_warming() {
            assert!(Instant::now() < deadline, "the warm never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        let deadline = Instant::now() + Duration::from_secs(600);
        let mut ticks = 0u32;
        while gw.is_warming() {
            assert!(Instant::now() < deadline, "the warm never returned");
            warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
            ticks += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        let report = gw.last_report().cloned().expect("the warm finished");
        assert!(!report.timed_out, "not abandoned: {report:?}");
        assert!(report.pipelines > 0, "{report:?}");
        let before = crate::frame_stats::pipeline_creates();
        assert!(draw(&mut runtime, &device, &scene) > 0, "the scene draws");
        let creates = crate::frame_stats::pipeline_creates() - before;
        println!(
            "slow warm: {} pipelines in {:.0} ms over {ticks} watchdog ticks past its timeout; \
             first game frame created {creates}",
            report.pipelines, report.ms
        );
        assert_eq!(
            creates, 0,
            "the first game frame compiled nothing on the frame thread"
        );
        queue.shutdown();
    }

    /// A shared pass draw for the abandoned-warm tests (draws nothing).
    struct NoDraw;
    impl crate::SharedPassDraw for NoDraw {
        fn prepare(&mut self, _info: &crate::SharedFrameInfo, _upload: &mut crate::SharedUpload<'_>) {}
        fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: crate::SharedPass) {}
    }

    /// A runtime renderer with a GI field, one reflection probe and a shared draw installed.
    fn installed_renderer() -> crate::FrameRenderer {
        let mut runtime = crate::RuntimeRenderer::default().0;
        runtime.set_gi_field(Some(
            crate::gi::GiField::new([0.0; 3], [1.0; 3], [2, 2, 2]).expect("gi field"),
        ));
        runtime.set_reflection_probes(vec![crate::reflections::ReflectionProbe {
            id: 1,
            center: [0.0; 3],
            half_extent: [1.0; 3],
            equirect: vec![0; 4 * 4 * 4],
            width: 4,
            height: 4,
        }]);
        runtime.set_shared_draw(Some(Box::new(NoDraw)));
        assert_eq!(runtime.installs(), (true, 1, true));
        runtime
    }

    /// Abandons a running game warm of an [`installed_renderer`]; `meanwhile` runs on the
    /// stand-in after the abandon, then the hung job returns and is polled. Returns the
    /// stand-in's installs right after the abandon and after the late return.
    fn abandon_installed_warm(
        meanwhile: impl FnOnce(&mut crate::FrameRenderer),
    ) -> ((bool, usize, bool), (bool, usize, bool)) {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = installed_renderer();
        // What `GameFrameWarm::start` does, with a warm job that hangs until released.
        let mut stand_in = crate::RuntimeRenderer::default().0;
        runtime.share_installs_with(&mut stand_in);
        let renderer = std::mem::replace(&mut runtime, stand_in);
        let (release, hung) = std::sync::mpsc::channel::<()>();
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let handle = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |_, _| {
            let _ = started_tx.send(());
            let _ = hung.recv();
            Ok(())
        });
        let mut gw = GameFrameWarm {
            in_flight: Some(InFlight {
                handle,
                since: Instant::now(),
                pipelines_at_start: 0,
                progress: (0, Instant::now()),
                uploads: (0, 0),
                for_prepare: false,
            }),
            ..GameFrameWarm::default()
        };
        started
            .recv_timeout(Duration::from_secs(30))
            .expect("the warm started");

        gw.watchdog_with(
            &queue,
            &mut runtime,
            Instant::now() + gw.timeout + Duration::from_secs(1),
            0,
        );
        assert!(!gw.is_warming(), "abandoned");
        assert!(gw.last_report().is_some_and(|r| r.timed_out));
        let after_abandon = runtime.installs();
        meanwhile(&mut runtime);

        release.send(()).expect("release the hung warm");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        gw.poll(&mut runtime);
        assert!(gw.abandoned.is_none(), "the late result was taken");
        assert!(
            gw.last_report().is_some_and(|r| r.timed_out),
            "the report stays timed out"
        );
        queue.shutdown();
        (after_abandon, runtime.installs())
    }

    /// Audit #17: after the watchdog abandons a running warm the map still draws as before. The
    /// stand-in that takes over has the GI field and reflection probes at once, and the terrain
    /// shared draw comes back when the abandoned job returns.
    #[test]
    fn an_abandoned_warm_keeps_the_gi_field_probes_and_returns_the_shared_draw() {
        let (after_abandon, after_return) = abandon_installed_warm(|_| {});
        assert_eq!(
            after_abandon,
            (true, 1, false),
            "GI field and probes on the stand-in"
        );
        assert_eq!(after_return, (true, 1, true), "the shared draw came back");
    }

    /// A host that sets (or clears) the shared draw on the stand-in after the abandon keeps
    /// its choice: the late warm's draw is not put back over it.
    #[test]
    fn an_abandoned_warm_does_not_override_a_shared_draw_set_meanwhile() {
        let (_, after_return) = abandon_installed_warm(|stand_in| stand_in.set_shared_draw(None));
        assert_eq!(after_return, (true, 1, false), "the host's `None` stands");
    }

    /// A new warm forgets an older abandoned one: that warm's terrain belongs to the old map
    /// and must never be salvaged onto the new map's stand-in.
    #[test]
    fn a_new_warm_drops_an_older_abandoned_warm() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        queue.push(WarmTier::Boot, "hold", move |_| {
            let _ = stuck.recv();
            Ok(())
        });
        queue.start(&device);
        let old = queue.warm_renderer(WarmTier::Map, "old map", installed_renderer(), |_, _| Ok(()));
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm {
            abandoned: Some(old),
            ..GameFrameWarm::default()
        };
        // Not started yet (no request): the abandoned warm is kept for its late return.
        let frame = canonical_frame();
        let target = (wgpu::TextureFormat::Rgba8Unorm, true);
        gw.start(&queue, &mut runtime, &frame, target, device.device_id(), None);
        assert!(gw.abandoned.is_some());
        gw.request();
        gw.start(&queue, &mut runtime, &frame, target, device.device_id(), None);
        assert!(gw.is_warming());
        assert!(gw.abandoned.is_none(), "the old map's warm is forgotten");
        release.send(()).expect("release the worker");
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(120)));
        gw.poll(&mut runtime);
        assert!(!runtime.installs().2, "no old terrain on the new renderer");
        queue.shutdown();
    }

    /// `GameFrameWarm::start` gives the stand-in the renderer's GI field and probes (the shared
    /// draw goes with the warm), and a warm that finishes normally hands everything back.
    #[test]
    fn a_game_warm_stand_in_starts_with_the_installs_and_the_warm_returns_them() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        // Hold the only worker so the game warm stays queued while the stand-in is inspected.
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        queue.push(WarmTier::Boot, "hold", move |_| {
            let _ = stuck.recv();
            Ok(())
        });
        queue.start(&device);
        let mut runtime = installed_renderer();
        let mut gw = GameFrameWarm::default();
        gw.request();
        gw.start(
            &queue,
            &mut runtime,
            &canonical_frame(),
            (wgpu::TextureFormat::Rgba8Unorm, true),
            device.device_id(),
            None,
        );
        assert!(gw.is_warming(), "the warm started");
        assert_eq!(runtime.installs(), (true, 1, false), "the stand-in's installs");
        let dropped = stand_ins_dropped_off_thread();
        release.send(()).expect("release the worker");
        let deadline = Instant::now() + Duration::from_secs(120);
        while gw.is_warming() {
            assert!(Instant::now() < deadline, "the warm never finished");
            gw.poll(&mut runtime);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(runtime.installs(), (true, 1, true), "the warmed renderer is back");
        // v0.5.1: the stand-in is freed on a worker, not on the frame that swaps.
        assert!(
            stand_ins_dropped_off_thread() > dropped,
            "the swap dropped the stand-in on the frame thread"
        );
        queue.shutdown();
    }

    /// ADR-5131: a warm draws its frame with the per-frame renderer state the window render
    /// applies to every frame it draws (`render_windows`). Before, the GI tier, the radiance
    /// cache and the two-phase prepass cull stayed at the renderer's defaults, so an Ultra demo
    /// warmed its first frame with High's probe volume and the first real frame rebuilt the
    /// probes (8 compute pipelines outside the warm queue on cold lavapipe). The shadow cull is
    /// the renderer's own switch on that path, so the warm leaves it alone.
    #[test]
    fn a_warm_applies_the_frames_gi_tier_and_culling_switches() {
        let device = crate::device::test_device();
        let mut renderer = crate::RuntimeRenderer::default().0;
        assert_eq!(
            renderer.dynamic_gi_tier(),
            crate::DynamicGiTier::High,
            "the default tier"
        );
        let shadow_cull = renderer.shadow_occlusion();
        let mut frame = canonical_frame();
        frame.dynamic_gi_tier = crate::DynamicGiTier::Ultra;
        frame.radiance_cache = true;
        frame.two_phase_occlusion = !renderer.two_phase_occlusion();
        frame.shadow_occlusion = !shadow_cull;
        if let Err(err) = renderer.warm_extracted(&device, &frame, wgpu::TextureFormat::Rgba8Unorm, true) {
            panic!("warm: {err}");
        }
        assert_eq!(
            renderer.dynamic_gi_tier(),
            crate::DynamicGiTier::Ultra,
            "the frame's tier"
        );
        assert!(
            renderer.radiance_cache_status().enabled,
            "the frame's radiance cache"
        );
        assert_eq!(renderer.two_phase_occlusion(), frame.two_phase_occlusion);
        assert_eq!(
            renderer.shadow_occlusion(),
            shadow_cull,
            "the renderer's own shadow cull"
        );
    }

    /// Installs the host makes on the stand-in while the renderer is on a worker (a new-scene
    /// warm can start while the host still configures the map) reach the renderer that comes
    /// back: GI field, reflection probes, the DDGI switch. Untouched ones keep the renderer's.
    #[test]
    fn installs_made_on_the_stand_in_during_a_warm_reach_the_returning_renderer() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        queue.push(WarmTier::Boot, "hold", move |_| {
            let _ = stuck.recv();
            Ok(())
        });
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        runtime.set_ddgi_enabled(Some(true));
        let mut gw = GameFrameWarm::default();
        gw.request();
        gw.start(
            &queue,
            &mut runtime,
            &canonical_frame(),
            (wgpu::TextureFormat::Rgba8Unorm, true),
            device.device_id(),
            None,
        );
        assert!(gw.is_warming(), "the warm started");
        assert_eq!(runtime.installs(), (false, 0, false), "a bare stand-in");
        // The host configures the map on what it sees: the stand-in.
        runtime.set_gi_field(Some(
            crate::gi::GiField::new([0.0; 3], [1.0; 3], [2, 2, 2]).expect("gi field"),
        ));
        let probe = |id| crate::reflections::ReflectionProbe {
            id,
            center: [0.0; 3],
            half_extent: [1.0; 3],
            equirect: vec![0; 4 * 4 * 4],
            width: 4,
            height: 4,
        };
        runtime.set_reflection_probes(vec![probe(1), probe(2)]);
        runtime.reserve_instances(4096);
        release.send(()).expect("release the worker");
        let deadline = Instant::now() + Duration::from_secs(120);
        while gw.is_warming() {
            assert!(Instant::now() < deadline, "the warm never finished");
            gw.poll(&mut runtime);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            runtime.installs(),
            (true, 2, false),
            "the GI field and probes set meanwhile came over"
        );
        assert_eq!(
            runtime.ddgi_enabled(),
            Some(true),
            "the renderer's own switch stayed (the stand-in never set one)"
        );
        queue.shutdown();
    }

    /// What `render_windows` does each tick with a started warm queue and no splash: the
    /// warm's poll and watchdog, the new-scene check, the warm's start; the scene is drawn (and
    /// noted) only while no warm runs. Returns whether it drew.
    fn window_tick(
        gw: &mut GameFrameWarm,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        device: &RenderDevice,
    ) -> bool {
        gw.poll(runtime);
        gw.watchdog(queue, runtime);
        gw.note_scene(Some(queue), runtime, Some(frame), true);
        gw.start(
            queue,
            runtime,
            frame,
            (wgpu::TextureFormat::Rgba8Unorm, true),
            device.device_id(),
            Some(WINDOW),
        );
        if gw.is_warming() || !frame.is_renderable() {
            return false;
        }
        draw(runtime, device, frame);
        gw.note_frame(0.0);
        true
    }

    /// A map spawned behind a UI-only menu: `count` distinct meshes (own `Arc`s), each a strip
    /// of `quads` quads (96-byte vertices: about 0.4 KiB per quad with its indices).
    fn map_scene(count: usize, quads: usize) -> crate::ExtractedFrame {
        let mut frame = canonical_frame();
        let template = frame.meshes[0].clone();
        let quad = template.mesh.vertices.clone();
        frame.meshes.clear();
        for i in 0..count {
            let mut vertices = Vec::with_capacity(quads * 4);
            let mut indices = Vec::with_capacity(quads * 6);
            for q in 0..quads {
                let base = (q * 4) as u32;
                for v in &quad {
                    let mut v = *v;
                    v.position[1] = (q % 7) as f32 * 0.01 + i as f32 * 0.001;
                    vertices.push(v);
                }
                indices.extend([0, 2, 1, 0, 3, 2].map(|k| base + k));
            }
            let mut mesh = template.clone();
            mesh.mesh = Arc::new(crate::CpuMesh { vertices, indices });
            frame.meshes.push(mesh);
        }
        frame
    }

    /// Bytes of mesh data in `frame`, each distinct mesh once.
    fn mesh_bytes(frame: &crate::ExtractedFrame) -> u64 {
        crate::FrameRenderer::new()
            .pending_mesh_uploads(frame, (u64::MAX, usize::MAX))
            .0
    }

    /// Stall (a) of the v0.1.6 Windows testers (lane v018-stalls): DO27's map became resident
    /// behind the menu (no scene drawn before it), and its first frame compiled the frame's
    /// pipelines and uploaded the map on the frame thread (4.7 s, 72 pipelines, 1 GB). With
    /// the new-scene warm the frame thread keeps presenting the menu: no tick creates a
    /// pipeline or uploads more than a small per-frame amount of data, the map's bytes go up
    /// on the warm worker, and the first scene frame compiles and uploads nothing new.
    #[test]
    fn a_large_new_scene_behind_a_ui_only_menu_is_warmed_off_the_frame_thread() {
        // Per-tick upload ceiling on the frame thread: per-frame data (instances, uniforms,
        // clusters), never the map. The map here is ten times larger.
        const MAX_TICK_UPLOAD: u64 = 4 << 20;
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm {
            timeout: Duration::from_secs(300),
            on_new_scene: true,
            ..GameFrameWarm::default()
        };
        // The menu: a camera, no meshes (the UI overlay is drawn by the window, not here).
        let mut menu = canonical_frame();
        menu.meshes.clear();
        for _ in 0..3 {
            assert!(!window_tick(&mut gw, &queue, &mut runtime, &menu, &device));
        }
        let map = map_scene(48, 2048);
        let total = mesh_bytes(&map);
        assert!(total >= NEW_SCENE_MIN_BYTES, "the map is large: {total} bytes");
        let deadline = Instant::now() + Duration::from_secs(300);
        let (mut ticks, mut max_upload, mut drawn) = (0u32, 0u64, false);
        while Instant::now() < deadline {
            let before = crate::frame_stats::counters();
            let drew = window_tick(&mut gw, &queue, &mut runtime, &map, &device);
            let tick = crate::frame_stats::counters().since(before);
            ticks += 1;
            max_upload = max_upload.max(tick.upload_bytes);
            assert_eq!(
                tick.pipelines, 0,
                "tick {ticks} (drew {drew}) created a pipeline on the frame thread"
            );
            assert!(
                tick.upload_bytes <= MAX_TICK_UPLOAD,
                "tick {ticks} (drew {drew}) uploaded {} bytes on the frame thread (map {total})",
                tick.upload_bytes
            );
            if drew {
                drawn = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(drawn, "the map was drawn before the deadline");
        assert_eq!(gw.scene_warms(), 1, "one new-scene warm");
        assert_eq!(gw.explicit_requests(), 0, "not a level-load request");
        let report = gw.last_report().expect("the warm finished");
        assert_eq!(report.error, None);
        assert!(report.pipelines > 0, "the warm built the map's pipelines");
        assert_eq!(runtime.pending_mesh_uploads(&map, (u64::MAX, usize::MAX)), (0, 0));
        println!(
            "map {:.1} MiB drawn after {ticks} ticks; max frame-thread upload per tick {:.2} MiB",
            total as f64 / f64::from(1u32 << 20),
            max_upload as f64 / f64::from(1u32 << 20)
        );

        // The same map without the warm (pre-v0.1.8, or `on_new_scene: false`): its first
        // frame uploads all of it on the frame thread. This keeps the measurement honest.
        let mut cold = crate::RuntimeRenderer::default().0;
        let mut off = GameFrameWarm {
            on_new_scene: false,
            ..GameFrameWarm::default()
        };
        assert!(!window_tick(&mut off, &queue, &mut cold, &menu, &device));
        let before = crate::frame_stats::counters();
        assert!(
            window_tick(&mut off, &queue, &mut cold, &map, &device),
            "drawn at once"
        );
        let first = crate::frame_stats::counters().since(before);
        assert!(
            first.upload_bytes >= total,
            "an unwarmed first frame uploads the map: {} of {total} bytes",
            first.upload_bytes
        );
        assert_eq!(off.scene_warms(), 0);
        queue.shutdown();
    }

    /// The new-scene warm never blanks a scene on screen and never loops: a window that shows
    /// a scene draws a new map at once; at most one warm per scene-less stretch, even when more
    /// of the map arrives while it runs; a later scene-less stretch may warm again; a covering
    /// splash, a disabled switch or a small scene request nothing.
    /// v0.1.16 (ADR-223): the new-scene warm is off on an integrated GPU unless
    /// `REGIA_SCENE_WARM=1`; other devices keep it; `on_new_scene: false` turns it off
    /// everywhere.
    #[test]
    fn the_new_scene_warm_is_off_on_integrated_gpus_unless_forced() {
        let warm = |on_new_scene, scene_warm_forced| GameFrameWarm {
            on_new_scene,
            scene_warm_forced,
            ..GameFrameWarm::default()
        };
        assert!(warm(true, false).new_scene_warm_on(false), "discrete: on");
        assert!(!warm(true, false).new_scene_warm_on(true), "integrated: off");
        assert!(
            warm(true, true).new_scene_warm_on(true),
            "integrated with REGIA_SCENE_WARM=1: on"
        );
        assert!(!warm(false, true).new_scene_warm_on(false), "off everywhere");
        assert!(!warm(false, true).new_scene_warm_on(true));
    }

    #[test]
    fn the_new_scene_warm_runs_once_per_scene_less_stretch_and_never_blanks_a_scene() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm {
            timeout: Duration::from_secs(300),
            on_new_scene: true,
            // Light meshes, a low mesh threshold: the decision, not the bytes, is under test.
            new_scene_min_bytes: u64::MAX,
            new_scene_min_meshes: 3,
            ..GameFrameWarm::default()
        };
        let mut menu = canonical_frame();
        menu.meshes.clear();
        let small = map_scene(2, 1);
        let map = map_scene(4, 1);

        // A covering splash: its own rules apply, no new-scene request.
        gw.note_scene(Some(&queue), &runtime, Some(&map), false);
        assert!(!gw.is_pending());
        // DO27's order: the splash finishes with no scene (its request waits), then fades and
        // the request is dropped. That pending request must not use up the scene-less stretch.
        gw.note_splash(true, false);
        assert!(gw.is_pending());
        assert!(!window_tick(&mut gw, &queue, &mut runtime, &menu, &device));
        gw.note_splash(false, true);
        assert!(!gw.is_pending(), "the splash request expired with the splash");
        gw.note_scene(Some(&queue), &runtime, Some(&map), true);
        assert!(gw.is_pending(), "the stretch still warms its map");
        assert_eq!(gw.scene_warms(), 1);
        // Run that warm to the end; the map draws.
        let deadline = Instant::now() + Duration::from_secs(300);
        while !window_tick(&mut gw, &queue, &mut runtime, &map, &device) {
            assert!(Instant::now() < deadline, "the map was drawn before the deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm {
            timeout: Duration::from_secs(300),
            on_new_scene: true,
            new_scene_min_bytes: u64::MAX,
            new_scene_min_meshes: 3,
            ..GameFrameWarm::default()
        };
        // A small scene (under both thresholds) draws at once.
        assert!(!window_tick(&mut gw, &queue, &mut runtime, &menu, &device));
        assert!(window_tick(&mut gw, &queue, &mut runtime, &small, &device));
        assert_eq!(gw.scene_warms(), 0);
        // A window showing a scene: a new map draws at once (no warm blanks a visible scene).
        assert!(window_tick(&mut gw, &queue, &mut runtime, &map, &device));
        assert_eq!(gw.scene_warms(), 0);

        // Back to the menu, then a new map: warmed once, and once only, even though more of
        // it is still arriving (a new mesh set every tick) while the warm runs.
        assert!(!window_tick(&mut gw, &queue, &mut runtime, &menu, &device));
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut drawn = false;
        while Instant::now() < deadline {
            let streaming = map_scene(4, 1);
            if window_tick(&mut gw, &queue, &mut runtime, &streaming, &device) {
                drawn = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(drawn, "the map was drawn before the deadline");
        assert_eq!(gw.scene_warms(), 1, "one warm for the stretch");

        // Menu again, another map: a new stretch may warm again.
        assert!(!window_tick(&mut gw, &queue, &mut runtime, &menu, &device));
        let next = map_scene(4, 1);
        gw.note_scene(Some(&queue), &runtime, Some(&next), true);
        assert!(gw.is_pending(), "a new stretch warms its new map");
        assert_eq!(gw.scene_warms(), 2);

        // Switched off: nothing is requested.
        let mut off = GameFrameWarm {
            on_new_scene: false,
            new_scene_min_meshes: 1,
            ..GameFrameWarm::default()
        };
        let fresh = crate::RuntimeRenderer::default().0;
        off.note_scene(Some(&queue), &fresh, Some(&map), true);
        assert!(!off.is_pending());
        // No started queue: nothing is requested either.
        let idle = PipelineWarmQueue::new(1);
        let mut gw2 = GameFrameWarm {
            on_new_scene: true,
            new_scene_min_meshes: 1,
            ..GameFrameWarm::default()
        };
        gw2.note_scene(Some(&idle), &fresh, Some(&map), true);
        assert!(!gw2.is_pending());
        queue.shutdown();
    }

    /// DO27's order: `finish()` at sim boot, the splash fades, the scene arrives 30+ s later.
    /// The late scene is never blanked: the splash request expired with the splash, nothing
    /// moves the runtime renderer, and the scene draws on its first tick.
    #[test]
    fn a_scene_arriving_after_the_splash_is_gone_is_drawn_at_once() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm::default();
        let empty = crate::ExtractedFrame::default();
        gw.note_splash(true, false);
        warm_tick(&mut gw, &queue, &mut runtime, &empty, &device);
        assert!(gw.is_pending());
        // The splash fades out (nothing held it: no warm was running).
        gw.note_splash(false, true);
        assert!(!gw.is_pending(), "the splash request expired with the splash");
        // 30 s later the scene arrives (the watchdog's clock jumps too).
        gw.watchdog_at(&queue, &mut runtime, Instant::now() + Duration::from_secs(31));
        let scene = canonical_frame();
        warm_tick(&mut gw, &queue, &mut runtime, &scene, &device);
        assert!(!gw.is_warming(), "a late scene is never blanked by a warm");
        assert!(
            draw(&mut runtime, &device, &scene) > 0,
            "the scene draws on its first tick"
        );
        assert_eq!(queue.progress().total, 0, "no warm job was queued");
        queue.shutdown();
    }

    /// An explicit request that sees no renderable frame expires after the grace period
    /// instead of firing (and blanking the window) whenever a scene finally shows up.
    #[test]
    fn a_request_without_a_renderable_frame_expires_after_the_grace() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm::default();
        gw.request();
        assert_eq!(gw.explicit_requests(), 1);
        gw.watchdog_at(&queue, &mut runtime, Instant::now() + gw.request_grace / 2);
        assert!(gw.is_pending(), "still within the grace");
        gw.watchdog_at(
            &queue,
            &mut runtime,
            Instant::now() + gw.request_grace + Duration::from_secs(1),
        );
        assert!(!gw.is_pending(), "expired");
        warm_tick(&mut gw, &queue, &mut runtime, &canonical_frame(), &device);
        assert!(!gw.is_warming(), "an expired request never starts");
        queue.shutdown();
    }

    /// A request is dropped, not run across devices, when the queue runs on another device.
    #[test]
    fn a_warm_is_skipped_when_the_queue_runs_on_another_device() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm::default();
        gw.request();
        gw.start(
            &queue,
            &mut runtime,
            &canonical_frame(),
            (wgpu::TextureFormat::Rgba8Unorm, true),
            device.device_id() + 1,
            None,
        );
        assert!(!gw.is_warming() && !gw.is_pending(), "skipped and dropped");
        queue.shutdown();
    }

    #[test]
    fn tiers_run_in_priority_order_and_report_progress() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        let order = Arc::new(Mutex::new(Vec::new()));
        for tier in [WarmTier::Map, WarmTier::Optional, WarmTier::Boot, WarmTier::Menu] {
            let order = order.clone();
            queue.push(tier, tier.name(), move |_| {
                order.lock().expect("order").push(tier);
                Ok(())
            });
        }
        queue.push(WarmTier::Lobby, "fails", |_| Err("no lobby".into()));
        let before = queue.progress();
        assert_eq!((before.done, before.total), (0, 5));
        assert!(!before.is_tier_ready(WarmTier::Boot));
        // Inline up to the lobby: boot, menu, lobby run; map and optional stay queued.
        assert_eq!(queue.run_inline(&device, WarmTier::Lobby), 3);
        assert!(queue.is_tier_ready(WarmTier::Lobby));
        assert!(!queue.is_tier_ready(WarmTier::Map));
        queue.start(&device);
        assert!(queue.wait_tier(WarmTier::Optional, Duration::from_secs(30)));
        let order = order.lock().expect("order").clone();
        assert_eq!(
            order,
            [WarmTier::Boot, WarmTier::Menu, WarmTier::Map, WarmTier::Optional]
        );
        let progress = queue.progress();
        assert_eq!((progress.done, progress.total), (5, 5));
        assert_eq!(progress.tiers[WarmTier::Lobby as usize].jobs_failed, 1);
        assert!(
            progress
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("no lobby"))
        );
        assert!(progress.tiers.iter().all(|t| t.ready_after_ms.is_some()));
        assert!((progress.fraction() - 1.0).abs() < f32::EPSILON);
        queue.shutdown();
    }

    /// Waits (bounded) until `cond` holds.
    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn boot_threads_cap_follows_ram_on_every_desktop() {
        assert_eq!(boot_threads_for(16, false, Some(4 * 1024)), 4, "4 GB machine");
        assert_eq!(boot_threads_for(16, false, Some(8 * 1024)), 12);
        assert_eq!(boot_threads_for(16, false, None), 12, "unknown RAM does not cap");
        assert_eq!(boot_threads_for(16, true, Some(32 * 1024)), 4, "Android");
        assert_eq!(boot_threads_for(2, false, Some(64 * 1024)), 1);
    }

    #[test]
    fn mode_thread_counts_are_bounded() {
        let boot = boot_threads();
        let play = default_threads();
        assert!((1..=12).contains(&boot));
        assert!((1..=4).contains(&play));
        assert_eq!(WarmMode::Boot.threads(), boot);
        assert_eq!(WarmMode::Play.threads(), play);
        assert_eq!(PipelineWarmQueue::default().threads(), boot);
    }

    #[test]
    fn the_worker_count_follows_the_mode() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(4);
        // Before start only the target moves.
        queue.set_threads(3);
        assert_eq!((queue.threads(), queue.live_workers()), (3, 0));
        queue.start(&device);
        assert_eq!(queue.live_workers(), 3);
        queue.set_threads(6);
        assert_eq!(queue.live_workers(), 6);
        queue.set_threads(2);
        assert_eq!(queue.threads(), 2);
        assert!(wait_for(|| queue.live_workers() == 2), "extra workers retire");
        queue.set_threads(0);
        assert_eq!(queue.threads(), 1, "at least one worker");
        assert!(wait_for(|| queue.live_workers() == 1));
        queue.shutdown();
        queue.set_threads(5);
        assert_eq!(queue.live_workers(), 1, "no spawn after shutdown");
    }

    #[test]
    fn a_host_without_game_frame_warm_switches_on_its_first_scene_frame() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(WarmMode::Boot.threads().max(5));
        queue.start(&device);
        queue.note_scene_frame();
        assert_eq!(queue.threads(), WarmMode::Play.threads());
        // The host's own override sticks: later frames do not undo it.
        queue.set_mode(WarmMode::Boot);
        queue.note_scene_frame();
        assert_eq!(queue.threads(), WarmMode::Boot.threads());
        queue.shutdown();
    }

    #[test]
    fn set_threads_racing_shutdown_leaves_no_unjoined_worker() {
        let device = crate::device::test_device();
        for _ in 0..20 {
            let queue = PipelineWarmQueue::isolated(1);
            queue.start(&device);
            let grower = {
                let queue = queue.clone();
                std::thread::spawn(move || {
                    for n in 2..8 {
                        queue.set_threads(n);
                    }
                })
            };
            queue.shutdown();
            let _ = grower.join();
            let left = queue
                .shared
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len();
            assert_eq!(left, 0, "every spawned worker was joined");
        }
    }

    #[test]
    fn shrinking_does_not_drop_queued_jobs() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(4);
        queue.start(&device);
        let ran = Arc::new(AtomicUsize::new(0));
        for i in 0..40 {
            let ran = ran.clone();
            queue.push(WarmTier::Map, format!("job {i}"), move |_| {
                std::thread::sleep(Duration::from_millis(2));
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            if i == 10 {
                queue.set_threads(1);
            }
            if i == 25 {
                queue.set_threads(3);
            }
        }
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(30)));
        assert_eq!(ran.load(Ordering::SeqCst), 40);
        assert_eq!(queue.progress().total, 40);
        queue.shutdown();
    }

    #[test]
    fn shutdown_after_a_mode_change_joins_every_worker() {
        let device = crate::device::test_device();
        for _ in 0..20 {
            let queue = PipelineWarmQueue::isolated(4);
            queue.start(&device);
            for i in 0..8 {
                queue.push(WarmTier::Map, format!("job {i}"), |_| {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(())
                });
            }
            queue.set_threads(1);
            queue.set_threads(5);
            queue.set_mode(WarmMode::Play);
            queue.shutdown();
            assert!(queue.shared.handles.lock().expect("handles").is_empty());
            assert_eq!(queue.shared.busy.load(Ordering::Acquire), 0);
            assert!(queue.device_id().is_none(), "the device is released");
        }
    }

    #[test]
    fn a_panicking_job_is_a_failure_not_a_dead_worker() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.push(WarmTier::Boot, "panics", |_| panic!("boom"));
        queue.push(WarmTier::Menu, "ok", |_| Ok(()));
        queue.start(&device);
        assert!(queue.wait_tier(WarmTier::Menu, Duration::from_secs(30)));
        let p = queue.progress();
        assert_eq!(p.tiers[0].jobs_failed, 1);
        assert_eq!(p.tiers[1].jobs_done, 1);
        queue.shutdown();
    }

    /// Isolated: a worker compiling a parallel test's deferred pipeline may rightly outlast the
    /// wait (`shutdown_leaves_no_other_holder_under_contention`).
    #[test]
    fn shutdown_joins_the_workers_and_releases_the_device() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(2);
        queue.push(WarmTier::Boot, "ok", |_| Ok(()));
        queue.start(&device);
        assert!(queue.wait_tier(WarmTier::Boot, Duration::from_secs(30)));
        queue.shutdown();
        assert!(!queue.is_started(), "the queue no longer holds the device");
        assert!(
            queue
                .shared
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "every worker was joined"
        );
        assert_eq!(
            Arc::strong_count(&queue.shared),
            1,
            "no worker thread still holds the queue"
        );
        // A later start is refused: the queue is shut down.
        queue.start(&device);
        assert!(!queue.is_started());
    }

    /// Stress: 200 start/shutdown cycles while other threads keep asking whether warm workers
    /// exist and waking the queues (every frame and every deferred pipeline does), with the
    /// CPU oversubscribed. `shutdown` must return with nothing but the caller holding the queue,
    /// every time. The original test failed once under a loaded gate. Two holders were found:
    /// a thread that had upgraded the queue's registry handle (fails here at once without the
    /// fix), and a worker compiling a parallel test's deferred pipeline past the 2 s wait. That
    /// is the documented bounded path, so these queues are [`PipelineWarmQueue::isolated`].
    #[test]
    fn shutdown_leaves_no_other_holder_under_contention() {
        let device = crate::device::test_device();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let spinners = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = background_available();
                        notify_queues();
                    }
                });
            }
            for _ in 0..spinners {
                scope.spawn(|| {
                    let mut x = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
                    }
                });
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for round in 0..200 {
                    let queue = PipelineWarmQueue::isolated(2);
                    queue.start(&device);
                    let t = Instant::now();
                    queue.shutdown();
                    assert!(!queue.is_started());
                    assert_eq!(
                        Arc::strong_count(&queue.shared),
                        1,
                        "round {round}: another thread still holds the queue after shutdown \
                         ({:?}, {} worker(s) in a job)",
                        t.elapsed(),
                        queue.shared.busy.load(Ordering::Acquire)
                    );
                }
            }));
            stop.store(true, Ordering::Relaxed);
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        });
    }

    /// An idle worker the scheduler leaves off-CPU past `shutdown`'s wait (here: it sleeps
    /// 300 ms after seeing the shutdown; the wait is 20 ms) is still joined before `shutdown`
    /// returns. Only a worker inside a job may be left to the exit hook.
    #[test]
    fn shutdown_joins_an_idle_worker_that_is_slow_to_exit() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(2);
        queue.shared.exit_delay_ms.store(300, Ordering::Relaxed);
        queue.start(&device);
        let t = Instant::now();
        shutdown_shared(&queue.shared, Duration::from_millis(20), true);
        assert!(
            t.elapsed() >= Duration::from_millis(250),
            "the workers were waited for"
        );
        assert_eq!(
            Arc::strong_count(&queue.shared),
            1,
            "no idle worker outlives shutdown"
        );
    }

    #[test]
    fn tier_names_round_trip() {
        for tier in WarmTier::ALL {
            assert_eq!(WarmTier::from_name(tier.name()), Some(tier));
        }
        assert_eq!(WarmTier::from_name("nope"), None);
    }

    /// Quit at a fixed boundary on both the worker and its `join` helper. Each completes ten
    /// compiles, then waits for shutdown to drop a queued closure holding its release sender.
    /// Neither may complete another compile; the 100 queued jobs remain unrun, and shutdown
    /// joins the worker and releases the device. Scheduler latency is not a correctness budget.
    #[test]
    fn shutdown_stops_a_long_job_between_compiles_and_joins_the_worker() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(1);
        let compiles = Arc::new(AtomicU64::new(0));
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let (release_worker, resume_worker) = std::sync::mpsc::channel::<()>();
        let (release_helper, resume_helper) = std::sync::mpsc::channel::<()>();
        let counter = compiles.clone();
        queue.push(WarmTier::Boot, "long cold warm", move |device| {
            let spin = |resume: std::sync::mpsc::Receiver<()>| {
                for _ in 0..10 {
                    let _ = crate::pipeline_cache::cache_for(device);
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                let _ = started_tx.send(());
                // signal_shutdown sets cancellation before dropping pending jobs. Their
                // senders disconnect these receivers, so both next compiles see the cancel.
                let _ = resume.recv();
                for _ in 0..2000 {
                    let _ = crate::pipeline_cache::cache_for(device);
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            };
            join(|| spin(resume_worker), || spin(resume_helper));
            Ok(())
        });
        queue.push(WarmTier::Map, "queued 0", move |_| {
            drop((release_worker, release_helper));
            panic!("a queued job ran after shutdown")
        });
        for i in 1..100 {
            queue.push(WarmTier::Map, format!("queued {i}"), |_| {
                panic!("a queued job ran after shutdown")
            });
        }
        queue.start(&device);
        started.recv().expect("one side reached the compile boundary");
        started.recv().expect("both sides reached the compile boundary");
        let before_shutdown = compiles.load(Ordering::Relaxed);

        queue.shutdown();
        assert_eq!(before_shutdown, 20);
        assert!(
            queue
                .shared
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the worker was joined"
        );
        assert_eq!(
            Arc::strong_count(&queue.shared),
            1,
            "no worker still holds the queue after shutdown"
        );
        assert_eq!(compiles.load(Ordering::Relaxed), 20, "no compile after shutdown");
        assert_eq!(
            queue.shared.cancel.created.load(Ordering::Relaxed),
            22,
            "both sides reached their next cancellation check"
        );
        let state = queue.lock();
        assert!(state.device.is_none(), "the queue released its device");
        assert!(
            state
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("long cold warm: canceled at shutdown")),
            "{:?}",
            state.last_error
        );
        assert_eq!(
            state.stats[WarmTier::Map as usize].done,
            0,
            "queued jobs never ran"
        );
    }

    /// How long a test waits for a warm job before it counts one idle window ([`Stall`]). Two
    /// windows in a row with no new pipeline and no event call the job hung. Only a stuck job
    /// reaches this: a job that keeps compiling never trips it, however slowly (ADR-2681).
    const HANG_WINDOW: Duration = Duration::from_secs(90);

    /// Counts the windows in a row in which a job created no pipeline. It takes two, not one: a
    /// process frozen during a window (a stopped test run) sees that window expire at once when
    /// it thaws, before the job had a chance to go on.
    #[derive(Default)]
    struct Stall {
        seen: u64,
        idle: u32,
    }

    impl Stall {
        /// A window passed and the job has created `created` pipelines; whether it looks hung.
        fn window_passed(&mut self, created: u64) -> bool {
            self.idle = if created == self.seen { self.idle + 1 } else { 0 };
            self.seen = created;
            self.idle >= 2
        }
    }

    /// Moved into a job: dropping it is the job's end on every path, because the closure owns it
    /// whether the job returns, fails, unwinds or is dropped unrun.
    struct JobEnd(std::sync::mpsc::Sender<PauseEvent>);

    impl Drop for JobEnd {
        fn drop(&mut self) {
            let _ = self.0.send(PauseEvent::JobEnded);
        }
    }

    /// Arms `cancel`'s pause at pipeline `at`. Returns the job's events, the sender whose drop
    /// releases the paused warm thread, and the [`JobEnd`] for the job to own.
    fn arm_pause(
        cancel: &Cancel,
        at: u64,
    ) -> (
        std::sync::mpsc::Receiver<PauseEvent>,
        std::sync::mpsc::Sender<()>,
        JobEnd,
    ) {
        let (events_tx, events) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        *cancel
            .pause
            .meeting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((events_tx.clone(), resume_rx));
        cancel.pause.at.store(at, Ordering::Relaxed);
        (events, resume, JobEnd(events_tx))
    }

    /// The job's next event, or why there is none: it stopped creating pipelines for two windows
    /// of `window`. How slow the machine is does not matter while the job makes progress.
    fn next_event(
        events: &std::sync::mpsc::Receiver<PauseEvent>,
        cancel: &Cancel,
        window: Duration,
    ) -> Result<PauseEvent, String> {
        let mut stall = Stall {
            seen: cancel.created.load(Ordering::Relaxed),
            idle: 0,
        };
        loop {
            match events.recv_timeout(window) {
                Ok(event) => return Ok(event),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the job's channel closed with no end reported".to_owned());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if stall.window_passed(cancel.created.load(Ordering::Relaxed)) {
                        return Err(format!(
                            "the job looks hung: no new pipeline and no event over two {window:?} windows"
                        ));
                    }
                }
            }
        }
    }

    /// `shutdown_shared` on a helper thread; `false` if it did not return over two windows (a
    /// worker stuck after its job: joining an idle worker has no deadline of its own).
    fn shut_down_guarded(shared: &Arc<Shared>, window: Duration) -> bool {
        let (joined_tx, joined) = std::sync::mpsc::channel::<()>();
        let shared = Arc::clone(shared);
        std::thread::spawn(move || {
            shutdown_shared(&shared, window, true);
            let _ = joined_tx.send(());
        });
        joined.recv_timeout(window * 2).is_ok()
    }

    /// Fails a quit test: asks the job to stop, then panics with what is known. The text also
    /// goes straight to stderr, past the test harness's capture: the harness prints a failure's
    /// text only after the last test, and a runner that kills a slow binary loses it.
    fn fail_quit(queue: &PipelineWarmQueue, after: u64, why: &str) -> ! {
        use std::io::Write as _;
        signal_shutdown(&queue.shared);
        let described = queue.describe();
        let last_error = queue.lock().last_error.clone();
        let text = format!(
            "quit before pipeline {}: {why} ({} pipelines created; queue: {described}; last error: {last_error:?})",
            after + 1,
            queue.shared.cancel.created.load(Ordering::Relaxed),
        );
        let _ = writeln!(std::io::stderr(), "{text}");
        panic!("{text}");
    }

    /// Quits during a cold frame warm right before the job creates pipeline number `after + 1`,
    /// on a queue that does not unwind (the shipping build's path). The warm thread reports that
    /// pipeline (`TestPause`) and waits while the shutdown is signalled, so the quit lands at a
    /// known point of the frame with no clock involved (ADR-2585). The test then waits for the
    /// job's end, an event every exit path sends (ADR-2681), and joins the worker. Fails if the
    /// job did not report that it was canceled, if it created more than 16 pipelines after the
    /// cancel, or if a frame warm that began after the cancel ran to its end (the job running on
    /// instead of stopping between passes).
    fn quit_frame_warm_after(after: u64) {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.shared.cancel.unwind.store(false, Ordering::Relaxed);
        let cancel = queue.shared.cancel.clone();
        let (events, resume, job_end) = arm_pause(&cancel, after + 1);
        let ran_on = Arc::new(AtomicU64::new(0));
        let job_ran_on = Arc::clone(&ran_on);
        queue.push(WarmTier::Optional, "frame warms", move |device| {
            let _job_end = job_end;
            let frame = canonical_frame();
            // Fresh renderers one after another: each compiles the canonical frame's whole
            // pipeline set again (seconds on a cold lavapipe), so the job outlasts the test.
            for format in [
                wgpu::TextureFormat::Rgba8UnormSrgb,
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Bgra8Unorm,
            ]
            .into_iter()
            .cycle()
            .take(40)
            {
                let started_after_the_quit = warm_cancelled();
                crate::FrameRenderer::new()
                    .warm_extracted(device, &frame, format, true)
                    .map_err(|e| e.to_string())?;
                // A frame warm that began after the quit and still returned normally passed
                // every pass boundary without stopping. Without the checks the job compiles all
                // 40 (minutes on lavapipe): end it here, so the failure shows after one.
                if started_after_the_quit {
                    job_ran_on.fetch_add(1, Ordering::Relaxed);
                    return Err("ran on after the quit".to_owned());
                }
            }
            Ok(())
        });
        queue.start(&device);
        let started = Instant::now();
        // The job is about to create pipeline `after + 1`: quit (what `shutdown` signals first),
        // then let the job go on. A failure here unwinds, which drops `resume` and frees the job.
        match next_event(&events, &cancel, HANG_WINDOW) {
            Ok(PauseEvent::Reached) => {}
            Ok(PauseEvent::JobEnded) => {
                // Let `run_job` record why before the shutdown flag renames the error.
                let _ = queue.wait_tier(WarmTier::Optional, HANG_WINDOW);
                fail_quit(&queue, after, "the job ended before that pipeline");
            }
            Err(why) => fail_quit(&queue, after, &why),
        }
        let reached = started.elapsed();
        signal_shutdown(&queue.shared);
        let quit = Instant::now();
        drop(resume);
        match next_event(&events, &cancel, HANG_WINDOW) {
            Ok(PauseEvent::JobEnded) => {}
            Ok(PauseEvent::Reached) => fail_quit(&queue, after, "the pause was reached twice"),
            Err(why) => fail_quit(&queue, after, &why),
        }
        let ended = quit.elapsed();
        if !shut_down_guarded(&queue.shared, HANG_WINDOW) {
            fail_quit(&queue, after, "the worker did not exit after its job ended");
        }
        let late = cancel.late.load(Ordering::Relaxed);
        let ran_on = ran_on.load(Ordering::Relaxed);
        // The times are for the log only: nothing below reads them.
        eprintln!(
            "quit before pipeline {}: reached in {reached:?}, job ended {ended:?} after the quit; \
             {late} pipelines created after the cancel, {} by the job, {ran_on} frame warms ran to \
             their end after it",
            after + 1,
            cancel.created.load(Ordering::Relaxed)
        );
        let last_error = queue.lock().last_error.clone();
        if !last_error
            .as_deref()
            .is_some_and(|e| e.contains("frame warms: canceled at shutdown"))
        {
            fail_quit(&queue, after, "the job did not report that it was canceled");
        }
        if late > 16 {
            let why = format!("{late} pipelines compiled after the cancel");
            fail_quit(&queue, after, &why);
        }
        if ran_on != 0 {
            fail_quit(&queue, after, "a frame warm begun after the quit ran to its end");
        }
    }

    /// The shipping build's path (no unwinding): a quit during a real cold frame warm stops the
    /// job at the frame's next pass boundary, wherever in the frame it lands. Only a few
    /// pipelines may be created after the cancel (the compile batch in flight: a `join` of a
    /// pipeline set, a `join_all` of variants), and no frame warm that begins after the quit
    /// may run to its end, so the job returns as cancelled. What it rules out is the job running on:
    /// without the control-flow checks it keeps compiling its 40 frame warms (hundreds of
    /// pipelines, minutes on lavapipe) and never reports cancelled. The quit lands right before
    /// a given pipeline (the warm thread waits for the test there) and the test waits for the
    /// job's end, so nothing here depends on how fast the machine is (ADR-2585: the old 10 s
    /// bound on the job's end failed under load; ADR-2681: a job ending early fails the test
    /// instead of blocking it).
    #[test]
    fn shutdown_stops_a_frame_warm_between_passes_without_unwinding() {
        // Spread over the first frame warm (about 30 pipelines on lavapipe) and into the second.
        for after in [3, 9, 15, 21, 27, 40] {
            quit_frame_warm_after(after);
        }
    }

    /// The same check with the quit landing at every pipeline of the first frame warms, for a
    /// new adapter or a changed frame: the batch size bound above must hold at each one, since
    /// which pipeline a real quit lands on depends on timing. Quadratic in compiles, so ignored:
    /// `--ignored --exact --nocapture`.
    #[test]
    #[ignore = "quadratic in pipeline compiles: minutes on a card, about an hour on lavapipe"]
    fn shutdown_stops_a_frame_warm_wherever_the_quit_lands() {
        for after in 0..=70 {
            quit_frame_warm_after(after);
        }
    }

    /// Runs `job` as the only job of a started queue with its pause armed at a pipeline it never
    /// reaches, and returns the first event and the queue's last error after the worker is joined.
    fn event_of_a_job_that_stops_early(
        job: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> (Result<PauseEvent, String>, Option<String>) {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(1);
        let cancel = queue.shared.cancel.clone();
        let (events, _resume, job_end) = arm_pause(&cancel, u64::MAX);
        queue.push(WarmTier::Optional, "stops early", move |_| {
            let _job_end = job_end;
            job()
        });
        queue.start(&device);
        let event = next_event(&events, &cancel, HANG_WINDOW);
        assert!(
            queue.wait_tier(WarmTier::Optional, HANG_WINDOW),
            "{}",
            queue.describe()
        );
        let error = queue.lock().last_error.clone();
        assert!(
            shut_down_guarded(&queue.shared, HANG_WINDOW),
            "{}",
            queue.describe()
        );
        (event, error)
    }

    /// A job that returns before the pause reports its end, so the test fails at once instead of
    /// waiting for a pipeline that never comes (it used to block forever on a barrier).
    #[test]
    fn a_job_returning_before_the_pause_reports_its_end() {
        let (event, error) = event_of_a_job_that_stops_early(|| Ok(()));
        assert_eq!(event, Ok(PauseEvent::JobEnded));
        assert_eq!(error, None);
    }

    /// As above for a job that fails, and its error is what the test's message shows.
    #[test]
    fn a_job_failing_before_the_pause_reports_its_end() {
        let (event, error) = event_of_a_job_that_stops_early(|| Err("boom".to_owned()));
        assert_eq!(event, Ok(PauseEvent::JobEnded));
        assert_eq!(error.as_deref(), Some("stops early: boom"));
    }

    /// As above for a job that panics; the error carries the panic's text.
    #[test]
    fn a_job_panicking_before_the_pause_reports_its_end() {
        let (event, error) = event_of_a_job_that_stops_early(|| panic!("boom in the warm"));
        assert_eq!(event, Ok(PauseEvent::JobEnded));
        assert_eq!(
            error.as_deref(),
            Some("stops early: warm job panicked: boom in the warm")
        );
    }

    /// A job dropped unrun (the queue shut down first) also reports its end.
    #[test]
    fn a_job_dropped_unrun_reports_its_end() {
        let queue = PipelineWarmQueue::isolated(1);
        let cancel = queue.shared.cancel.clone();
        let (events, _resume, job_end) = arm_pause(&cancel, 1);
        queue.push(WarmTier::Optional, "never runs", move |_| {
            let _job_end = job_end;
            panic!("a dropped job ran")
        });
        queue.shutdown();
        assert_eq!(
            next_event(&events, &cancel, HANG_WINDOW),
            Ok(PauseEvent::JobEnded)
        );
    }

    /// A stuck job trips the guard, with no event ever sent: a clear failure, not a blocked test.
    #[test]
    fn a_stuck_job_trips_the_guard() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(1);
        let cancel = queue.shared.cancel.clone();
        let (events, _resume, job_end) = arm_pause(&cancel, u64::MAX);
        let (unstick, stuck) = std::sync::mpsc::channel::<()>();
        queue.push(WarmTier::Optional, "stuck", move |_| {
            let _job_end = job_end;
            let _ = stuck.recv();
            Ok(())
        });
        queue.start(&device);
        let why = next_event(&events, &cancel, Duration::from_millis(5)).expect_err("no event was sent");
        assert!(why.contains("looks hung"), "{why}");
        drop(unstick);
        assert_eq!(
            next_event(&events, &cancel, HANG_WINDOW),
            Ok(PauseEvent::JobEnded)
        );
        assert!(
            shut_down_guarded(&queue.shared, HANG_WINDOW),
            "{}",
            queue.describe()
        );
    }

    /// A test that fails while the job waits at the pause (it drops its sender) does not strand
    /// the job: the worker goes on and reports the job's end.
    #[test]
    fn a_failed_test_does_not_strand_the_paused_job() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::isolated(1);
        let cancel = queue.shared.cancel.clone();
        let (events, resume, job_end) = arm_pause(&cancel, 2);
        queue.push(WarmTier::Optional, "paused", move |_| {
            let _job_end = job_end;
            // What every pipeline creation on a warm thread calls first.
            cancel_point();
            cancel_point();
            Ok(())
        });
        queue.start(&device);
        assert_eq!(next_event(&events, &cancel, HANG_WINDOW), Ok(PauseEvent::Reached));
        assert_eq!(
            cancel.created.load(Ordering::Relaxed),
            2,
            "paused at the asked number"
        );
        drop(resume);
        assert_eq!(
            next_event(&events, &cancel, HANG_WINDOW),
            Ok(PauseEvent::JobEnded)
        );
        assert!(
            shut_down_guarded(&queue.shared, HANG_WINDOW),
            "{}",
            queue.describe()
        );
    }

    /// Slow is not hung: any new pipeline starts the guard over, however many windows the job
    /// has taken. Only two idle windows in a row call it hung, and one idle window alone (a
    /// process frozen and thawed during it) does not.
    #[test]
    fn only_two_idle_windows_in_a_row_are_a_stall() {
        let mut stall = Stall::default();
        for created in 1..1000 {
            assert!(!stall.window_passed(created), "progress in every window");
        }
        assert!(!stall.window_passed(999), "one idle window");
        assert!(!stall.window_passed(1000), "progress clears it");
        assert!(!stall.window_passed(1000), "one idle window again");
        assert!(stall.window_passed(1000), "two in a row");
    }

    /// Off warm worker threads nothing is ever cancelled: the frame thread and inline warms
    /// keep rendering after a queue shut down.
    #[test]
    fn warm_cancelled_is_false_off_the_workers() {
        let queue = PipelineWarmQueue::new(1);
        queue.shutdown();
        assert!(!warm_cancelled());
        assert!(warm_checkpoint().is_ok());
    }

    /// An optional warm step that fails is logged and the warm continues (v0.1.14 review).
    #[test]
    fn a_failing_optional_step_does_not_end_the_warm() {
        let failed = optional_warm_step("test", Err(crate::RenderError::Encode("boom".into())));
        assert_eq!(failed.ok(), Some(0));
        assert_eq!(optional_warm_step("test", Ok(7)).ok(), Some(7));
    }
}
