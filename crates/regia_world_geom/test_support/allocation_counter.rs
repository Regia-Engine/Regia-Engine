// SPDX-License-Identifier: MPL-2.0
//! One audited counting global allocator for the allocation tests of every crate, and the rules
//! that keep their counts the same on a loaded machine and in every build (ADR-5718).
//!
//! A test binary that counts heap allocations installs [`CountingAlloc`] and reads the counters
//! through the functions of this module; no test writes its own `unsafe impl GlobalAlloc`:
//!
//! ```ignore
//! use regia_test_utils::alloc_counter::{self, CountingAlloc};
//!
//! #[global_allocator]
//! static GLOBAL: CountingAlloc = CountingAlloc::new();
//!
//! #[test]
//! fn a_tick_allocates_nothing() {
//!     let ((), counted) = alloc_counter::measure(|| tick());
//!     assert_eq!(counted.allocs, 0);
//! }
//! ```
//!
//! The global allocator is per binary, so a test file that installs it is its own test binary,
//! and counters that are not per thread (`counted_stats`, `process_stats`, the largest request,
//! the process's live bytes) see every test of that binary running beside it.
//!
//! # What is counted
//!
//! Every request (`alloc`, `alloc_zeroed`, `realloc`) is **one allocation**; a `realloc` is not an
//! allocation plus a free. Its **bytes** are the size asked for by `alloc` and `alloc_zeroed`, and
//! the growth (`new_size - old_size`, 0 when it shrinks) of a `realloc`. Requests are counted when
//! they are made, before the system allocator answers, so a refused request still counts (the
//! largest request is what the hostile-input tests look for). **Live bytes** move only on success:
//! up by the size of a new block, down by the size of a freed one, by the difference on a
//! `realloc`. `dealloc` is not a request.
//!
//! # Whose allocations
//!
//! - **This thread** ([`thread_stats`], [`measure`], [`thread_live_bytes`], [`peak_bytes`]):
//!   requests made on the calling thread, from the thread's start. A test's measured work runs on
//!   the test's own thread, so the other tests of the binary and the runtime's helper threads do
//!   not leak in.
//! - **Counted threads** ([`count_this_thread`], [`counted_stats`]): the threads that called
//!   [`count_this_thread`], summed. For work spread over threads the test controls, such as the
//!   renderer's encode workers (`FrameRenderer::set_encode_worker_start_hook(count_this_thread)`).
//! - **The process** ([`process_stats`], [`process_live_bytes`], [`process_peak_bytes`],
//!   [`largest_request`]): every thread.
//!
//! The per-thread state is `const`-initialized and has no destructor, so counting works on every
//! thread at any time, including while thread-local storage is torn down.
//!
//! # Hooks
//!
//! A test that needs more than counts (a backtrace per allocation, the hits of one request size)
//! passes an [`AllocHook`] to [`CountingAlloc::with_hook`]. The hook runs after the counters on
//! every request of every thread; an allocation the hook itself makes is counted but does not call
//! the hook again.
//!
//! # Steady frames
//!
//! [`SteadyAllocCheck`] holds the steady-frame rule (warm, settle, measure, no growth) that the
//! renderer's frame tests share; its docs state the rule.
//!
//! # What made these tests fail at random, and the rule against each (ADR-5718)
//!
//! - **Work that moves between threads.** A frame's work is the same every frame, but which thread
//!   does it is not: the drawing thread finishes an encode segment itself when no worker has
//!   started it (ADR-2299), which happens on a loaded machine. A count of the drawing thread alone
//!   then jumps by a segment's allocations for one frame. Count the frame's threads together:
//!   [`count_this_thread`] on the drawing thread and as the renderer's encode-worker start hook,
//!   then [`counted_allocs`].
//! - **Tests side by side.** [`process_stats`] and the heap ledger of `regia_core::memory` see every
//!   thread, so another test of the same binary that is still building its input lands in the
//!   measured window. A lock around the measured call alone does not help. Take [`exclusive`] as
//!   the first line of every test of a binary that reads process-wide numbers.
//! - **The device's pools.** wgpu allocates for its own pools at times that depend on what the
//!   device has finished and on how many threads encode at once. Each has its rule:
//!   - Its command-buffer free lists refill 16 at a time, and each pooled encoder grows its own
//!     framebuffer cache the first time it records a given pass. Both settle only when every
//!     frame starts from a drained device: a measured draw ends with
//!     `device.poll(wgpu::PollType::wait_indefinitely())`, and [`SteadyAllocCheck`] warms 40
//!     frames and settles before it measures.
//!   - Its pool of usage scopes gains a scope, 6 allocations, the first time one more thread
//!     replays a pass at the same moment; a pooled scope's lists grow (up to 6 reallocations)
//!     when it is taken again after the device has made more resources; and its map of textures
//!     used in part grows the first time that scope serves a pass with more of them. All three
//!     land in a frame the scheduler picks. [`crate::device_pools::fill_usage_scope_pool`] makes
//!     them happen before the batches are counted ([`SteadyAllocCheck::run_warmed`]).
//!   - A buffer mapped right after a submit costs one allocation more while that submission is
//!     still in flight than once the device has finished it, and wgpu's submit already looks for
//!     finished work before it returns, so which one a map gets depends on the machine's load.
//!     A frame maps buffers after its submit (the upload ring's chunk, its readbacks), and
//!     `render_offscreen` maps the pixel readback: a batch then counts up to one allocation per
//!     frame fewer, in streaks. So a measured frame does not read back (`begin_offscreen`, or
//!     `render_into` a target the test owns), and the test turns on
//!     `RenderDevice::wait_after_submit_on_this_thread` (a `regia_render` test hook): every map
//!     then follows a finished submission.
//! - **The build's feature set.** A whole-workspace build turns on `tracing`'s `log` feature, and
//!   then, in a process with no subscriber, every system run on the single-threaded executor
//!   allocates a `String` for a disabled span. A build of one crate does not. [`quiet_tracing`]
//!   (its docs have the details) puts the process in the state of an engine host, where that span
//!   costs nothing; [`measure`] and [`SteadyAllocCheck::run`] call it.
//!
//! # Where an allocation comes from
//!
//! [`trace`] runs a closure with a backtrace captured for every request of this thread (and of the
//! counted threads with [`trace_counted`]); [`AllocTrace::report`] lists the sites. A failing
//! exact-count test can print it (`assert_eq!(counted.allocs, 0, "{}", trace(step).1.report(8))`).
//! With `REGIA_ALLOC_TRACE=1` set, [`SteadyAllocCheck::run`] names the sites that grew when it
//! fails, and a run that passes lists every batch that counted differently from the usual one,
//! with the sites that made more requests and the sites that made fewer
//! ([`SteadyAllocRun::deviations`]): how each event above was found while it was still inside
//! the tests' slack.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// What a request asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocKind {
    /// `GlobalAlloc::alloc`.
    Alloc,
    /// `GlobalAlloc::alloc_zeroed`.
    Zeroed,
    /// `GlobalAlloc::realloc`; `size` is the new size.
    Realloc,
}

/// One request, as an [`AllocHook`] sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocEvent {
    /// What was asked for.
    pub kind: AllocKind,
    /// Bytes asked for (the new size of a `realloc`).
    pub size: usize,
    /// Whether the requesting thread called [`count_this_thread`].
    pub counted_thread: bool,
}

/// Extra work on every allocation request, for a test that needs more than the counters.
///
/// The hook runs inside the global allocator: it must not panic. It may allocate (that allocation
/// is counted and does not re-enter the hook), but it should keep to fixed-size work, such as
/// pushing into a vector reserved up front.
pub trait AllocHook: Sync {
    /// Whether the allocator calls [`AllocHook::on_request`] at all. [`NoHook`] says no, which
    /// leaves the allocator with only the counters.
    const ACTIVE: bool = true;
    /// Called after the counters on every request of every thread.
    fn on_request(&self, event: AllocEvent);
}

/// The hook of a [`CountingAlloc`] that only counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHook;

impl AllocHook for NoHook {
    const ACTIVE: bool = false;
    fn on_request(&self, _event: AllocEvent) {}
}

/// The counting global allocator: forwards every call to [`System`] and counts (module docs).
#[derive(Debug, Default)]
pub struct CountingAlloc<H: AllocHook = NoHook> {
    hook: H,
}

impl CountingAlloc<NoHook> {
    /// The allocator with counters only.
    pub const fn new() -> Self {
        Self { hook: NoHook }
    }
}

impl<H: AllocHook> CountingAlloc<H> {
    /// The allocator with `hook` called on every request.
    pub const fn with_hook(hook: H) -> Self {
        Self { hook }
    }

    fn request(&self, kind: AllocKind, size: usize, grown: usize) {
        // A request the tracer makes while it captures a backtrace is not the test's: not counted.
        if THREAD.try_with(|t| t.capturing.get()).unwrap_or(false) {
            return;
        }
        let counted_thread = note_request(size, grown);
        trace_request(kind, size, counted_thread);
        if H::ACTIVE {
            let entered = THREAD.try_with(|t| !t.in_hook.replace(true)).unwrap_or(false);
            if entered {
                self.hook.on_request(AllocEvent {
                    kind,
                    size,
                    counted_thread,
                });
                let _ = THREAD.try_with(|t| t.in_hook.set(false));
            }
        }
    }
}

/// Requests and bytes (module docs: what is counted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocStats {
    /// Requests: `alloc`, `alloc_zeroed` and `realloc`, one each.
    pub allocs: u64,
    /// Bytes asked for: the size of an `alloc`, the growth of a `realloc`.
    pub bytes: u64,
}

impl AllocStats {
    /// The counts since `earlier` (both read from the same counter).
    pub fn since(self, earlier: AllocStats) -> AllocStats {
        AllocStats {
            allocs: self.allocs.wrapping_sub(earlier.allocs),
            bytes: self.bytes.wrapping_sub(earlier.bytes),
        }
    }
}

struct ThreadState {
    counted: Cell<bool>,
    in_hook: Cell<bool>,
    /// Whether [`trace`] is running on this thread.
    traced: Cell<bool>,
    /// Whether the tracer is capturing a backtrace on this thread right now.
    capturing: Cell<bool>,
    allocs: Cell<u64>,
    bytes: Cell<u64>,
    live: Cell<i64>,
    peak: Cell<i64>,
}

impl ThreadState {
    const fn new() -> Self {
        Self {
            counted: Cell::new(false),
            in_hook: Cell::new(false),
            traced: Cell::new(false),
            capturing: Cell::new(false),
            allocs: Cell::new(0),
            bytes: Cell::new(0),
            live: Cell::new(0),
            peak: Cell::new(0),
        }
    }
}

thread_local! {
    static THREAD: ThreadState = const { ThreadState::new() };
}

static COUNTED_ALLOCS: AtomicU64 = AtomicU64::new(0);
static COUNTED_BYTES: AtomicU64 = AtomicU64::new(0);
static PROCESS_ALLOCS: AtomicU64 = AtomicU64::new(0);
static PROCESS_BYTES: AtomicU64 = AtomicU64::new(0);
static PROCESS_LIVE: AtomicI64 = AtomicI64::new(0);
static PROCESS_PEAK: AtomicI64 = AtomicI64::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

fn to_i64(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// Counts one request; returns whether the thread is a counted one.
fn note_request(size: usize, grown: usize) -> bool {
    let grown = grown as u64;
    let counted = THREAD
        .try_with(|t| {
            t.allocs.set(t.allocs.get().wrapping_add(1));
            t.bytes.set(t.bytes.get().wrapping_add(grown));
            t.counted.get()
        })
        .unwrap_or(false);
    if counted {
        COUNTED_ALLOCS.fetch_add(1, Ordering::Relaxed);
        COUNTED_BYTES.fetch_add(grown, Ordering::Relaxed);
    }
    PROCESS_ALLOCS.fetch_add(1, Ordering::Relaxed);
    PROCESS_BYTES.fetch_add(grown, Ordering::Relaxed);
    if size > LARGEST.load(Ordering::Relaxed) {
        LARGEST.fetch_max(size, Ordering::Relaxed);
    }
    counted
}

/// Moves the live bytes of this thread and of the process by `delta` (a block came or went).
fn note_live(delta: i64) {
    let _ = THREAD.try_with(|t| {
        let live = t.live.get().wrapping_add(delta);
        t.live.set(live);
        if live > t.peak.get() {
            t.peak.set(live);
        }
    });
    let live = PROCESS_LIVE
        .fetch_add(delta, Ordering::Relaxed)
        .wrapping_add(delta);
    if live > PROCESS_PEAK.load(Ordering::Relaxed) {
        PROCESS_PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System` with the caller's own arguments, so `System` upholds
// the `GlobalAlloc` contract for each block; the counting around the call only touches atomics and
// `const`-initialized thread-local cells (no allocation, no unwinding), and the hook is guarded
// against re-entry and documented as not panicking.
unsafe impl<H: AllocHook> GlobalAlloc for CountingAlloc<H> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.request(AllocKind::Alloc, layout.size(), layout.size());
        // SAFETY: the caller's contract for `alloc` (a non-zero-sized `layout`) is passed on as is.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            note_live(to_i64(layout.size()));
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.request(AllocKind::Zeroed, layout.size(), layout.size());
        // SAFETY: the caller's contract for `alloc_zeroed` is passed on as is.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            note_live(to_i64(layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` came from this allocator with `layout`; every block
        // this allocator hands out came from `System` with the same layout.
        unsafe { System.dealloc(ptr, layout) };
        note_live(-to_i64(layout.size()));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.request(
            AllocKind::Realloc,
            new_size,
            new_size.saturating_sub(layout.size()),
        );
        // SAFETY: the caller guarantees `ptr` came from this allocator with `layout` and that
        // `new_size` is valid for `layout`'s alignment; the block came from `System`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            note_live(to_i64(new_size) - to_i64(layout.size()));
        }
        new
    }
}

/// This thread's requests and bytes since the thread started.
pub fn thread_stats() -> AllocStats {
    THREAD
        .try_with(|t| AllocStats {
            allocs: t.allocs.get(),
            bytes: t.bytes.get(),
        })
        .unwrap_or_default()
}

/// This thread's requests since the thread started (`thread_stats().allocs`).
pub fn thread_allocs() -> u64 {
    thread_stats().allocs
}

/// Runs `f` and returns what it asked for on this thread (the scoped region of this thread).
/// Calls [`quiet_tracing`] first, so the count does not depend on the build's feature set.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    quiet_tracing();
    let before = thread_stats();
    let out = f();
    (out, thread_stats().since(before))
}

/// Bytes this thread allocated minus the bytes it freed, since it started. Negative when it freed
/// more than it allocated (blocks another thread allocated).
pub fn thread_live_bytes() -> i64 {
    THREAD.try_with(|t| t.live.get()).unwrap_or(0)
}

/// Runs `f` and returns the most this thread's live bytes rose above their value at the start of
/// `f`, at any moment during `f` (0 when they never rose). Calls may nest.
pub fn peak_bytes<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let (base, outer_peak) = THREAD
        .try_with(|t| {
            let base = t.live.get();
            (base, t.peak.replace(base))
        })
        .unwrap_or((0, 0));
    let out = f();
    let peak = THREAD
        .try_with(|t| {
            let peak = t.peak.get();
            t.peak.set(peak.max(outer_peak));
            peak
        })
        .unwrap_or(base);
    (out, u64::try_from(peak - base).unwrap_or(0))
}

/// Counts this thread's requests in [`counted_stats`] from now on. A plain `fn()`, so it can be a
/// thread-start hook (`FrameRenderer::set_encode_worker_start_hook(count_this_thread)`).
pub fn count_this_thread() {
    let _ = THREAD.try_with(|t| t.counted.set(true));
}

/// Stops counting this thread's requests in [`counted_stats`].
pub fn stop_counting_this_thread() {
    let _ = THREAD.try_with(|t| t.counted.set(false));
}

/// Whether this thread's requests count in [`counted_stats`].
pub fn is_this_thread_counted() -> bool {
    THREAD.try_with(|t| t.counted.get()).unwrap_or(false)
}

/// Requests and bytes of every thread that called [`count_this_thread`] (while it was counted).
pub fn counted_stats() -> AllocStats {
    AllocStats {
        allocs: COUNTED_ALLOCS.load(Ordering::Relaxed),
        bytes: COUNTED_BYTES.load(Ordering::Relaxed),
    }
}

/// Requests of the counted threads (`counted_stats().allocs`).
pub fn counted_allocs() -> u64 {
    COUNTED_ALLOCS.load(Ordering::Relaxed)
}

/// Requests and bytes of every thread of the process.
pub fn process_stats() -> AllocStats {
    AllocStats {
        allocs: PROCESS_ALLOCS.load(Ordering::Relaxed),
        bytes: PROCESS_BYTES.load(Ordering::Relaxed),
    }
}

/// Requests of every thread of the process (`process_stats().allocs`).
pub fn process_allocs() -> u64 {
    PROCESS_ALLOCS.load(Ordering::Relaxed)
}

/// Bytes allocated minus bytes freed by every thread, since the process started.
pub fn process_live_bytes() -> i64 {
    PROCESS_LIVE.load(Ordering::Relaxed)
}

/// Runs `f` and returns the most the process's live bytes rose above their value at the start of
/// `f` (every thread counts). Not for tests running beside each other in one binary: they share
/// the process peak.
pub fn process_peak_bytes<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let base = PROCESS_LIVE.load(Ordering::Relaxed);
    PROCESS_PEAK.store(base, Ordering::Relaxed);
    let out = f();
    let peak = PROCESS_PEAK.load(Ordering::Relaxed);
    (out, u64::try_from(peak - base).unwrap_or(0))
}

/// The largest single request (`alloc` size or `realloc` new size) of any thread since the last
/// [`reset_largest_request`], refused requests included.
pub fn largest_request() -> usize {
    LARGEST.load(Ordering::Relaxed)
}

/// Starts [`largest_request`] over at 0.
pub fn reset_largest_request() {
    LARGEST.store(0, Ordering::Relaxed);
}

/// Runs `f` and returns the largest request of any thread made during it.
pub fn largest_request_in<R>(f: impl FnOnce() -> R) -> (R, usize) {
    reset_largest_request();
    let out = f();
    (out, largest_request())
}

/// Puts `tracing` in the state it has in an engine host, so a count does not depend on the
/// build's feature set.
///
/// `tracing` has a `log` feature that this workspace never asks for, but a whole-workspace build
/// gets it through feature unification (the Wayland event loop and the crash symbolizer enable
/// it); a build of one engine crate does not. With that feature on, and until a dispatcher has
/// been set once in the process, every *disabled* span and event still evaluates its field
/// expressions for the `log` fallback. `bevy_ecs` opens `check_conditions` per system per run on
/// the single-threaded executor with the system's name copied into a new `String`, so each system
/// run then allocates once, whatever the system does: in the merge gate's build, and not under
/// `cargo test -p <crate>`. An engine host is not affected: `regia_core::log::init` installs a
/// subscriber, which is the dispatcher, and its `keeps_callsite` filter drops those spans.
///
/// This sets a no-op dispatcher for the calling thread and removes it again, which is all
/// `tracing` needs to stop the fallback, on every thread, for the rest of the process. It installs
/// no global subscriber, so a test can still install its own afterwards. Idempotent and cheap.
///
/// [`measure`] and [`SteadyAllocCheck::run`] call it; a test that reads the raw counters around
/// systems or spans calls it once before its warm-up. [`trace`] does not call it, so a trace taken
/// first shows what the build's `tracing` allocates by itself.
pub fn quiet_tracing() {
    drop(tracing::dispatcher::set_default(&tracing::Dispatch::none()));
}

static EXCLUSIVE: Mutex<()> = Mutex::new(());

/// Held by a test that reads process-wide numbers ([`exclusive`]).
#[must_use = "the other tests of the binary run again as soon as this is dropped"]
#[derive(Debug)]
pub struct Exclusive {
    _guard: MutexGuard<'static, ()>,
}

/// Runs the calling test alone among the tests of its binary that also call this, until the
/// returned guard is dropped.
///
/// For a binary whose tests read process-wide numbers ([`process_stats`], [`process_peak_bytes`],
/// the heap ledger of `regia_core::memory`): take it as the **first line of every test**, before
/// the test builds its input. A lock around the measured call alone lets another test's setup run
/// inside the window: `regia_asset`'s `perf_hot_alloc` failed 20 runs of 20 that way with 4 test
/// threads and none of 20 with one (ADR-5718). The test harness's own threads still run (they
/// print a result line between tests), so a process-wide number carries a few small requests of
/// noise: use it for byte bounds with slack, and the per-thread or counted-thread counters for an
/// exact count.
pub fn exclusive() -> Exclusive {
    Exclusive {
        _guard: untracked(|| EXCLUSIVE.lock().unwrap_or_else(PoisonError::into_inner)),
    }
}

/// Runs `f` with this thread's requests left out of every count and trace: this module's own
/// bookkeeping. Where std's mutexes are pthread objects (macOS), a static mutex allocates its state
/// on its first lock, on whichever thread locks it first: inside a test's [`measure`] that was 2
/// requests more than the test made, in whichever test traced first (ADR-7484, the M5 runs).
fn untracked<R>(f: impl FnOnce() -> R) -> R {
    let was = THREAD.try_with(|t| t.capturing.replace(true)).unwrap_or(false);
    let out = f();
    let _ = THREAD.try_with(|t| t.capturing.set(was));
    out
}

/// Whether the counted threads' requests are traced ([`trace_counted`]).
static TRACE_COUNTED: AtomicBool = AtomicBool::new(false);
/// The mark stored with each traced request ([`set_trace_mark`]).
static TRACE_MARK: AtomicU64 = AtomicU64::new(0);
/// The requests traced so far in the running [`trace`].
static TRACED: Mutex<Vec<Captured>> = Mutex::new(Vec::new());
/// One [`trace`] at a time: they share `TRACED`.
static TRACE_SESSION: Mutex<()> = Mutex::new(());

struct Captured {
    kind: AllocKind,
    size: usize,
    mark: u64,
    /// The address of the requesting thread's state: the same for every request of one thread.
    thread: usize,
    backtrace: Backtrace,
}

/// Stores a backtrace for a request of a traced thread. Runs inside the global allocator.
fn trace_request(kind: AllocKind, size: usize, counted_thread: bool) {
    let (traced, thread) = THREAD
        .try_with(|t| (t.traced.get(), std::ptr::from_ref(t) as usize))
        .unwrap_or((false, 0));
    if !(traced || (counted_thread && TRACE_COUNTED.load(Ordering::Relaxed))) {
        return;
    }
    // A panic's own report allocates while std holds its backtrace lock: capturing there would
    // wait on that lock forever.
    if std::thread::panicking() {
        return;
    }
    let _ = THREAD.try_with(|t| t.capturing.set(true));
    let backtrace = Backtrace::force_capture();
    TRACED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Captured {
            kind,
            size,
            mark: TRACE_MARK.load(Ordering::Relaxed),
            thread,
            backtrace,
        });
    let _ = THREAD.try_with(|t| t.capturing.set(false));
}

/// Labels the requests traced from now on with `mark` (a batch number, a step number), so an
/// [`AllocTrace`] can compare two parts of one run ([`AllocTrace::grown`]). A trace starts at
/// mark 0. Only the thread inside [`trace`] sets the mark: called anywhere else (a test running
/// beside the traced one, a [`SteadyAllocCheck::run`] that is not traced) it does nothing, so it
/// cannot relabel another test's trace.
pub fn set_trace_mark(mark: u64) {
    if THREAD.try_with(|t| t.traced.get()).unwrap_or(false) {
        TRACE_MARK.store(mark, Ordering::Relaxed);
    }
}

/// Runs `f` with a backtrace captured for every request this thread makes, and returns them.
///
/// A diagnostic: a capture walks the stack, so `f` runs many times slower, and the counters stay
/// exact while the live and peak bytes do not (the tracer's own blocks are not counted in, and
/// count out when the trace is dropped). One trace runs at a time in a binary; do not call it
/// from inside `f`.
pub fn trace<R>(f: impl FnOnce() -> R) -> (R, AllocTrace) {
    trace_with(false, f)
}

/// As [`trace`], and the requests of the counted threads ([`count_this_thread`]) are captured
/// too: a frame's drawing thread and encode workers. Every counted thread of the process is
/// captured, so in a binary with several tests that count threads, hold [`exclusive`] around it.
pub fn trace_counted<R>(f: impl FnOnce() -> R) -> (R, AllocTrace) {
    trace_with(true, f)
}

fn trace_with<R>(counted: bool, f: impl FnOnce() -> R) -> (R, AllocTrace) {
    /// Stops the trace, also when `f` unwinds.
    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            let _ = THREAD.try_with(|t| t.traced.set(false));
            TRACE_COUNTED.store(false, Ordering::Relaxed);
        }
    }
    let _session = untracked(|| TRACE_SESSION.lock().unwrap_or_else(PoisonError::into_inner));
    untracked(|| TRACED.lock().unwrap_or_else(PoisonError::into_inner).clear());
    TRACE_MARK.store(0, Ordering::Relaxed);
    let this_thread = THREAD
        .try_with(|t| {
            t.traced.set(true);
            std::ptr::from_ref(t) as usize
        })
        .unwrap_or(0);
    TRACE_COUNTED.store(counted, Ordering::Relaxed);
    let disarm = Disarm;
    let out = f();
    drop(disarm);
    let requests = untracked(|| std::mem::take(&mut *TRACED.lock().unwrap_or_else(PoisonError::into_inner)));
    (
        out,
        AllocTrace {
            requests,
            this_thread,
        },
    )
}

/// The requests of one [`trace`], each with the stack that made it.
pub struct AllocTrace {
    requests: Vec<Captured>,
    this_thread: usize,
}

impl fmt::Debug for AllocTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllocTrace")
            .field("requests", &self.requests.len())
            .finish()
    }
}

/// Requests that share a stack ([`AllocTrace::sites`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocSite {
    /// Requests made from this stack.
    pub requests: u64,
    /// Bytes they asked for (the new size of a `realloc`).
    pub bytes: u64,
    /// How many of them were `realloc`s: a container that grew, where the others are new blocks.
    pub reallocs: u64,
    /// How many of them were made on the thread that called [`trace`]; the rest came from
    /// counted threads.
    pub on_tracing_thread: u64,
    /// The stack, innermost caller of the allocator first: `function (file:line)`.
    pub stack: Vec<String>,
}

/// Frames kept per site: enough to reach the engine function behind a container's growth.
const SITE_FRAMES: usize = 28;
/// Frames of a site when work that moved between threads must cancel out
/// ([`AllocTrace::grown_at`]): the allocating function and its nearest callers.
const NET_SITE_FRAMES: usize = 12;

/// The frames of `backtrace` from the allocator's caller outwards, without the test harness's.
fn stack_of(backtrace: &Backtrace) -> Vec<String> {
    stack_of_text(&backtrace.to_string())
}

/// [`stack_of`] over a backtrace's printed form.
fn stack_of_text(text: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(location) = line.strip_prefix("at ") {
            // Keep the last three path components and the line: enough to find the file.
            let location = location.rsplitn(2, ':').last().unwrap_or(location);
            let short: Vec<&str> = location.rsplit('/').take(3).collect();
            let short: Vec<&str> = short.into_iter().rev().collect();
            if let Some(last) = frames.last_mut() {
                last.push_str(&format!(" ({})", short.join("/")));
            }
        } else if let Some((index, name)) = line.split_once(": ")
            && !index.is_empty()
            && index.bytes().all(|b| b.is_ascii_digit())
        {
            frames.push(name.to_string());
        }
    }
    // The innermost frames are this tracer and the allocator. std's own capture frames are the
    // tracer's too: on Linux `Backtrace` hides them, on macOS it starts at
    // `<std::backtrace::Backtrace>::create` (ADR-7484, the M5 run).
    const ALLOCATOR: [&str; 7] = [
        "alloc_counter::",
        "__rust_alloc",
        "__rust_realloc",
        "__rustc",
        "__rg_",
        "core::alloc::global::GlobalAlloc>::",
        "std::backtrace",
    ];
    // The stack ends where this module called the traced closure (`trace`, or a batch of
    // `SteadyAllocCheck::run`): what is above differs from batch to batch and says nothing about
    // the site. A thread this module did not call (a counted worker) ends at the test harness or
    // runs out.
    const CALLER: [&str; 3] = [
        "alloc_counter::",
        "__rust_begin_short_backtrace",
        "test::run_test",
    ];
    let start = frames
        .iter()
        .position(|f| !frame_is(f, &ALLOCATOR))
        .unwrap_or(frames.len());
    let end = frames[start..]
        .iter()
        .position(|f| frame_is(f, &CALLER))
        .map_or(frames.len(), |offset| start + offset);
    frames.drain(start..end).take(SITE_FRAMES).collect()
}

/// Whether the frame `function (file:line)` of [`stack_of_text`] is one of `markers`' (symbol
/// prefixes and fragments), or this module's by its file.
///
/// A frame is judged by its symbol when the symbol carries a module path: the file and line are
/// those of the innermost code at the frame's address, which is often code inlined into it. When a
/// capture has no inline frames (no split DWARF to read: the `.dwo` paths the binary names are the
/// remapped `/builder/…` ones of ADR-10354, which exist on no host; ADR-10394), the frame of a
/// function that inlined the allocator is named for that function and placed in `alloc_counter.rs`,
/// and it is the site, not the allocator. Only a bare name (an inlined function's short name, as
/// `line-tables-only` debuginfo gives it, without its module path) is judged by its file. The
/// markers are looked for in the generic arguments too: without inline frames, a closure of this
/// module that an iterator adapter inlined is known only by the adapter frame's type arguments.
fn frame_is(frame: &str, markers: &[&str]) -> bool {
    // `stack_of_text` appends the location last, as ` (path:line)`.
    let (symbol, location) = match frame.rsplit_once(" (") {
        Some((symbol, location)) if location.ends_with(')') && location.contains(".rs:") => {
            (symbol, location)
        }
        _ => (frame, ""),
    };
    if markers.iter().any(|marker| symbol.contains(marker)) {
        return true;
    }
    !without_generic_args(symbol).contains("::") && location.contains("alloc_counter.rs:")
}

/// `symbol` without its generic argument lists (`measure::<T, F>` and `trace_with<R, F>` become
/// `measure` and `trace_with`); a qualified path's `<Type as Trait>` stays, its arguments dropped.
fn without_generic_args(symbol: &str) -> String {
    let mut out = String::with_capacity(symbol.len());
    let mut depth = 0usize;
    let mut prev = ' ';
    for c in symbol.chars() {
        // The `>` of a `fn() -> T` argument closes nothing.
        let arrow = prev == '-';
        prev = c;
        if depth > 0 {
            match c {
                '<' => depth += 1,
                '>' if !arrow => depth -= 1,
                _ => {}
            }
            continue;
        }
        let after_name = out
            .chars()
            .next_back()
            .is_some_and(|p| p.is_alphanumeric() || matches!(p, '_' | ':' | '}'));
        if c == '<' && after_name {
            depth = 1;
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches("::").to_string()
}

impl AllocTrace {
    /// Requests traced.
    #[must_use]
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    /// Whether nothing was requested.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// Requests labelled `mark` ([`set_trace_mark`]).
    #[must_use]
    pub fn len_at(&self, mark: u64) -> usize {
        self.requests.iter().filter(|r| r.mark == mark).count()
    }

    /// The kept requests grouped by the innermost `frames` frames of their stack.
    fn sites_where(&self, frames: usize, keep: impl Fn(&Captured) -> bool) -> Vec<AllocSite> {
        let mut sites: Vec<AllocSite> = Vec::new();
        let mut by_stack: std::collections::HashMap<Vec<String>, usize> = std::collections::HashMap::new();
        for request in self.requests.iter().filter(|r| keep(r)) {
            let mut stack = stack_of(&request.backtrace);
            stack.truncate(frames);
            let index = *by_stack.entry(stack.clone()).or_insert_with(|| {
                sites.push(AllocSite {
                    requests: 0,
                    bytes: 0,
                    reallocs: 0,
                    on_tracing_thread: 0,
                    stack,
                });
                sites.len() - 1
            });
            let site = &mut sites[index];
            site.requests += 1;
            site.bytes += request.size as u64;
            site.reallocs += u64::from(request.kind == AllocKind::Realloc);
            site.on_tracing_thread += u64::from(request.thread == self.this_thread);
        }
        sites.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.stack.cmp(&b.stack)));
        sites
    }

    /// The traced requests grouped by stack, the busiest site first.
    #[must_use]
    pub fn sites(&self) -> Vec<AllocSite> {
        self.sites_where(SITE_FRAMES, |_| true)
    }

    /// The requests labelled `mark`, grouped by stack.
    #[must_use]
    pub fn sites_at(&self, mark: u64) -> Vec<AllocSite> {
        self.sites_where(SITE_FRAMES, |r| r.mark == mark)
    }

    /// The sites that made more requests under mark `to` than under mark `from`, with the
    /// difference as their `requests` (and `bytes`, `on_tracing_thread`), the largest first: what
    /// a batch that grew did that the steady batch did not. Sites are whole stacks here, so work
    /// that another thread did under `from` and this one under `to` is listed too (its stack
    /// differs where the threads' callers differ): the view that shows work moving.
    #[must_use]
    pub fn grown(&self, from: u64, to: u64) -> Vec<AllocSite> {
        self.grown_at(from, to, SITE_FRAMES)
    }

    /// As [`Self::grown`], with a site being the innermost `frames` frames of the stack only. A
    /// dozen frames name the function that allocates and its callers, and stop short of the
    /// frames that tell one thread from another, so work that merely moved between threads
    /// cancels out and what is left is what the traced threads did more of in total.
    #[must_use]
    pub fn grown_at(&self, from: u64, to: u64, frames: usize) -> Vec<AllocSite> {
        let before = self.sites_where(frames, |r| r.mark == from);
        more_requests(&before, self.sites_where(frames, |r| r.mark == to))
    }

    /// The `most` busiest sites as text, for an assertion message.
    #[must_use]
    pub fn report(&self, most: usize) -> String {
        format!(
            "{} requests traced\n{}",
            self.requests.len(),
            sites_text(&self.sites(), most)
        )
    }
}

/// The sites of `after` that made more requests than the same stack did in `before`, with the
/// difference as their `requests` (and `bytes`, `reallocs`, `on_tracing_thread`), the largest first.
fn more_requests(before: &[AllocSite], after: Vec<AllocSite>) -> Vec<AllocSite> {
    let mut grown: Vec<AllocSite> = after
        .into_iter()
        .filter_map(|mut site| {
            let base = before.iter().find(|b| b.stack == site.stack);
            let had = base.map_or(0, |b| b.requests);
            if site.requests <= had {
                return None;
            }
            site.requests -= had;
            site.bytes = site.bytes.saturating_sub(base.map_or(0, |b| b.bytes));
            site.reallocs = site.reallocs.saturating_sub(base.map_or(0, |b| b.reallocs));
            site.on_tracing_thread = site
                .on_tracing_thread
                .saturating_sub(base.map_or(0, |b| b.on_tracing_thread));
            Some(site)
        })
        .collect();
    grown.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.stack.cmp(&b.stack)));
    grown
}

/// `sites` as text: one block per site, its count and where its requests were made, then the stack.
#[must_use]
pub fn sites_text(sites: &[AllocSite], most: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for site in sites.iter().take(most) {
        let _ = writeln!(
            out,
            "{} requests ({} on the tracing thread, {} on counted threads; {} reallocs), {} bytes:",
            site.requests,
            site.on_tracing_thread,
            site.requests - site.on_tracing_thread.min(site.requests),
            site.reallocs,
            site.bytes
        );
        for frame in &site.stack {
            let _ = writeln!(out, "    {frame}");
        }
    }
    if sites.len() > most {
        let rest: u64 = sites.iter().skip(most).map(|s| s.requests).sum();
        let _ = writeln!(
            out,
            "... and {} more sites with {rest} requests",
            sites.len() - most
        );
    }
    out
}

/// The steady-frame rule of the renderer's allocation tests, in one place (ADR-2035, ADR-2240,
/// ADR-2299, ADR-5718).
///
/// A frame drawn over and over must settle to the same heap allocations per frame. The rule:
///
/// 1. **Warm.** Draw `warm_frames` frames; then, while the test's own "still warming" condition
///    holds (a probe brick baking on the task pool, say), keep drawing, up to `max_warm_frames`
///    in all. A frame count, never a clock. Still warming at the cap is a failure.
/// 2. **Settle.** Count batches of `batch_frames` frames until two consecutive batches agree
///    within `settle_slack` allocations; the second of the pair is the *steady* batch. A frame
///    that has not settled after `max_settle_batches` batches fails. With `settle_slack` set to
///    `None` there is no settling: the first batch is the steady one (the older first-batch rule).
/// 3. **Measure.** Count `measured_batches` more batches. Each must be at most
///    `steady + growth_slack`. A batch below the steady one is not growth (the probes going idle
///    is a step down).
/// 4. **Lone batch.** Not allowed by default: since the encode workers are counted with the
///    drawing thread (ADR-2299), a lone frame's extra work is gone. With `lone_batch` set, one
///    measured batch over the slack is tolerated when it is not the last one and every other
///    batch is within the slack (the rule before ADR-2299; for a counter that sees one thread of
///    work spread over several).
///
/// Count the frame's own threads: the drawing thread and the renderer's encode workers
/// ([`count_this_thread`] on both, then [`counted_allocs`]), so a segment the drawing thread
/// finishes for a late worker does not move from one batch to another. Draw the frame without a
/// pixel readback and end each draw with a poll of the device to completion, so every frame
/// starts from the same wgpu pools; fill the usage-scope pool between the warm frames and the
/// settling ([`Self::run_warmed`]); and make every submit of the drawing thread wait for the
/// device (`RenderDevice::wait_after_submit_on_this_thread`). The module docs say what each of
/// these keeps out of the count.
///
/// A test whose old private rule was stricter keeps it through the fields: a smaller
/// `growth_slack`, more `measured_batches`, no settling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteadyAllocCheck {
    /// Frames per counted batch.
    pub batch_frames: u32,
    /// Frames always drawn before counting starts.
    pub warm_frames: u32,
    /// The most frames drawn while the test's warm condition still holds.
    pub max_warm_frames: u32,
    /// Two consecutive batches within this many allocations are settled; `None`: the first batch
    /// is the steady one.
    pub settle_slack: Option<u64>,
    /// Batches tried before a frame that never settles fails.
    pub max_settle_batches: usize,
    /// Batches measured against the steady one.
    pub measured_batches: usize,
    /// The most a measured batch may exceed the steady one.
    pub growth_slack: u64,
    /// Whether one lone over-slack batch (not the last) is tolerated (rule step 4).
    pub lone_batch: bool,
}

/// What a [`SteadyAllocCheck`] run counted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SteadyAllocRun {
    /// Frames drawn to warm.
    pub warm_frames: u32,
    /// The settling batches; the last one is the steady batch.
    pub settling: Vec<u64>,
    /// The steady batch.
    pub steady: u64,
    /// The measured batches.
    pub measured: Vec<u64>,
    /// The calling thread's own requests in each settling batch, then in each measured one,
    /// whatever counter the batches were taken from. Beside a counted-thread total it shows the
    /// work that moved: on a loaded machine these jump by a segment while the total stays put.
    /// All zero when [`CountingAlloc`] is not the binary's global allocator.
    pub calling_thread: Vec<u64>,
    /// Every thread's requests in the same batches ([`process_allocs`]). Beside a counted-thread
    /// total it shows whether a thread that is not counted does part of the frame's work: equal
    /// when none does (the test harness's own threads add a few requests now and then).
    pub process: Vec<u64>,
    /// In a traced run ([`TRACE_ENV`], [`SteadyAllocCheck::run_traced`]): every batch that counted
    /// differently from the run's most common batch, with the sites that made more requests in it
    /// and the sites that made fewer, as text. It names an event that stays inside the slack: a
    /// pool of the device that grew once, a request that the machine's load makes come and go.
    /// Empty when the run was not traced or every batch counted the same.
    pub deviations: String,
}

impl fmt::Display for SteadyAllocRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "steady batch {}, measured {:?} (after {} warm frames and settling {:?}; the calling thread alone {:?}, \
             every thread {:?})",
            self.steady, self.measured, self.warm_frames, self.settling, self.calling_thread, self.process
        )?;
        if !self.deviations.is_empty() {
            write!(
                f,
                "\nbatches that differ from the usual one:\n{}",
                self.deviations
            )?;
        }
        Ok(())
    }
}

/// The trace mark of batch `index` of a run (settling batches first, then the measured ones).
/// Mark 0 holds what the rule itself requested between batches.
fn batch_mark(index: usize) -> u64 {
    index as u64 + 1
}

/// The batches of `run` that differ from its most common batch (the earliest such batch stands for
/// it), each with the sites that made more requests in it and the sites that made fewer.
fn deviations_text(traced: &AllocTrace, run: &SteadyAllocRun, listed: usize) -> String {
    use std::fmt::Write as _;
    let batches: Vec<u64> = run.settling.iter().chain(&run.measured).copied().collect();
    let (mut usual_index, mut usual_count) = (0, 0);
    for (i, &batch) in batches.iter().enumerate() {
        let count = batches.iter().filter(|&&other| other == batch).count();
        if count > usual_count {
            (usual_index, usual_count) = (i, count);
        }
    }
    let Some(&usual) = batches.get(usual_index) else {
        return String::new();
    };
    if batches.iter().all(|&batch| batch == usual) {
        return String::new();
    }
    let usual_sites = traced.sites_where(NET_SITE_FRAMES, |r| r.mark == batch_mark(usual_index));
    let total = |sites: &[AllocSite]| sites.iter().map(|site| site.requests).sum::<u64>();
    let mut out = String::new();
    for (i, &batch) in batches.iter().enumerate() {
        if batch == usual {
            continue;
        }
        let these = traced.sites_where(NET_SITE_FRAMES, |r| r.mark == batch_mark(i));
        let more = more_requests(&usual_sites, these.clone());
        let fewer = more_requests(&these, usual_sites.clone());
        let _ = writeln!(
            out,
            "batch {i} counted {batch}, the usual batch (batch {usual_index}) {usual}.\nsites with more requests ({} in \
             all; a site is the innermost {NET_SITE_FRAMES} frames):\n{}sites with fewer ({} in all):\n{}",
            total(&more),
            sites_text(&more, listed),
            total(&fewer),
            sites_text(&fewer, listed)
        );
    }
    out
}

/// Why a [`SteadyAllocCheck`] run failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SteadyAllocError {
    /// The test's warm condition still held after `max_warm_frames` frames.
    StillWarming {
        /// Frames drawn.
        frames: u32,
    },
    /// No two consecutive batches agreed within the settle slack.
    NeverSettled {
        /// Frames drawn to warm.
        warm_frames: u32,
        /// Every batch counted.
        batches: Vec<u64>,
    },
    /// Measured batches exceeded the steady batch by more than the growth slack.
    Grows {
        /// The run up to the failure.
        run: Box<SteadyAllocRun>,
        /// What [`SteadyAllocCheck::check`] said.
        reason: String,
        /// With `REGIA_ALLOC_TRACE=1`: the sites that made more requests in the largest measured
        /// batch than in the steady one ([`AllocTrace::grown`], as text). Empty otherwise.
        sites: String,
    },
}

impl fmt::Display for SteadyAllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StillWarming { frames } => write!(f, "still warming after {frames} frames"),
            Self::NeverSettled { warm_frames, batches } => write!(
                f,
                "the allocations never settle (after {warm_frames} warm frames): {batches:?}"
            ),
            Self::Grows { run, reason, sites } => {
                write!(
                    f,
                    "the allocations grow: {reason}; settling {:?}, measured {:?}",
                    run.settling, run.measured
                )?;
                if sites.is_empty() {
                    write!(f, " (rerun with {TRACE_ENV}=1 to see which sites grew)")
                } else {
                    write!(
                        f,
                        "\nwhat the largest batch did that the steady one did not:\n{sites}"
                    )
                }
            }
        }
    }
}

impl std::error::Error for SteadyAllocError {}

/// Set to `1`, [`SteadyAllocCheck::run`] traces the settling and measured batches
/// ([`trace_counted`]) and a failure names the 12 sites that grew most. A larger number is the
/// number of sites to list. Many times slower: a diagnostic for a failing run, not for the gate.
pub const TRACE_ENV: &str = "REGIA_ALLOC_TRACE";

/// The sites a failure lists when [`TRACE_ENV`] asks for a trace; `None` when it does not.
fn trace_asked() -> Option<usize> {
    let value = std::env::var(TRACE_ENV).ok()?;
    match value.trim().parse::<usize>() {
        Ok(0) | Err(_) => None,
        Ok(1) => Some(12),
        Ok(sites) => Some(sites),
    }
}

impl SteadyAllocCheck {
    /// The renderer's rule (ADR-2299) for batches of `batch_frames` frames: 40 warm frames (at most
    /// 2000 while the test's condition holds), settled within one allocation per frame in at most
    /// 12 batches, 6 measured batches within 4 allocations per frame, no lone batch.
    pub const fn frames(batch_frames: u32) -> Self {
        let batch = batch_frames as u64;
        Self {
            batch_frames,
            warm_frames: 40,
            max_warm_frames: 2000,
            settle_slack: Some(batch),
            max_settle_batches: 12,
            measured_batches: 6,
            growth_slack: 4 * batch,
            lone_batch: false,
        }
    }

    /// Rule step 3 (and 4) on `measured` batches against the `steady` one. The error names the
    /// batches over the slack.
    pub fn check(&self, steady: u64, measured: &[u64]) -> Result<(), String> {
        let limit = steady.saturating_add(self.growth_slack);
        let over: Vec<usize> = measured
            .iter()
            .enumerate()
            .filter(|&(_, &batch)| batch > limit)
            .map(|(i, _)| i)
            .collect();
        if over.is_empty() {
            return Ok(());
        }
        if self.lone_batch && over.len() == 1 && over[0] + 1 < measured.len() {
            return Ok(());
        }
        Err(format!(
            "batches {over:?} exceed the settled batch ({steady}) by more than {}",
            self.growth_slack
        ))
    }

    /// Rule step 2 on batches counted by `batch`: the batches and the steady one, `None` when
    /// they never settle.
    pub fn settle(&self, mut batch: impl FnMut() -> u64) -> (Vec<u64>, Option<u64>) {
        let mut batches: Vec<u64> = Vec::new();
        let Some(slack) = self.settle_slack else {
            let first = batch();
            return (vec![first], Some(first));
        };
        while batches.len() < self.max_settle_batches {
            let next = batch();
            let settled = batches.last().is_some_and(|&prev| prev.abs_diff(next) <= slack);
            batches.push(next);
            if settled {
                return (batches, Some(next));
            }
        }
        (batches, None)
    }

    /// The whole rule. `draw` draws one frame of `state`, `warming` is the test's extra warm
    /// condition (`|_| false` for none), and `count` reads the counter the batches are taken
    /// from (such as [`counted_allocs`] or [`thread_allocs`]).
    ///
    /// Calls [`quiet_tracing`] first. With [`TRACE_ENV`] set it is [`Self::run_traced`]: the
    /// batches are traced, a [`SteadyAllocError::Grows`] carries the sites that grew, and the run
    /// lists the batches that differ from the usual one ([`SteadyAllocRun::deviations`]).
    pub fn run<S>(
        &self,
        state: &mut S,
        count: impl Fn() -> u64,
        draw: impl FnMut(&mut S),
        warming: impl FnMut(&S) -> bool,
    ) -> Result<SteadyAllocRun, SteadyAllocError> {
        self.run_listing(state, count, draw, warming, |_| (), trace_asked())
    }

    /// [`Self::run`], with `warmed` called once between the warm frames and the settling batches,
    /// when everything the frame creates exists. The place to bring a pool whose growth depends
    /// on timing to its full size: a frame test passes
    /// `|renderer| { device_pools::fill_usage_scope_pool(device, renderer.encode_threads() + 1); }`
    /// ([`crate::device_pools`]). The settling batches after it absorb what the call disturbed.
    pub fn run_warmed<S>(
        &self,
        state: &mut S,
        count: impl Fn() -> u64,
        draw: impl FnMut(&mut S),
        warming: impl FnMut(&S) -> bool,
        warmed: impl FnOnce(&mut S),
    ) -> Result<SteadyAllocRun, SteadyAllocError> {
        self.run_listing(state, count, draw, warming, warmed, trace_asked())
    }

    /// [`Self::run`] with the settling and measured batches traced ([`trace_counted`]), whatever
    /// [`TRACE_ENV`] says, listing at most `sites` sites per comparison. Many times slower: for a
    /// test of the tracing itself, and for a run a person reads.
    pub fn run_traced<S>(
        &self,
        state: &mut S,
        count: impl Fn() -> u64,
        draw: impl FnMut(&mut S),
        warming: impl FnMut(&S) -> bool,
        sites: usize,
    ) -> Result<SteadyAllocRun, SteadyAllocError> {
        self.run_listing(state, count, draw, warming, |_| (), Some(sites))
    }

    fn run_listing<S>(
        &self,
        state: &mut S,
        count: impl Fn() -> u64,
        mut draw: impl FnMut(&mut S),
        mut warming: impl FnMut(&S) -> bool,
        warmed: impl FnOnce(&mut S),
        listed: Option<usize>,
    ) -> Result<SteadyAllocRun, SteadyAllocError> {
        quiet_tracing();
        let mut warm = 0;
        while warm < self.warm_frames || (warming(state) && warm < self.max_warm_frames) {
            draw(state);
            warm += 1;
        }
        if warming(state) {
            return Err(SteadyAllocError::StillWarming { frames: warm });
        }
        warmed(state);
        // Batch `i` (settling batches first, then the measured ones) is traced under mark `i + 1`
        // (`batch_mark`). Mark 0 is what the rule itself requests between batches (its lists of
        // batches), which belongs to no batch.
        let mut mark = 0;
        let mut calling_thread = Vec::with_capacity(self.max_settle_batches + self.measured_batches);
        let mut process = Vec::with_capacity(self.max_settle_batches + self.measured_batches);
        let mut batch = || {
            mark += 1;
            set_trace_mark(mark);
            let (own, all) = (thread_allocs(), process_allocs());
            let before = count();
            for _ in 0..self.batch_frames {
                draw(state);
            }
            let counted = count().wrapping_sub(before);
            set_trace_mark(0);
            calling_thread.push(thread_allocs().wrapping_sub(own));
            process.push(process_allocs().wrapping_sub(all));
            counted
        };
        let mut batches = || {
            let (settling, steady) = self.settle(&mut batch);
            let measured: Vec<u64> = match steady {
                Some(_) => (0..self.measured_batches).map(|_| batch()).collect(),
                None => Vec::new(),
            };
            (settling, steady, measured)
        };
        let ((settling, steady, measured), traced) = if listed.is_some() {
            let (out, traced) = trace_counted(batches);
            (out, Some(traced))
        } else {
            (batches(), None)
        };
        let Some(steady) = steady else {
            return Err(SteadyAllocError::NeverSettled {
                warm_frames: warm,
                batches: settling,
            });
        };
        let mut run = SteadyAllocRun {
            warm_frames: warm,
            settling,
            steady,
            measured,
            calling_thread,
            process,
            deviations: String::new(),
        };
        if let Some(traced) = &traced {
            run.deviations = deviations_text(traced, &run, listed.unwrap_or(12));
        }
        match self.check(steady, &run.measured) {
            Ok(()) => Ok(run),
            Err(reason) => {
                let sites = traced.map_or_else(String::new, |traced| {
                    let steady_mark = batch_mark(run.settling.len().saturating_sub(1));
                    let largest = (0..run.measured.len())
                        .max_by_key(|&i| run.measured[i])
                        .unwrap_or(0);
                    let largest_mark = batch_mark(run.settling.len() + largest);
                    let listed = listed.unwrap_or(12);
                    // First what the traced threads did more of in total, then by whole stack,
                    // where work that only moved from one thread to another shows as well.
                    let net = traced.grown_at(steady_mark, largest_mark, NET_SITE_FRAMES);
                    let moved = traced.grown(steady_mark, largest_mark);
                    format!(
                        "more in total on the traced threads ({} requests; a site is the innermost \
                         {NET_SITE_FRAMES} frames):\n{}by whole stack ({} requests; work that moved between \
                         threads is listed here too):\n{}",
                        net.iter().map(|site| site.requests).sum::<u64>(),
                        sites_text(&net, listed),
                        moved.iter().map(|site| site.requests).sum::<u64>(),
                        sites_text(&moved, listed)
                    )
                });
                Err(SteadyAllocError::Grows {
                    run: Box::new(run),
                    reason,
                    sites,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    /// The tests that call the allocator run one at a time: the process-wide counters (the
    /// largest request, say) see every thread's requests.
    static PROCESS_WIDE: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_WIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The printed form of a capture as macOS gives it: std's own frames first (Linux hides them),
    /// then the tracer and the allocator, the site, and the caller of the traced closure.
    const MACOS_CAPTURE: &str = "   0: std::backtrace_rs::backtrace::libunwind::trace
             at /rustc/library/std/src/../../backtrace/src/backtrace/libunwind.rs:117:9
   1: <std::backtrace::Backtrace>::create
             at /rustc/library/std/src/backtrace.rs:331:13
   2: regia_test_utils::alloc_counter::trace_request
             at ./crates/regia_test_utils/src/alloc_counter.rs:563:21
   3: <regia_test_utils::alloc_counter::CountingAlloc as core::alloc::global::GlobalAlloc>::alloc
             at ./crates/regia_test_utils/src/alloc_counter.rs:140:9
   4: __rustc::__rust_alloc
             at ./crates/regia_test_utils/tests/alloc_counter_rules.rs:17:1
   5: alloc_counter_rules::a_marked_site
             at ./crates/regia_test_utils/tests/alloc_counter_rules.rs:272:24
   6: regia_test_utils::alloc_counter::trace_with
             at ./crates/regia_test_utils/src/alloc_counter.rs:625:15
   7: alloc_counter_rules::a_trace_names_the_function_that_allocated
             at ./crates/regia_test_utils/tests/alloc_counter_rules.rs:284:45
";

    /// The site starts at the function that asked, also where the capture lists std's own frames
    /// (the M5 run's stack was `<std::backtrace::Backtrace>::create` alone).
    #[test]
    fn a_site_skips_std_s_capture_frames() {
        let stack = stack_of_text(MACOS_CAPTURE);
        assert_eq!(
            stack,
            ["alloc_counter_rules::a_marked_site (regia_test_utils/tests/alloc_counter_rules.rs:272)"],
            "{stack:?}"
        );
    }

    /// A Linux capture with no inline frames, as the v0.7.0 rc2 gate host printed it: the test
    /// profile inlines the allocator into the function that asked, and the `.dwo` files that would
    /// list the inlined frames sit at remapped `/builder/…` paths (ADR-10354), so that function's
    /// frame carries the allocator's file and line.
    const REMAPPED_CAPTURE: &str = "   0: regia_test_utils::alloc_counter::trace_request
             at /builder/lanes/src/crates/regia_test_utils/src/alloc_counter.rs:575:21
   1: alloc_counter_rules::a_marked_site
             at /builder/lanes/src/crates/regia_test_utils/src/alloc_counter.rs:191:9
   2: regia_test_utils::alloc_counter::measure::<(alloc::boxed::Box<[u8; 48]>, regia_test_utils::alloc_counter::AllocTrace), alloc_counter_rules::a_trace_names_the_function_that_allocated::{closure#0}>
             at /builder/me/.rustup/toolchains/1.99.0/lib/rustlib/src/rust/library/core/src/ops/function.rs:250:5
   3: <alloc_counter_rules::a_trace_names_the_function_that_allocated::{closure#0} as core::ops::function::FnOnce<()>>::call_once
             at /builder/lanes/src/crates/regia_test_utils/tests/alloc_counter_rules.rs:290:36
   4: test::__rust_begin_short_backtrace::<core::result::Result<(), alloc::string::String>, fn() -> core::result::Result<(), alloc::string::String>>
             at /rustc/library/test/src/lib.rs:733:18
";

    /// The site is the function the frame is named for, also where its line is the inlined
    /// allocator's (ADR-10394): a file inside this module does not make a named frame the tracer's.
    #[test]
    fn a_named_frame_with_the_allocator_inlined_is_the_site() {
        let stack = stack_of_text(REMAPPED_CAPTURE);
        assert_eq!(
            stack,
            ["alloc_counter_rules::a_marked_site (regia_test_utils/src/alloc_counter.rs:191)"],
            "{stack:?}"
        );
    }

    /// A capture with inline frames under `line-tables-only` debuginfo: inlined frames carry bare
    /// names, so this module's are known by their file, and the site still starts past them.
    const INLINED_CAPTURE: &str = "   0: regia_test_utils::alloc_counter::trace_request
             at ./crates/regia_test_utils/src/alloc_counter.rs:575:21
   1: alloc
             at ./crates/regia_test_utils/src/alloc_counter.rs:191:9
   2: __rust_alloc
             at ./crates/regia_test_utils/tests/alloc_counter_rules.rs:17:1
   3: exchange_malloc
             at /rustc/library/alloc/src/alloc.rs:255:22
   4: alloc_counter_rules::a_marked_site
             at ./crates/regia_test_utils/tests/alloc_counter_rules.rs:279:24
   5: trace_with<alloc::boxed::Box<[u8; 48]>, fn() -> alloc::boxed::Box<[u8; 48]>>
             at ./crates/regia_test_utils/src/alloc_counter.rs:625:15
   6: alloc_counter_rules::a_trace_names_the_function_that_allocated
             at ./crates/regia_test_utils/tests/alloc_counter_rules.rs:290:45
";

    #[test]
    fn bare_inlined_frames_of_this_module_are_known_by_their_file() {
        let stack = stack_of_text(INLINED_CAPTURE);
        assert_eq!(
            stack,
            [
                "exchange_malloc (alloc/src/alloc.rs:255)",
                "alloc_counter_rules::a_marked_site (regia_test_utils/tests/alloc_counter_rules.rs:279)"
            ],
            "{stack:?}"
        );
    }

    // The counters are driven by calling the allocator's methods directly: this binary's global
    // allocator is the system one, so the only requests counted are the ones made here. That is
    // also what Miri checks (docs/06 § 14).

    #[test]
    fn a_request_is_one_allocation_and_its_bytes() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let layout = Layout::from_size_align(24, 8).expect("layout");
        let (_, stats) = measure(|| {
            // SAFETY: a non-zero-sized layout; the block is freed below with the same layout.
            let p = unsafe { a.alloc(layout) };
            assert!(!p.is_null());
            // SAFETY: `p` came from `a.alloc(layout)`.
            unsafe { p.write_bytes(0xAB, 24) };
            // SAFETY: `p` came from `a.alloc(layout)` and is not used again.
            unsafe { a.dealloc(p, layout) };
        });
        assert_eq!(stats, AllocStats { allocs: 1, bytes: 24 });
    }

    /// The module's own bookkeeping (a static mutex's first lock on macOS) counts nothing, and
    /// the requests after it count again.
    #[test]
    fn a_request_inside_untracked_is_not_counted() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let ((), stats) = measure(|| {
            untracked(|| a.request(AllocKind::Alloc, 32, 32));
            a.request(AllocKind::Alloc, 24, 24);
        });
        assert_eq!(stats, AllocStats { allocs: 1, bytes: 24 });
    }

    #[test]
    fn realloc_counts_once_with_its_growth() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let layout = Layout::from_size_align(16, 8).expect("layout");
        let ((), stats) = measure(|| {
            // SAFETY: a non-zero-sized layout.
            let p = unsafe { a.alloc(layout) };
            assert!(!p.is_null());
            // SAFETY: `p` holds 16 bytes.
            unsafe { p.write_bytes(7, 16) };
            // SAFETY: `p` came from `a` with `layout`; 64 is a valid size for alignment 8.
            let q = unsafe { a.realloc(p, layout, 64) };
            assert!(!q.is_null());
            // SAFETY: `realloc` keeps the first 16 bytes.
            assert_eq!(unsafe { q.add(15).read() }, 7);
            let grown = Layout::from_size_align(64, 8).expect("layout");
            // SAFETY: `q` came from `a` with `grown`; 8 is a valid size for alignment 8.
            let r = unsafe { a.realloc(q, grown, 8) };
            assert!(!r.is_null());
            // SAFETY: `r` came from `a` with an 8-byte layout of alignment 8.
            unsafe { a.dealloc(r, Layout::from_size_align(8, 8).expect("layout")) };
        });
        // alloc 16, realloc to 64 (+48), realloc to 8 (shrinks: +0).
        assert_eq!(stats, AllocStats { allocs: 3, bytes: 64 });
    }

    #[test]
    fn alloc_zeroed_counts_once_and_zeroes() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let layout = Layout::from_size_align(32, 16).expect("layout");
        let ((), stats) = measure(|| {
            // SAFETY: a non-zero-sized layout.
            let p = unsafe { a.alloc_zeroed(layout) };
            assert!(!p.is_null());
            assert_eq!(p.align_offset(16), 0);
            // SAFETY: `p` holds 32 zeroed bytes.
            let bytes = unsafe { std::slice::from_raw_parts(p, 32) };
            assert!(bytes.iter().all(|&b| b == 0));
            // SAFETY: `p` came from `a` with `layout`.
            unsafe { a.dealloc(p, layout) };
        });
        assert_eq!(stats, AllocStats { allocs: 1, bytes: 32 });
    }

    #[test]
    fn every_alignment_round_trips() {
        let _guard = lock();
        let a = CountingAlloc::new();
        for shift in 0..12 {
            let align = 1usize << shift;
            let layout = Layout::from_size_align(3 * align, align).expect("layout");
            // SAFETY: a non-zero-sized layout.
            let p = unsafe { a.alloc(layout) };
            assert!(!p.is_null());
            assert_eq!(p.align_offset(align), 0, "align {align}");
            // SAFETY: `p` holds `3 * align` bytes.
            unsafe { p.write_bytes(1, 3 * align) };
            // SAFETY: `p` came from `a` with `layout`; the new size keeps the alignment valid.
            let q = unsafe { a.realloc(p, layout, 5 * align) };
            assert!(!q.is_null());
            assert_eq!(q.align_offset(align), 0, "align {align} after realloc");
            // SAFETY: `q` came from `a` with the grown layout.
            unsafe { a.dealloc(q, Layout::from_size_align(5 * align, align).expect("layout")) };
        }
    }

    #[test]
    fn live_and_peak_bytes_follow_blocks() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let small = Layout::from_size_align(100, 4).expect("layout");
        let big = Layout::from_size_align(1000, 4).expect("layout");
        let start = thread_live_bytes();
        let ((), peak) = peak_bytes(|| {
            // SAFETY: non-zero-sized layouts; both blocks are freed below.
            let p = unsafe { a.alloc(small) };
            // SAFETY: as above.
            let q = unsafe { a.alloc(big) };
            assert_eq!(thread_live_bytes() - start, 1100);
            // SAFETY: `q` came from `a` with `big`.
            unsafe { a.dealloc(q, big) };
            let ((), inner) = peak_bytes(|| {
                // SAFETY: a non-zero-sized layout; freed right away.
                let r = unsafe { a.alloc(small) };
                // SAFETY: `r` came from `a` with `small`.
                unsafe { a.dealloc(r, small) };
            });
            assert_eq!(inner, 100);
            // SAFETY: `p` came from `a` with `small`.
            unsafe { a.dealloc(p, small) };
        });
        // The inner scope's 100 bytes on top of 100 never beat the outer 1100.
        assert_eq!(peak, 1100);
        assert_eq!(thread_live_bytes(), start);
        // Freeing what the scope did not allocate is no growth.
        let held = Layout::from_size_align(64, 8).expect("layout");
        // SAFETY: a non-zero-sized layout.
        let h = unsafe { a.alloc(held) };
        let ((), peak) = peak_bytes(|| {
            // SAFETY: `h` came from `a` with `held`.
            unsafe { a.dealloc(h, held) };
        });
        assert_eq!(peak, 0);
    }

    #[test]
    fn only_counted_threads_count_in_counted_stats() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let layout = Layout::from_size_align(8, 8).expect("layout");
        let alloc_free = || {
            // SAFETY: a non-zero-sized layout; freed right away with the same layout.
            let p = unsafe { a.alloc(layout) };
            // SAFETY: `p` came from `a` with `layout`.
            unsafe { a.dealloc(p, layout) };
        };
        let before = counted_stats();
        std::thread::scope(|s| {
            s.spawn(alloc_free).join().expect("uncounted thread");
            s.spawn(|| {
                count_this_thread();
                assert!(is_this_thread_counted());
                alloc_free();
                alloc_free();
                stop_counting_this_thread();
                alloc_free();
            })
            .join()
            .expect("counted thread");
        });
        let counted = counted_stats().since(before);
        assert_eq!(counted, AllocStats { allocs: 2, bytes: 16 });
        // A thread's own counter sees its own requests only.
        let ((), mine) = measure(|| std::thread::scope(|s| s.spawn(alloc_free).join().expect("thread")));
        assert_eq!(mine.allocs, 0);
    }

    #[test]
    fn process_counters_see_every_thread() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let layout = Layout::from_size_align(40, 8).expect("layout");
        let before = process_stats();
        let ((), peak) = process_peak_bytes(|| {
            std::thread::scope(|s| {
                s.spawn(|| {
                    // SAFETY: a non-zero-sized layout; freed right away.
                    let p = unsafe { a.alloc(layout) };
                    // SAFETY: `p` came from `a` with `layout`.
                    unsafe { a.dealloc(p, layout) };
                })
                .join()
                .expect("thread");
            });
        });
        assert_eq!(process_stats().since(before), AllocStats { allocs: 1, bytes: 40 });
        assert_eq!(peak, 40);
    }

    #[test]
    fn the_largest_request_counts_refused_ones() {
        let _guard = lock();
        let a = CountingAlloc::new();
        let ((), largest) = largest_request_in(|| {
            let layout = Layout::from_size_align(48, 8).expect("layout");
            // SAFETY: a non-zero-sized layout; freed right away.
            let p = unsafe { a.alloc(layout) };
            // SAFETY: `p` came from `a` with `layout`.
            unsafe { a.dealloc(p, layout) };
        });
        assert_eq!(largest, 48);
        // A request no allocator can grant still counts (Miri reports the refusal as an error, so
        // the size is one a real allocator refuses).
        #[cfg(not(miri))]
        {
            let ((), largest) = largest_request_in(|| {
                let huge = Layout::from_size_align(isize::MAX as usize - 4095, 4096).expect("layout");
                // SAFETY: a non-zero-sized layout; a null result is checked.
                let p = unsafe { a.alloc(huge) };
                assert!(p.is_null());
            });
            assert_eq!(largest, isize::MAX as usize - 4095);
        }
    }

    static HOOK_SIZES: Mutex<Vec<(AllocKind, usize)>> = Mutex::new(Vec::new());
    static HOOK_ON: AtomicBool = AtomicBool::new(false);

    struct Recorder;
    impl AllocHook for Recorder {
        fn on_request(&self, event: AllocEvent) {
            if !HOOK_ON.load(Ordering::Relaxed) {
                return;
            }
            // Re-entry: this push may allocate, and must not call the hook again.
            HOOK_SIZES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.kind, event.size));
        }
    }

    #[test]
    fn a_hook_sees_each_request_once_and_does_not_re_enter() {
        let _guard = lock();
        let a = CountingAlloc::with_hook(Recorder);
        let layout = Layout::from_size_align(16, 8).expect("layout");
        HOOK_ON.store(true, Ordering::Relaxed);
        // SAFETY: a non-zero-sized layout.
        let p = unsafe { a.alloc(layout) };
        // SAFETY: `p` came from `a` with `layout`.
        let q = unsafe { a.realloc(p, layout, 32) };
        // SAFETY: a non-zero-sized layout.
        let z = unsafe { a.alloc_zeroed(layout) };
        HOOK_ON.store(false, Ordering::Relaxed);
        // SAFETY: each came from `a` with the layout given.
        unsafe {
            a.dealloc(q, Layout::from_size_align(32, 8).expect("layout"));
            a.dealloc(z, layout);
        }
        let seen = std::mem::take(
            &mut *HOOK_SIZES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        assert_eq!(
            seen,
            vec![
                (AllocKind::Alloc, 16),
                (AllocKind::Realloc, 32),
                (AllocKind::Zeroed, 16)
            ]
        );
    }

    const RULE: SteadyAllocCheck = SteadyAllocCheck::frames(8);

    #[test]
    fn the_growth_check_tells_noise_from_growth() {
        // Steady, with the noise measured on real drivers.
        assert!(RULE.check(1888, &[1888, 1890, 1887, 1888, 1889, 1888]).is_ok());
        // Exactly at the slack passes, one over fails.
        assert!(RULE.check(1000, &[1032]).is_ok());
        assert_eq!(
            RULE.check(1000, &[1033]),
            Err("batches [0] exceed the settled batch (1000) by more than 32".to_owned())
        );
        // A lone over-slack batch fails by default (ADR-2299) ...
        assert!(RULE.check(1712, &[1710, 1712, 1760, 1712, 1713, 1712]).is_err());
        // ... and passes with `lone_batch` unless it is the last one or not alone.
        let lone = SteadyAllocCheck {
            lone_batch: true,
            ..RULE
        };
        assert!(lone.check(1712, &[1710, 1712, 1760, 1712, 1713, 1712]).is_ok());
        assert!(lone.check(1712, &[1712, 1712, 1712, 1712, 1712, 1760]).is_err());
        assert!(lone.check(1712, &[1760, 1712, 1760, 1712, 1712, 1712]).is_err());
        // A step that stays up, creep and per-frame rebuilds fail.
        assert!(RULE.check(1712, &[1712, 1712, 1712, 1760, 1760, 1760]).is_err());
        let creeping: Vec<u64> = (1..=6).map(|k| 1707 + 8 * k).collect();
        assert_eq!(
            RULE.check(1707, &creeping),
            Err("batches [4, 5] exceed the settled batch (1707) by more than 32".to_owned())
        );
        // A step down is not growth.
        assert!(RULE.check(1712, &[1712, 1712, 1672, 1672, 1672, 1672]).is_ok());
        // No overflow at the top of the range.
        assert!(RULE.check(u64::MAX - 1, &[u64::MAX]).is_ok());
    }

    #[test]
    fn settling_needs_two_batches_that_agree() {
        let mut feed = [2100u64, 1950, 1890, 1888, 1500].into_iter();
        let (batches, steady) = RULE.settle(|| feed.next().unwrap_or(0));
        assert_eq!(batches, vec![2100, 1950, 1890, 1888]);
        assert_eq!(steady, Some(1888));
        let mut wild = (0u64..).map(|k| 1000 + 100 * k);
        let (batches, steady) = RULE.settle(|| wild.next().unwrap_or(0));
        assert_eq!(batches.len(), RULE.max_settle_batches);
        assert_eq!(steady, None);
        let first = SteadyAllocCheck {
            settle_slack: None,
            ..RULE
        };
        let mut feed = [5u64, 6].into_iter();
        assert_eq!(first.settle(|| feed.next().unwrap_or(0)), (vec![5], Some(5)));
    }

    /// A pretend frame: `extra` allocations on frames listed in `spikes`, a constant otherwise.
    struct Frames {
        drawn: u64,
        counter: Cell<u64>,
        per_frame: u64,
        warming_until: u64,
        spikes: Vec<u64>,
    }

    impl Frames {
        fn draw(&mut self) {
            let extra = if self.drawn < self.warming_until { 5 } else { 0 };
            let spike = if self.spikes.contains(&self.drawn) { 48 } else { 0 };
            self.counter
                .set(self.counter.get() + self.per_frame + extra + spike);
            self.drawn += 1;
        }
    }

    #[test]
    fn a_run_warms_settles_and_measures() {
        let mut frames = Frames {
            drawn: 0,
            counter: Cell::new(0),
            per_frame: 200,
            warming_until: 90,
            spikes: Vec::new(),
        };
        let counter = std::rc::Rc::new(Cell::new(0u64));
        let shared = counter.clone();
        let run = RULE
            .run(
                &mut frames,
                || shared.get(),
                |f| {
                    f.draw();
                    counter.set(f.counter.get());
                },
                |f| f.drawn < f.warming_until,
            )
            .expect("steady");
        assert_eq!(run.warm_frames, 90);
        assert_eq!(run.steady, 1600);
        assert_eq!(run.measured, vec![1600; 6]);

        // A spike in a measured batch fails it.
        let mut frames = Frames {
            drawn: 0,
            counter: Cell::new(0),
            per_frame: 200,
            warming_until: 0,
            spikes: vec![40 + 16 + 20],
        };
        let counter = std::rc::Rc::new(Cell::new(0u64));
        let shared = counter.clone();
        let err = RULE
            .run(
                &mut frames,
                || shared.get(),
                |f| {
                    f.draw();
                    counter.set(f.counter.get());
                },
                |_| false,
            )
            .expect_err("a spike");
        let SteadyAllocError::Grows { run, reason, sites } = err else {
            panic!("{err}");
        };
        assert_eq!(run.measured[2], 1648);
        assert_eq!(
            reason,
            "batches [2] exceed the settled batch (1600) by more than 32"
        );
        // Not traced (the variable is unset in a test run): the message says how to ask.
        if trace_asked().is_none() {
            assert!(sites.is_empty());
        }

        // Still warming at the cap.
        let capped = SteadyAllocCheck {
            max_warm_frames: 50,
            ..RULE
        };
        let mut frames = Frames {
            drawn: 0,
            counter: Cell::new(0),
            per_frame: 1,
            warming_until: 1000,
            spikes: Vec::new(),
        };
        let err = capped
            .run(&mut frames, || 0, Frames::draw, |f| f.drawn < f.warming_until)
            .expect_err("still warming");
        assert_eq!(err, SteadyAllocError::StillWarming { frames: 50 });
    }
}
