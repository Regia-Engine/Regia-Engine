// SPDX-License-Identifier: MPL-2.0
//! Task pools (`docs/specs/10-core.md` § 1.4): three global `bevy_tasks` pools sized from the CPU count.
//!
//! | Pool | Use | Free helper |
//! |---|---|---|
//! | [`ComputePool`] | work that must finish this frame (parallel systems, culling, skinning) | [`scope`], [`par_chunks`], [`par_chunks_mut`] |
//! | [`AsyncComputePool`] | CPU work that may span frames (navmesh bakes, decompression) | [`spawn`] |
//! | [`IoPool`] | blocking-light IO futures (file and network reads) | [`spawn_io`] |
//!
//! Call [`init`] once at startup; any pool accessor initialises the pools with default options if
//! [`init`] has not run, so library code never has to care.
//!
//! # Profiling
//!
//! A system's `tracing` span covers only the thread the system runs on. [`par_chunks`] and
//! [`par_chunks_mut`] open an `info` span named `par_chunk` in every task, a child of the span that
//! was current where they were called, with that span's name in the `of` field and the chunk index
//! in `index`; the frame profiler and Tracy then see the work on the pool threads (ADR-1378).
//! [`spawn`] and [`spawn_io`] run their future inside an `async_task` or `io_task` span (entered on
//! every poll), parented and named the same way. Inside [`scope`] each spawned task opens its own
//! span.
//!
//! # Locks
//!
//! Never hold a lock across [`scope`], [`par_chunks`] or [`par_chunks_mut`] that another pool
//! task may take (ADR-303). The waiting thread does not just wait: it runs other queued
//! compute tasks, including unrelated ones such as `Send` systems of the frame schedule. One
//! of them taking the held lock waits on its own thread, and the scope never ends. Copy what the
//! fan-out needs (or clone an [`std::sync::Arc`] snapshot) under the lock and release it first.

use std::sync::OnceLock;

pub use bevy_tasks::{Scope, Task, TaskPool};

/// Runs a future to completion on the current thread.
///
/// It only parks the thread: it does not run the task pools. A future running on a pool thread
/// that calls `block_on(spawn(..))` (or `spawn_io`) on its own pool waits for a poll that only
/// that thread could do, and hangs when the pool has one thread (fewer than 8 hardware
/// threads) or every worker is in such a wait. Await instead of blocking inside pool tasks.
pub use bevy_tasks::block_on;

use bevy_tasks::{
    AsyncComputeTaskPool, ComputeTaskPool, IoTaskPool, ParallelSlice, ParallelSliceMut, TaskPoolBuilder,
};

/// Thread-count options for [`init`]. `None` means "derive from the CPU count".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPoolOptions {
    /// Compute pool threads.
    pub compute_threads: Option<usize>,
    /// Async compute pool threads.
    pub async_compute_threads: Option<usize>,
    /// IO pool threads.
    pub io_threads: Option<usize>,
    /// Android only: the compute pool is capped at this many threads (the device's big cores), which
    /// limits thermal throttling. Default 4.
    pub android_big_cores: usize,
}

impl Default for TaskPoolOptions {
    fn default() -> Self {
        Self {
            compute_threads: None,
            async_compute_threads: None,
            io_threads: None,
            android_big_cores: 4,
        }
    }
}

/// Threads per pool, as returned by [`init`] and [`sizes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskPoolSizes {
    /// Compute pool threads.
    pub compute: usize,
    /// Async compute pool threads.
    pub async_compute: usize,
    /// IO pool threads.
    pub io: usize,
}

impl TaskPoolOptions {
    /// Pool sizes for a machine with `cpus` hardware threads on the current platform.
    ///
    /// Formula: compute = max(1, cpus − 2) (leaving the main and render threads a core each),
    /// async compute = max(1, cpus / 4), IO = max(1, cpus / 4); on Android compute is further capped at
    /// [`TaskPoolOptions::android_big_cores`]. Explicit counts override the formula (minimum 1).
    pub fn sizes_for(&self, cpus: usize) -> TaskPoolSizes {
        self.sizes_for_platform(cpus, cfg!(target_os = "android"))
    }

    fn sizes_for_platform(&self, cpus: usize, android: bool) -> TaskPoolSizes {
        let cpus = cpus.max(1);
        let mut compute = cpus.saturating_sub(2).max(1);
        if android {
            compute = compute.min(self.android_big_cores.max(1));
        }
        let quarter = (cpus / 4).max(1);
        TaskPoolSizes {
            compute: self.compute_threads.unwrap_or(compute).max(1),
            async_compute: self.async_compute_threads.unwrap_or(quarter).max(1),
            io: self.io_threads.unwrap_or(quarter).max(1),
        }
    }
}

static SIZES: OnceLock<TaskPoolSizes> = OnceLock::new();

fn build(threads: usize, name: &str) -> TaskPool {
    TaskPoolBuilder::new()
        .num_threads(threads)
        .thread_name(name.to_owned())
        .build()
}

/// Creates the three global pools. Only the first call has an effect; later calls log at `debug` and
/// return the sizes chosen by the first. Pools another crate created first are kept as they are.
pub fn init(options: TaskPoolOptions) -> TaskPoolSizes {
    let mut first = false;
    let sizes = *SIZES.get_or_init(|| {
        first = true;
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let wanted = options.sizes_for(cpus);
        let actual = TaskPoolSizes {
            compute: ComputeTaskPool::get_or_init(|| build(wanted.compute, "regia-compute")).thread_num(),
            async_compute: AsyncComputeTaskPool::get_or_init(|| build(wanted.async_compute, "regia-async"))
                .thread_num(),
            io: IoTaskPool::get_or_init(|| build(wanted.io, "regia-io")).thread_num(),
        };
        tracing::info!(
            target: "regia_core",
            "task pools: {} compute, {} async compute, {} io threads ({cpus} hardware threads)",
            actual.compute,
            actual.async_compute,
            actual.io
        );
        actual
    });
    if !first {
        tracing::debug!(target: "regia_core", "tasks::init called again; keeping the existing pools");
    }
    sizes
}

/// The pool sizes, or `None` before [`init`].
pub fn sizes() -> Option<TaskPoolSizes> {
    SIZES.get().copied()
}

fn ensure_init() {
    if SIZES.get().is_none() {
        init(TaskPoolOptions::default());
    }
}

/// The compute pool: work that must complete within the current frame.
#[derive(Debug, Clone, Copy)]
pub struct ComputePool;

impl ComputePool {
    /// The global pool (initialised with defaults on first use if [`init`] has not run).
    pub fn get() -> &'static TaskPool {
        ensure_init();
        ComputeTaskPool::get_or_init(|| build(1, "regia-compute"))
    }
}

/// The async compute pool: CPU work that may take several frames.
#[derive(Debug, Clone, Copy)]
pub struct AsyncComputePool;

impl AsyncComputePool {
    /// The global pool (initialised with defaults on first use if [`init`] has not run).
    pub fn get() -> &'static TaskPool {
        ensure_init();
        AsyncComputeTaskPool::get_or_init(|| build(1, "regia-async"))
    }
}

/// The IO pool: futures that mostly wait on IO.
#[derive(Debug, Clone, Copy)]
pub struct IoPool;

impl IoPool {
    /// The global pool (initialised with defaults on first use if [`init`] has not run).
    pub fn get() -> &'static TaskPool {
        ensure_init();
        IoTaskPool::get_or_init(|| build(1, "regia-io"))
    }
}

/// Spawns background CPU work on the [`AsyncComputePool`]. Dropping the returned [`Task`] cancels it;
/// call `detach()` to let it run unobserved.
pub fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
    let caller = tracing::Span::current();
    let of = caller.metadata().map_or("", |m| m.name());
    let span = tracing::info_span!(parent: &caller, "async_task", of);
    AsyncComputePool::get().spawn(tracing::Instrument::instrument(future, span))
}

/// Spawns an IO future on the [`IoPool`].
pub fn spawn_io<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
    let caller = tracing::Span::current();
    let of = caller.metadata().map_or("", |m| m.name());
    let span = tracing::info_span!(parent: &caller, "io_task", of);
    IoPool::get().spawn(tracing::Instrument::instrument(future, span))
}

/// Runs `f` with a scope on the [`ComputePool`] whose tasks may borrow from the caller; returns every
/// task's result once all have finished. Results are in spawn order only for tasks spawned from the
/// `f` closure itself; a task that calls `Scope::spawn` from inside another task races other workers,
/// so such children must return `(index, value)` and be sorted by the caller.
pub fn scope<'env, F, T>(f: F) -> Vec<T>
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env, T>),
    T: Send + 'static,
{
    ComputePool::get().scope(f)
}

/// Calls `f(chunk_index, chunk)` for each `chunk_size`-element chunk of `slice` in parallel on the
/// [`ComputePool`] and returns the results in chunk order. A `chunk_size` of 0 is treated as 1.
/// Each chunk runs inside a `par_chunk` span (see the module docs).
pub fn par_chunks<T, R, F>(slice: &[T], chunk_size: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send + 'static,
    F: Fn(usize, &[T]) -> R + Send + Sync,
{
    let caller = tracing::Span::current();
    slice.par_chunk_map(ComputePool::get(), chunk_size.max(1), |index, chunk| {
        let _span = chunk_span(&caller, index).entered();
        f(index, chunk)
    })
}

/// Mutable variant of [`par_chunks`].
pub fn par_chunks_mut<T, R, F>(mut slice: &mut [T], chunk_size: usize, f: F) -> Vec<R>
where
    T: Send,
    R: Send + 'static,
    F: Fn(usize, &mut [T]) -> R + Send + Sync,
{
    let caller = tracing::Span::current();
    slice.par_chunk_map_mut(ComputePool::get(), chunk_size.max(1), |index, chunk| {
        let _span = chunk_span(&caller, index).entered();
        f(index, chunk)
    })
}

/// The span one chunk of [`par_chunks`] runs in: a child of `caller`, which named it.
fn chunk_span(caller: &tracing::Span, index: usize) -> tracing::Span {
    let of = caller.metadata().map_or("", |m| m.name());
    tracing::info_span!(parent: caller, "par_chunk", of, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn size_formula() {
        let o = TaskPoolOptions::default();
        let s = |cpus, android| o.sizes_for_platform(cpus, android);
        assert_eq!(
            s(16, false),
            TaskPoolSizes {
                compute: 14,
                async_compute: 4,
                io: 4
            }
        );
        assert_eq!(
            s(8, false),
            TaskPoolSizes {
                compute: 6,
                async_compute: 2,
                io: 2
            }
        );
        assert_eq!(
            s(2, false),
            TaskPoolSizes {
                compute: 1,
                async_compute: 1,
                io: 1
            }
        );
        assert_eq!(
            s(0, false),
            TaskPoolSizes {
                compute: 1,
                async_compute: 1,
                io: 1
            }
        );
        // Pixel-class phone: 8 cores, compute capped at the 4 big cores.
        assert_eq!(
            s(8, true),
            TaskPoolSizes {
                compute: 4,
                async_compute: 2,
                io: 2
            }
        );
        let custom = TaskPoolOptions {
            compute_threads: Some(3),
            io_threads: Some(0),
            android_big_cores: 2,
            ..TaskPoolOptions::default()
        };
        assert_eq!(
            custom.sizes_for_platform(8, true),
            TaskPoolSizes {
                compute: 3,
                async_compute: 2,
                io: 1
            }
        );
        assert_eq!(custom.sizes_for_platform(16, true).compute, 3);
        let capped = TaskPoolOptions {
            android_big_cores: 2,
            ..TaskPoolOptions::default()
        };
        assert_eq!(capped.sizes_for_platform(16, true).compute, 2);
        assert_eq!(capped.sizes_for_platform(16, false).compute, 14);
    }

    #[test]
    fn init_is_idempotent() {
        let first = init(TaskPoolOptions::default());
        let second = init(TaskPoolOptions {
            compute_threads: Some(97),
            ..TaskPoolOptions::default()
        });
        assert_eq!(first, second);
        assert_eq!(sizes(), Some(first));
        assert_eq!(ComputePool::get().thread_num(), first.compute);
        assert_eq!(AsyncComputePool::get().thread_num(), first.async_compute);
        assert_eq!(IoPool::get().thread_num(), first.io);
    }

    #[test]
    fn par_chunks_covers_every_element_once() {
        for (len, chunk) in [
            (0usize, 7usize),
            (1, 1),
            (1000, 7),
            (1000, 1000),
            (1000, 4096),
            (33, 0),
        ] {
            let hits: Vec<AtomicU32> = (0..len).map(|_| AtomicU32::new(0)).collect();
            let indices: Vec<usize> = (0..len).collect();
            let sums = par_chunks(&indices, chunk, |chunk_index, items| {
                for &i in items {
                    hits[i].fetch_add(1, Ordering::Relaxed);
                }
                (chunk_index, items.iter().sum::<usize>())
            });
            assert!(
                hits.iter().all(|h| h.load(Ordering::Relaxed) == 1),
                "len {len} chunk {chunk}"
            );
            assert_eq!(
                sums.iter().map(|&(_, s)| s).sum::<usize>(),
                indices.iter().sum::<usize>()
            );
            assert!(
                sums.iter().enumerate().all(|(i, &(ci, _))| i == ci),
                "results in chunk order"
            );
        }

        let mut values = vec![1u32; 500];
        let counts = par_chunks_mut(&mut values, 64, |_, chunk| {
            for v in chunk.iter_mut() {
                *v *= 3;
            }
            chunk.len()
        });
        assert_eq!(counts.iter().sum::<usize>(), 500);
        assert!(values.iter().all(|&v| v == 3));
    }

    #[test]
    fn scope_and_spawn_return_results() {
        let data: Vec<u64> = (0..10).collect();
        let mut squares = scope(|s| {
            for &x in &data {
                s.spawn(async move { x * x });
            }
        });
        squares.sort_unstable();
        assert_eq!(squares, data.iter().map(|x| x * x).collect::<Vec<_>>());
        assert_eq!(block_on(spawn(async { 2 + 2 })), 4);
        assert_eq!(block_on(spawn_io(async { "io" })), "io");
    }
}
