// SPDX-License-Identifier: MPL-2.0
//! Pipelined command-buffer finishing: one frame, several encoders, one submit
//! (`specs/13` § 4, "records nodes in parallel into separate encoders").
//!
//! wgpu records lazily. Pass calls only append commands; `CommandEncoder::finish`
//! validates them and replays them into the backend's command buffers. On DO27's chase
//! that replay was about half of `render_into`'s CPU time. The frame cuts its encoder at
//! a few pass boundaries ([`FinishWorkers::cut`]). Each completed segment is finished on
//! a worker thread while the next one is recorded, and [`FinishWorkers::join`] returns the buffers in
//! recording order for a single `queue.submit`.
//!
//! The GPU receives the same commands in the same order. wgpu tracks resource state per
//! command buffer and inserts the transitions between the buffers of one submit, so the
//! frame is unchanged. With no worker threads ([`FinishWorkers::set_threads`] with 0) the
//! frame keeps one encoder on desktop. Android's pass-budget scope still cuts and finishes
//! synchronously, so disabling workers does not bypass its pool-growth bound.
//!
//! A segment's buffer comes back through a reply slot the renderer keeps from frame to frame
//! (`Reply`), not a channel per segment: a steady frame makes no allocation to hand a buffer
//! back, and waiting for a worker allocates nothing either (a blocking channel receive
//! registered a waker entry, one allocation now and then, ADR-2299).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use crate::RenderError;

pub(crate) mod pass_cuts;

/// Worker threads a renderer starts by default (capped by the CPU count minus one).
pub(crate) const DEFAULT_THREADS: usize = 2;
/// Upper bound on worker threads.
pub(crate) const MAX_THREADS: usize = 8;

/// A finished buffer, or the panic payload of a `finish` that panicked (a wgpu
/// validation error under the default handler), re-raised on the recording thread.
type Finished = std::thread::Result<wgpu::CommandBuffer>;

/// What a [`Reply`] holds.
enum ReplyState {
    /// Nothing yet (or the recording thread took the buffer).
    Empty,
    /// The segment's buffer, or the panic payload of its `finish`.
    Done(Finished),
    /// The job was dropped without running: no buffer will come.
    Abandoned,
}

/// Where one segment's buffer is left for the recording thread. [`FinishWorkers`] keeps these
/// from frame to frame (segment `i` of every frame uses reply `i`), so handing a buffer back
/// allocates nothing; a `Mutex` and `Condvar` wait allocates nothing either (ADR-2299).
struct Reply {
    state: Mutex<ReplyState>,
    ready: Condvar,
}

impl Reply {
    fn new() -> Self {
        Self {
            state: Mutex::new(ReplyState::Empty),
            ready: Condvar::new(),
        }
    }

    fn put(&self, state: ReplyState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
        self.ready.notify_all();
    }

    /// Blocks until the segment is finished or abandoned (`None`), and leaves the reply empty.
    fn take(&self) -> Option<Finished> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            match std::mem::replace(&mut *state, ReplyState::Empty) {
                ReplyState::Done(finished) => return Some(finished),
                ReplyState::Abandoned => return None,
                ReplyState::Empty => {
                    state = self.ready.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

/// Vendor buffers occupy segment indices without worker replies; fill every gap before indexing.
fn reply_slot(replies: &mut Vec<Arc<Reply>>, index: usize) -> Arc<Reply> {
    while replies.len() <= index {
        replies.push(Arc::new(Reply::new()));
    }
    Arc::clone(&replies[index])
}

struct Job {
    /// `None` once [`Self::run`] took it.
    encoder: Option<wgpu::CommandEncoder>,
    reply: Arc<Reply>,
    /// [`Shared::unfinished`].
    unfinished: Arc<AtomicUsize>,
}

impl Job {
    fn run(mut self) {
        // On a worker thread no system span is open: without its own span this time is invisible
        // to the frame profiler and Tracy (ADR-1378).
        let _span = tracing::info_span!("finish_segment").entered();
        let Some(encoder) = self.encoder.take() else {
            return;
        };
        let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || encoder.finish()));
        self.unfinished.fetch_sub(1, Ordering::AcqRel);
        // A dropped frame waits for this in `Segments::drop`; nobody else is waiting then.
        self.reply.put(ReplyState::Done(finished));
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // Never run (a queue dropped with jobs in it): the waiting thread gets an error rather
        // than waiting forever, as a closed channel did.
        if self.encoder.take().is_some() {
            self.unfinished.fetch_sub(1, Ordering::AcqRel);
            self.reply.put(ReplyState::Abandoned);
        }
    }
}

/// The job queue the workers and [`FinishWorkers::join`] take segments from.
#[derive(Default)]
struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    closed: AtomicBool,
    /// Segments queued whose encoder is not finished yet (tests).
    unfinished: Arc<AtomicUsize>,
}

impl Shared {
    fn pop(&self) -> Option<Job> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Blocks until a job arrives and returns it inside an open driver call of `gpu`; `None`
    /// once the owner is gone or the exit asked `gpu` to stop (ADR-287).
    ///
    /// The call opens under the queue lock, before the job leaves the queue, so a worker never
    /// holds a job (an encoder, which holds a device reference) outside a marked section, and a
    /// stopped worker takes no job: the recording thread finishes whatever is still queued
    /// ([`FinishWorkers::join`], the drop of `Segments`).
    fn wait<'g>(&self, gpu: &'g regia_platform::GpuWorker) -> Option<(Job, regia_platform::DriverCall<'g>)> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if !queue.is_empty() {
                let call = gpu.try_driver_call()?;
                if let Some(job) = queue.pop_front() {
                    return Some((job, call));
                }
            }
            if self.closed.load(Ordering::Acquire) || gpu.stop_requested() {
                return None;
            }
            queue = self.ready.wait(queue).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Wakes every waiting worker. Under the lock, so a worker between its check and its wait
    /// cannot miss it.
    fn wake(&self) {
        let _queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        self.ready.notify_all();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake();
    }
}

/// Persistent threads that finish command encoders. They start on the first cut and
/// exit when the owner is dropped.
#[derive(Default)]
pub(crate) struct FinishWorkers {
    /// Requested thread count; `None` reads `REGIA_ENCODE_THREADS` or the default.
    wanted: Option<usize>,
    /// The queue, while at least one worker runs.
    shared: Option<Arc<Shared>>,
    threads: usize,
    started: bool,
    /// One reply per segment of a frame, reused every frame (grown on the frames that cut
    /// more segments than any before).
    replies: Vec<Arc<Reply>>,
    /// Test hook: called first on each worker thread it starts
    /// (`FrameRenderer::set_encode_worker_start_hook`).
    start_hook: Option<fn()>,
}

impl Drop for FinishWorkers {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            shared.close();
        }
    }
}

impl FinishWorkers {
    /// Uses `threads` workers from the next frame on (0: finish on the recording thread,
    /// one encoder per frame unless a pass-budget scope is active).
    pub(crate) fn set_threads(&mut self, threads: usize) {
        if let Some(shared) = self.shared.take() {
            shared.close();
        }
        self.wanted = Some(threads.min(MAX_THREADS));
        self.threads = 0;
        self.started = false;
    }

    /// Worker threads in use (after the first frame), or requested.
    pub(crate) fn threads(&self) -> usize {
        if self.started {
            self.threads
        } else {
            self.resolved()
        }
    }

    fn resolved(&self) -> usize {
        let wanted = self.wanted.unwrap_or_else(|| {
            regia_core::brand::env_var("REGIA_ENCODE_THREADS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(DEFAULT_THREADS)
        });
        let spare = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .saturating_sub(1);
        wanted.min(spare).min(MAX_THREADS)
    }

    /// Test hook: `hook` runs first on each worker started from now on (the next frame's, after
    /// [`Self::set_threads`]), so a test can tell the workers' allocations from other threads'.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn set_start_hook(&mut self, hook: fn()) {
        self.start_hook = Some(hook);
    }

    fn start(&mut self) {
        self.started = true;
        let shared = Arc::new(Shared::default());
        let hook = self.start_hook;
        for index in 0..self.resolved() {
            let queue = Arc::clone(&shared);
            // Registered (ADR-287), counted only while it holds a segment: an idle pool thread
            // holds no GPU object and never holds the exit. The exit's stop wakes it, and from
            // then on it takes no segment (`Shared::wait`).
            let stop = Arc::downgrade(&shared);
            let spawned = regia_platform::GpuWorkerBuilder::new(format!("regia-encode-{index}"))
                .driver_calls_marked()
                .on_stop(move || {
                    if let Some(shared) = stop.upgrade() {
                        shared.wake();
                    }
                })
                .spawn(move |gpu| {
                    if let Some(hook) = hook {
                        hook();
                    }
                    while let Some((job, _call)) = queue.wait(gpu) {
                        job.run();
                    }
                });
            match spawned {
                Ok(_) => self.threads += 1,
                Err(err) => {
                    tracing::warn!(target: "regia_render", "encode worker did not start: {err}");
                    break;
                }
            }
        }
        if self.threads > 0 {
            self.shared = Some(shared);
        }
    }

    /// Ends the segment recorded in `encoder`: a fresh encoder takes its place and the
    /// recorded one is queued for a worker. Does nothing without workers, so the frame
    /// keeps a single encoder unless a pass-budget scope is active.
    pub(crate) fn cut(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        segments: &mut Segments,
    ) {
        pass_cuts::flush(encoder, segments);
        if !self.started {
            self.start();
        }
        let Some(shared) = self.shared.as_ref() else {
            return;
        };
        let fresh = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame segment"),
        });
        let done = std::mem::replace(encoder, fresh);
        pass_cuts::boundary(encoder);
        // Segment `i` of every frame uses reply `i`: the last frame's join (or the drop of its
        // segments) took every reply it used, so it is empty and no worker writes it.
        let index = segments.pending.len();
        // Already-finished vendor buffers occupy segment indices without reply slots.
        // A cut after those buffers can skip several indices on the first vendor frame.
        let reply = reply_slot(&mut self.replies, index);
        shared.unfinished.fetch_add(1, Ordering::AcqRel);
        shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(Job {
                encoder: Some(done),
                reply: Arc::clone(&reply),
                unfinished: Arc::clone(&shared.unfinished),
            });
        shared.ready.notify_one();
        segments.pending.push(Pending::Worker(reply));
        if segments.shared.is_none() {
            segments.shared = Some(Arc::clone(shared));
        }
    }

    /// Finishes `last` on this thread, then any segment no worker has started yet (a
    /// descheduled worker never makes the frame slower than finishing on one thread),
    /// waits for the rest, and returns every command buffer of the frame in recording
    /// order.
    ///
    /// Every segment is waited for before it returns, also on an error or a panic, so no worker
    /// still holds a reply the next frame uses.
    ///
    /// # Errors
    ///
    /// A segment was dropped without being finished.
    ///
    /// # Panics
    ///
    /// Re-raises the panic of a segment's `finish` (wgpu's default validation-error
    /// handler panics), as the single-encoder frame would have on this thread.
    pub(crate) fn join(
        &self,
        mut segments: Segments,
        last: wgpu::CommandEncoder,
    ) -> Result<Vec<wgpu::CommandBuffer>, RenderError> {
        // Includes the wait for the workers' segments.
        let _span = tracing::info_span!("finish_join").entered();
        // Its own span, so a timeline tells the last segment's finish from the wait for the
        // workers (frame pipelining S0, ADR-2074).
        let tail = {
            let _tail = tracing::info_span!("finish_tail").entered();
            last.finish()
        };
        if let Some(shared) = self.shared.as_ref() {
            while let Some(job) = shared.pop() {
                job.run();
            }
        }
        let pending = std::mem::take(&mut segments.pending);
        let mut buffers = Vec::with_capacity(pending.len() + 1);
        let mut panic = None;
        let mut abandoned = false;
        for entry in pending {
            match entry.take() {
                Some(Ok(buffer)) => buffers.push(buffer),
                Some(Err(payload)) => {
                    panic.get_or_insert(payload);
                }
                None => abandoned = true,
            }
        }
        if let Some(payload) = panic {
            std::panic::resume_unwind(payload);
        }
        if abandoned {
            return Err(RenderError::Encode(
                "an encode worker exited before finishing its command buffer".into(),
            ));
        }
        buffers.push(tail);
        Ok(buffers)
    }
}

/// The segments of one frame cut so far, in recording order.
///
/// A frame that ends without [`FinishWorkers::join`] (an error return mid-frame, a cancelled
/// warm, an unwind) drops it: the drop finishes every segment no worker has started on this
/// thread and waits for the ones in progress, so no encoder of the frame (each holds a device
/// reference) is finished or dropped on an encode worker after the frame has returned. A
/// cancelled warm job therefore leaves nothing on the device behind it when its worker exits
/// (ADR-170: no device reference may outlive the warm queue's shutdown).
#[derive(Default)]
pub(crate) struct Segments {
    pending: Vec<Pending>,
    /// The queue the segments went to.
    shared: Option<Arc<Shared>>,
}

/// A segment of the frame, in recording order.
enum Pending {
    /// Finished by a worker (or by the recording thread, if no worker started it).
    Worker(Arc<Reply>),
    /// Finished elsewhere ([`Segments::push_finished`]).
    Ready(wgpu::CommandBuffer),
}

impl Pending {
    /// The segment's buffer once it is finished; `None` when it was dropped unfinished.
    fn take(self) -> Option<Finished> {
        match self {
            Self::Worker(reply) => reply.take(),
            Self::Ready(buffer) => Some(Ok(buffer)),
        }
    }
}

impl Drop for Segments {
    fn drop(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if let Some(shared) = self.shared.as_ref() {
            // Only this frame's segments are queued (one recording thread per renderer).
            while let Some(job) = shared.pop() {
                job.run();
            }
        }
        // The rest are on a worker now: wait for each (a panic payload is dropped here).
        for entry in self.pending.drain(..) {
            let _ = entry.take();
        }
    }
}

impl Segments {
    /// Queues a command buffer finished elsewhere after the segments queued so far (a vendor
    /// upscaler's own buffer, ADR-2002), so `FinishWorkers::join` returns it in recording order.
    pub(crate) fn push_finished(&mut self, buffer: wgpu::CommandBuffer) {
        self.pending.push(Pending::Ready(buffer));
    }
}

#[cfg(test)]
impl Segments {
    /// Segments handed off so far.
    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn ready_vendor_segments_can_skip_worker_reply_indices() {
        let mut replies = Vec::new();
        // Worker cut 0, two ready vendor buffers, worker cut 3, another two ready buffers,
        // worker cut 6. The old one-slot growth panicked on the second cut.
        let first = reply_slot(&mut replies, 0);
        let middle = reply_slot(&mut replies, 3);
        let last = reply_slot(&mut replies, 6);
        assert_eq!(replies.len(), 7);
        assert!(!Arc::ptr_eq(&first, &middle));
        assert!(!Arc::ptr_eq(&middle, &last));
        // A later frame reuses the same replies, including a previously skipped index.
        let gap = reply_slot(&mut replies, 1);
        assert!(Arc::ptr_eq(&first, &reply_slot(&mut replies, 0)));
        assert!(Arc::ptr_eq(&middle, &reply_slot(&mut replies, 3)));
        assert!(Arc::ptr_eq(&last, &reply_slot(&mut replies, 6)));
        assert!(!Arc::ptr_eq(&first, &gap));
        assert_eq!(replies.len(), 7);
    }

    #[test]
    fn vendor_buffers_before_a_worker_cut_preserve_all_segments() {
        let device = crate::RenderDevice::headless().expect("regression requires Vulkan");
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(2);
        for _ in 0..2 {
            let mut segments = Segments::default();
            for _ in 0..3 {
                segments.push_finished(
                    d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default())
                        .finish(),
                );
            }
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            workers.cut(d, &mut encoder, &mut segments);
            assert!(workers.threads() > 0, "regression requires an encode worker");
            let buffers = workers
                .join(segments, encoder)
                .expect("join vendor and worker buffers");
            assert_eq!(buffers.len(), 5);
            device.queue.submit(buffers);
            device.wait_idle().expect("complete both frames");
        }
    }

    #[test]
    fn no_workers_keeps_one_encoder() {
        let mut workers = FinishWorkers::default();
        workers.set_threads(0);
        assert_eq!(workers.threads(), 0);
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let mut segments = Segments::default();
        workers.cut(&device.device, &mut encoder, &mut segments);
        assert_eq!(segments.len(), 0);
        let buffers = workers.join(segments, encoder).expect("join");
        assert_eq!(buffers.len(), 1);
    }

    #[test]
    fn segments_come_back_in_recording_order() {
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(2);
        // Four segments: segment `i` copies marker `i` into slot `i` and into the shared
        // slot 4; the tail copies marker 0 into slot 2. Only in recording order does the
        // shared slot end as marker 3 and slot 2 as marker 0.
        let make = |size: u64, usage, fill: u8| {
            let buf = d.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size,
                usage,
                mapped_at_creation: true,
            });
            buf.slice(..)
                .get_mapped_range_mut()
                .expect("mapped at creation")
                .copy_from_slice(&vec![fill; size as usize]);
            buf.unmap();
            buf
        };
        let markers: Vec<wgpu::Buffer> = (0..4u8)
            .map(|i| make(4, wgpu::BufferUsages::COPY_SRC, i + 10))
            .collect();
        let dst = make(20, wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST, 0);
        let read = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 20,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let mut segments = Segments::default();
        for (i, marker) in markers.iter().enumerate() {
            encoder.copy_buffer_to_buffer(marker, 0, &dst, (i as u64) * 4, 4);
            encoder.copy_buffer_to_buffer(marker, 0, &dst, 16, 4);
            workers.cut(d, &mut encoder, &mut segments);
        }
        encoder.copy_buffer_to_buffer(&markers[0], 0, &dst, 8, 4);
        encoder.copy_buffer_to_buffer(&dst, 0, &read, 0, 20);
        let expected_segments = workers.threads().min(1) * 4;
        assert_eq!(segments.len(), expected_segments);
        let buffers = workers.join(segments, encoder).expect("join");
        assert_eq!(buffers.len(), expected_segments + 1);
        device.queue.submit(buffers);
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = d.poll(wgpu::PollType::wait_indefinitely());
        let bytes = read.slice(..).get_mapped_range().expect("mapped").to_vec();
        let slots: Vec<u8> = bytes.chunks(4).map(|slot| slot[0]).collect();
        assert_eq!(slots, [10, 11, 10, 13, 13]);
    }

    /// ADR-287 (grok27 diff-core #1): an encode worker holds a segment only inside a driver call,
    /// and once the exit has asked it to stop it takes none (it would enter the driver after the
    /// exit's wait saw it idle); the segment stays queued for the recording thread.
    #[test]
    fn encode_workers_hold_segments_in_driver_calls_and_take_none_after_the_stop() {
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let queue_one = |shared: &Shared| {
            let reply = Arc::new(Reply::new());
            shared.unfinished.fetch_add(1, Ordering::AcqRel);
            shared
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_back(Job {
                    encoder: Some(
                        device
                            .device
                            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None }),
                    ),
                    reply: Arc::clone(&reply),
                    unfinished: Arc::clone(&shared.unfinished),
                });
            shared.ready.notify_one();
            reply
        };
        let registry = regia_platform::GpuWorkers::new();
        let shared = Arc::new(Shared::default());
        let (go, wait_go) = mpsc::channel::<()>();
        let (took, saw_take) = mpsc::channel::<()>();
        let queue = Arc::clone(&shared);
        let handle = regia_platform::GpuWorkerBuilder::new("fake-encode")
            .driver_calls_marked()
            .spawn_in(&registry, move |gpu| {
                // The first segment: held until the test has looked at the registry.
                if let Some((job, _call)) = queue.wait(gpu) {
                    let _ = took.send(());
                    let _ = wait_go.recv();
                    job.run();
                }
                // The second is queued after the stop.
                let _ = wait_go.recv();
                queue.wait(gpu).is_some()
            })
            .expect("spawn");
        let first = queue_one(&shared);
        saw_take.recv().expect("the worker took the first segment");
        let held = registry.outstanding();
        assert_eq!(held.len(), 1, "a held segment holds the exit");
        assert!(held[0].in_driver, "inside a driver call");
        go.send(()).expect("release");
        assert!(matches!(first.take(), Some(Ok(_))));
        registry.signal_stop();
        let second = queue_one(&shared);
        go.send(()).expect("release");
        assert!(!handle.join().expect("join"), "a stopped worker took a segment");
        let left = shared
            .pop()
            .expect("the segment stays queued for the recording thread");
        left.run();
        assert!(matches!(second.take(), Some(Ok(_))));
        assert!(registry.wait(std::time::Duration::ZERO).is_empty());
    }

    /// A frame that returns early (a cancelled warm, an error) drops its cut segments without
    /// `join`: the drop must leave no encoder unfinished on an encode worker, or a device
    /// reference outlives the frame (and a cancelled warm job's worker).
    #[test]
    fn dropped_segments_finish_before_the_frame_returns() {
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(2);
        let src = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 1 << 16,
            usage: wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let dst = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 1 << 16,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        for _ in 0..20 {
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            let mut segments = Segments::default();
            // Many segments with some recorded work each: the workers are still finishing
            // them when the frame gives up.
            for _ in 0..64 {
                for _ in 0..64 {
                    encoder.copy_buffer_to_buffer(&src, 0, &dst, 0, 1 << 16);
                }
                workers.cut(d, &mut encoder, &mut segments);
            }
            let Some(shared) = workers.shared.clone() else {
                return;
            };
            drop(segments);
            assert_eq!(
                shared.unfinished.load(Ordering::Acquire),
                0,
                "a segment was still being finished on an encode worker after the frame dropped it"
            );
            assert!(shared.pop().is_none(), "a segment was left queued");
            drop(encoder);
        }
    }

    /// ADR-2299: segment `i` of every frame hands its buffer back through the same reply, so a
    /// steady frame allocates nothing to return its segments (the channel per segment did).
    #[test]
    fn segment_replies_are_reused_frame_after_frame() {
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(2);
        let mut first: Vec<*const Reply> = Vec::new();
        for frame in 0..6 {
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            let mut segments = Segments::default();
            for _ in 0..4 {
                workers.cut(d, &mut encoder, &mut segments);
            }
            let cut = segments.len();
            let buffers = workers.join(segments, encoder).expect("join");
            assert_eq!(buffers.len(), cut + 1);
            let now: Vec<*const Reply> = workers.replies.iter().map(Arc::as_ptr).collect();
            if frame == 0 {
                first = now;
            } else {
                assert_eq!(now, first, "frame {frame} made new replies");
            }
            // Every reply is empty between frames (a worker may still be dropping its handle).
            for reply in &workers.replies {
                assert!(matches!(
                    *reply.state.lock().unwrap_or_else(PoisonError::into_inner),
                    ReplyState::Empty
                ));
            }
        }
        assert_eq!(first.len(), workers.threads().min(1) * 4);
    }

    /// A job dropped without running (its queue went away) tells the waiting thread so: `take`
    /// returns `None`, as a closed channel's receive returned an error, instead of blocking.
    #[test]
    fn a_job_dropped_unrun_is_abandoned() {
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let reply = Arc::new(Reply::new());
        let unfinished = Arc::new(AtomicUsize::new(1));
        let job = Job {
            encoder: Some(
                device
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None }),
            ),
            reply: Arc::clone(&reply),
            unfinished: Arc::clone(&unfinished),
        };
        drop(job);
        assert!(reply.take().is_none());
        assert_eq!(unfinished.load(Ordering::Acquire), 0);
        // A buffer handed back after that is taken normally (the reply is reusable).
        reply.put(ReplyState::Abandoned);
        assert!(reply.take().is_none());
    }

    /// The test hook runs on each encode worker before it takes a segment (the steady-frame
    /// allocation tests count the workers' allocations with it, ADR-2299).
    #[test]
    fn the_start_hook_runs_on_each_worker_before_its_first_segment() {
        static HOOKED: AtomicUsize = AtomicUsize::new(0);
        fn hook() {
            HOOKED.fetch_add(1, Ordering::AcqRel);
        }
        let Ok(device) = crate::RenderDevice::headless() else {
            return;
        };
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(1);
        workers.set_start_hook(hook);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let mut segments = Segments::default();
        workers.cut(d, &mut encoder, &mut segments);
        if workers.threads() == 0 {
            return;
        }
        // Wait for the worker to finish the segment (nobody else takes it from the queue).
        let Some(Pending::Worker(reply)) = segments.pending.pop() else {
            panic!("the cut queued no segment");
        };
        assert!(matches!(reply.take(), Some(Ok(_))));
        assert!(
            HOOKED.load(Ordering::Acquire) >= 1,
            "the worker ran a segment before the hook"
        );
        drop(encoder);
    }
}
