// SPDX-License-Identifier: MPL-2.0
//! What the present path of frame generation owns (ADR-2067): the two scene images, the copy onto
//! the present target and the worker that presents at the pacer's times.
//!
//! The scene renders into [`SceneImages`], never the swap chain, so the frame after a present does
//! not need a swap-chain image until it is finished. That is what lets the real image of a tick
//! wait for its time on the [`PresentWorker`] while the main thread already renders the next
//! frame: a wait on the main thread would stretch every frame by half an interval, and the display
//! would gain nothing over the plain frame rate (ADR-2067 replaces that part of ADR-514 § 4).

use crate::frame_work::counted::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use regia_app::prelude::*;

use super::PresentGaps;
use super::passes::FrameGenPasses;
use crate::RenderError;

/// A present is never held longer than this past the call, whatever the pacer asked. A bad clock
/// or a stale target must not hang the window.
pub const MAX_PRESENT_WAIT_NS: u64 = 250_000_000;

/// How long before the target the worker stops sleeping and spins. Sleep overshoots by up to a
/// scheduler tick; a present late by that much is what pacing exists to avoid.
const SPIN_NS: u64 = 1_000_000;

/// The nanoseconds to wait for a target time, capped by [`MAX_PRESENT_WAIT_NS`].
#[must_use]
pub fn wait_ns(now_ns: u64, at_ns: u64) -> u64 {
    at_ns.saturating_sub(now_ns).min(MAX_PRESENT_WAIT_NS)
}

/// Waits until `epoch + at_ns`, at most [`MAX_PRESENT_WAIT_NS`]; false when the worker was
/// cancelled, in which case the caller drops the payload without presenting. The sleep is
/// sliced so a cancellation lands within about a scheduler tick instead of the whole wait.
fn wait_until_or_cancel(epoch: Instant, at_ns: u64, cancel: &AtomicBool) -> bool {
    let elapsed = |epoch: Instant| u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let start = elapsed(epoch);
    let deadline = start.saturating_add(wait_ns(start, at_ns));
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let now = elapsed(epoch);
        if now >= deadline {
            return true;
        }
        let left = deadline - now;
        if left > SPIN_NS {
            std::thread::sleep(Duration::from_nanos((left - SPIN_NS).min(SPIN_NS)));
        } else {
            std::hint::spin_loop();
        }
    }
}

struct Job<T> {
    payload: T,
    at_ns: u64,
}

/// A thread that runs `present` on each submitted payload at its time, in submit order.
///
/// Generic so a test can drive it without a window; the renderer's payload is a swap-chain image.
pub struct PresentWorker<T: Send + 'static> {
    sender: Option<SyncSender<Job<T>>>,
    /// Jobs submitted and not yet presented, and whether the thread has ended.
    pending: Arc<(Mutex<Pending>, Condvar)>,
    gaps: Arc<Mutex<(Option<Instant>, PresentGaps)>>,
    /// Set by [`Self::cancel`]: the worker drops what it holds without presenting and ends.
    cancelled: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Test-only stops (`RaceHooks`); none outside tests.
    hooks: Hooks,
}

/// The present thread's queue count.
#[derive(Default)]
struct Pending {
    jobs: u32,
    /// The thread ended (its body returned or a present panicked): nothing more is presented.
    ended: bool,
}

/// The present thread's end of the queue. On return or unwind it marks the thread ended and wakes
/// [`PresentWorker::wait_idle`]: a present that panics must not leave the frame path waiting for a
/// count nothing will lower (audit-v035-conc M1). The receiver goes in the same step, under the
/// pending lock, so a `submit` that found the thread running and sends after this gets its
/// payload back instead of queueing a present nothing will show (deep-zai-present P2, ADR-8930).
struct EndOnExit<T> {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    receiver: Option<std::sync::mpsc::Receiver<Job<T>>>,
    hooks: Hooks,
}

impl<T> EndOnExit<T> {
    /// The next job, waiting for one; `None` once every sender is gone.
    fn recv(&self) -> Option<Job<T>> {
        self.receiver.as_ref()?.recv().ok()
    }
}

impl<T> Drop for EndOnExit<T> {
    fn drop(&mut self) {
        let (count, idle) = &*self.pending;
        let mut count = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        count.ended = true;
        count.jobs = 0;
        // Jobs still queued drop here, unpresented.
        drop(self.receiver.take());
        drop(count);
        idle.notify_all();
        self.hooks.ended();
    }
}

/// The test-only stops of [`PresentWorker`] (`RaceHooks`).
#[cfg(test)]
type Hooks = Arc<RaceHooks>;

/// Outside tests the present path has no stops.
#[cfg(not(test))]
#[derive(Clone)]
struct Hooks;

/// A worker's stops when nobody sets any.
#[cfg(not(test))]
fn no_hooks() -> Hooks {
    Hooks
}

/// A worker's stops when the test sets none.
#[cfg(test)]
fn no_hooks() -> Hooks {
    Arc::default()
}

#[cfg(not(test))]
impl Hooks {
    fn checked(&self) {}
    fn ended(&self) {}
}

/// Where a test parks the present path to order a race without timing (P2).
#[cfg(test)]
#[derive(Default)]
struct RaceHooks {
    /// `submit` parks here after it found the thread running, before it sends.
    checked: Option<Park>,
    /// The thread parks here once it has marked itself ended, before it returns.
    ended: Option<Park>,
}

#[cfg(test)]
impl RaceHooks {
    fn checked(&self) {
        if let Some(park) = self.checked.as_ref() {
            park.park();
        }
    }

    fn ended(&self) {
        if let Some(park) = self.ended.as_ref() {
            park.park();
        }
    }
}

/// A stop: the parked thread says it arrived and waits for the test to let it go.
#[cfg(test)]
struct Park {
    reached: std::sync::mpsc::Sender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl Park {
    /// A stop, with the receiver of its arrivals and the sender that resumes it.
    fn pair() -> (Self, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, arrived) = std::sync::mpsc::channel();
        let (go, resume) = std::sync::mpsc::channel();
        let park = Self {
            reached,
            resume: Mutex::new(resume),
        };
        (park, arrived, go)
    }

    fn park(&self) {
        let _ = self.reached.send(());
        let resume = self
            .resume
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = resume.recv();
    }
}

impl<T: Send + 'static> PresentWorker<T> {
    /// Starts the thread. Target times are nanoseconds since `epoch`. It is a GPU worker
    /// (ADR-287) counted inside the driver only while it presents, so the end of the process
    /// never tears the driver down under a present.
    ///
    /// # Errors
    /// The operating system refuses a thread.
    pub fn new(epoch: Instant, present: impl FnMut(T) + Send + 'static) -> Result<Self, std::io::Error> {
        Self::start(epoch, present, no_hooks())
    }

    fn start(
        epoch: Instant,
        mut present: impl FnMut(T) + Send + 'static,
        hooks: Hooks,
    ) -> Result<Self, std::io::Error> {
        let (sender, receiver) = sync_channel::<Job<T>>(1);
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let done = pending.clone();
        let gaps = Arc::new(Mutex::new((None, PresentGaps::default())));
        let measured = gaps.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let exit_hooks = hooks.clone();
        let thread = regia_platform::GpuWorkerBuilder::new("regia-frame-gen-present")
            .driver_calls_marked()
            .spawn(move |gpu| {
                let queue = EndOnExit {
                    pending: done.clone(),
                    receiver: Some(receiver),
                    hooks: exit_hooks,
                };
                while let Some(job) = queue.recv() {
                    if !wait_until_or_cancel(epoch, job.at_ns, &stop) {
                        // Cancelled (a suspend or termination): the swap chain is going away,
                        // so the payload drops here, unpublished and out of the driver.
                        let (count, idle) = &*done;
                        let mut count = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        count.jobs = count.jobs.saturating_sub(1);
                        idle.notify_all();
                        continue;
                    }
                    // Once the exit asked the worker to stop, nothing enters the driver again.
                    let Some(call) = gpu.try_driver_call() else {
                        break;
                    };
                    {
                        let _span = tracing::info_span!("frame_gen_present").entered();
                        present(job.payload);
                    }
                    drop(call);
                    let now = Instant::now();
                    let mut spacing = measured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(last) = spacing.0 {
                        let gap_ms = now.duration_since(last).as_secs_f64() * 1000.0;
                        if gap_ms <= MAX_PRESENT_WAIT_NS as f64 / 1.0e6 {
                            spacing.1.push(gap_ms);
                        }
                    }
                    spacing.0 = Some(now);
                    drop(spacing);
                    let (count, idle) = &*done;
                    let mut count = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    count.jobs = count.jobs.saturating_sub(1);
                    idle.notify_all();
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            pending,
            gaps,
            cancelled,
            thread: Some(thread),
            hooks,
        })
    }

    /// The spacing of the presents since the worker started or [`Self::reset_gaps`].
    #[must_use]
    pub fn gaps(&self) -> PresentGaps {
        self.gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
            .finished()
    }

    /// Starts measuring the spacing again.
    pub fn reset_gaps(&self) {
        *self
            .gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (None, PresentGaps::default());
    }

    /// Queues `payload` for `at_ns`. Returns it when the thread is gone, including a thread that
    /// ends while this call runs.
    pub fn submit(&self, payload: T, at_ns: u64) -> Result<(), T> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(payload);
        };
        {
            let (count, _) = &*self.pending;
            let mut count = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if count.ended {
                return Err(payload);
            }
            count.jobs += 1;
        }
        self.hooks.checked();
        match sender.send(Job { payload, at_ns }) {
            Ok(()) => Ok(()),
            Err(error) => {
                let (count, _) = &*self.pending;
                let mut count = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                count.jobs = count.jobs.saturating_sub(1);
                Err(error.0.payload)
            }
        }
    }

    /// Blocks until everything submitted has been presented, or the thread has ended (then the
    /// jobs it did not present are dropped).
    pub fn wait_idle(&self) {
        let (count, idle) = &*self.pending;
        let mut count = count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while count.jobs > 0 && !count.ended {
            count = idle
                .wait(count)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Stops the worker, dropping every queued payload without presenting it. What a window
    /// suspend or termination needs: the swap chain is about to be destroyed, and a swap-chain
    /// image must neither be presented onto it nor outlive it. Waits for the thread, so on
    /// return no payload is in flight and blocked [`Self::wait_idle`] callers were released
    /// (the job count reached zero on the way out).
    pub fn cancel(mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // The sender goes with the flag: the loop takes each queued job, drops it without
        // presenting, then `recv` fails and the thread ends.
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl<T: Send + 'static> Drop for PresentWorker<T> {
    fn drop(&mut self) {
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One image the scene renders into.
pub struct SceneImage {
    /// The texture (render attachment, copy source, sampled).
    pub texture: wgpu::Texture,
    /// Its default view: what the scene renders into and the real frame's copy reads.
    pub view: wgpu::TextureView,
    /// A view with no sRGB decode: what the interpolator reads.
    pub raw_view: wgpu::TextureView,
}

impl SceneImage {
    /// A scene image of this size and format.
    #[must_use]
    pub fn new(device: &wgpu::Device, size: (u32, u32), format: wgpu::TextureFormat) -> Self {
        let raw_format = format.remove_srgb_suffix();
        let view_formats = [raw_format];
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame_gen scene"),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: if format == raw_format { &[] } else { &view_formats },
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let raw_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("frame_gen scene raw"),
            format: Some(raw_format),
            ..Default::default()
        });
        Self {
            texture,
            view,
            raw_view,
        }
    }
}

/// The two newest rendered frames.
pub struct SceneImages {
    images: [SceneImage; 2],
    /// The image the next frame renders into.
    next: usize,
    size: (u32, u32),
    format: wgpu::TextureFormat,
}

impl SceneImages {
    /// Two images of this size and format.
    #[must_use]
    pub fn new(device: &wgpu::Device, size: (u32, u32), format: wgpu::TextureFormat) -> Self {
        Self {
            images: [
                SceneImage::new(device, size, format),
                SceneImage::new(device, size, format),
            ],
            next: 0,
            size,
            format,
        }
    }

    /// Whether these images fit a window.
    #[must_use]
    pub fn matches(&self, size: (u32, u32), format: wgpu::TextureFormat) -> bool {
        self.size == size && self.format == format
    }

    /// The image the frame in progress renders into.
    #[must_use]
    pub fn current(&self) -> &SceneImage {
        &self.images[self.next]
    }

    /// The last finished frame's image.
    #[must_use]
    pub fn previous(&self) -> &SceneImage {
        &self.images[1 - self.next]
    }

    /// The frame in progress finished.
    pub fn advance(&mut self) {
        self.next = 1 - self.next;
    }
}

/// Which copy [`BlitPipelines::record`] makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlitSource {
    /// A source of the target's own format: the target ends with the same values.
    SameFormat,
    /// The interpolator's output, written to a target with this encoding: sRGB-encoded values to
    /// an sRGB view need decoding first.
    Interpolated,
}

/// The copy onto the present target, as a render pass (a swap-chain image is rarely a copy
/// destination).
#[derive(Default)]
pub struct BlitPipelines {
    base: Option<(wgpu::ShaderModule, wgpu::BindGroupLayout, wgpu::PipelineLayout)>,
    /// The pipelines built so far: a window has two at most, so a scan is enough.
    pipelines: Vec<((wgpu::TextureFormat, bool), wgpu::RenderPipeline)>,
    /// Bind groups by source view (four in steady state: two scenes, the output, the debug image).
    groups: crate::bind_cache::BindCache<wgpu::TextureView, wgpu::BindGroup>,
}

impl BlitPipelines {
    fn pipeline(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        decode: bool,
    ) -> Result<(), RenderError> {
        if self.pipelines.iter().any(|(key, _)| *key == (format, decode)) {
            return Ok(());
        }
        if self.base.is_none() {
            let module = crate::shader_lib::compose_source(
                super::wgsl::BLIT,
                "frame_gen blit",
                std::collections::HashMap::new(),
            )?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("frame_gen blit"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
            });
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("frame_gen blit"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("frame_gen blit"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            self.base = Some((module, bgl, layout));
        }
        let Some((module, _, layout)) = self.base.as_ref() else {
            return Err(RenderError::Shader("frame_gen blit base missing".into()));
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("frame_gen blit"),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(if decode { "fs_decode_srgb" } else { "fs_copy" }),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(device).as_deref(),
        });
        self.pipelines.push(((format, decode), pipeline));
        Ok(())
    }

    /// Copies `source` over all of `target`.
    ///
    /// # Errors
    /// The blit shader failed to compile.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        kind: BlitSource,
        target: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        let decode = kind == BlitSource::Interpolated && target_format.is_srgb();
        self.pipeline(device, target_format, decode)?;
        let Some((_, bgl, _)) = self.base.as_ref() else {
            return Err(RenderError::Shader("frame_gen blit base missing".into()));
        };
        let group = self.groups.get_or_insert_with(source.clone(), || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("frame_gen blit"),
                layout: bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                }],
            })
        });
        let Some((_, pipeline)) = self
            .pipelines
            .iter()
            .find(|(key, _)| *key == (target_format, decode))
        else {
            return Err(RenderError::Shader("frame_gen blit pipeline missing".into()));
        };
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("frame_gen blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(pipeline);
        pass.counted_set_bind_group(0, &group, &[]);
        pass.counted_draw(0..3, 0..1);
        Ok(())
    }

    /// Forgets the bind groups of images that are gone.
    pub fn clear_groups(&mut self) {
        self.groups.clear();
    }
}

/// The render resources of frame generation for one window. Installed with the feature `frame-gen`
/// on a windowed app; it builds nothing until frame generation is enabled.
#[derive(Resource, Default)]
pub struct FrameGenPresenter {
    /// The interpolator.
    pub passes: FrameGenPasses,
    /// The scene images at the current size.
    pub(crate) scene: Option<SceneImages>,
    /// The copy onto the present target.
    pub(crate) blit: BlitPipelines,
    /// The present thread, started on first use.
    pub(crate) worker: Option<PresentWorker<wgpu::SurfaceTexture>>,
    /// A frame generation failure: it stays off until the settings change or the app restarts.
    pub(crate) failed: Option<String>,
    /// The renderer's camera cuts when the last frame was rendered.
    pub(crate) cuts: Option<u64>,
    /// The next frame starts a new history (a resize, a failed frame, a pause).
    pub(crate) force_reset: bool,
    /// The last tick rendered through this path, so a gap can reset the history.
    pub(crate) was_active: bool,
    /// When the last frame on this path finished, on the pacer's clock.
    pub(crate) last_ready_ns: Option<u64>,
    /// The present mode the host configured, while frame generation runs on Mailbox instead.
    pub(crate) host_present_mode: crate::surface::HeldPresentMode,
    /// The pacer as it was before this tick fed it, kept for a tick that queues no present;
    /// refilled in place every tick, so a steady tick allocates nothing for it.
    pub(crate) pacer_before: crate::surface::PacerBefore,
}

impl FrameGenPresenter {
    /// Blocks until every present of the last tick happened. The plain path calls this first: it
    /// cannot acquire a swap-chain image while the worker still holds one.
    pub fn drain(&self) {
        if let Some(worker) = self.worker.as_ref() {
            worker.wait_idle();
        }
    }

    /// A suspend or termination: the window's swap chain is about to be destroyed, so the
    /// worker must drop any image it still holds instead of presenting onto the dead surface
    /// (audit-g-render3-platform). The next frame after a resume starts a new worker on first
    /// use.
    pub(crate) fn cancel_present(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.cancel();
        }
    }

    /// The frame in progress is finished; the next renders into the other image.
    pub(crate) fn advance_scene(&mut self) {
        if let Some(images) = self.scene.as_mut() {
            images.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A payload that records where it went: the present closure sees it, or `Drop` does when
    /// the worker lets it go without presenting.
    struct CountedPayload {
        id: u32,
        dropped: Arc<Mutex<Vec<u32>>>,
    }

    impl Drop for CountedPayload {
        fn drop(&mut self) {
            self.dropped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.id);
        }
    }

    fn payload(id: u32, dropped: &Arc<Mutex<Vec<u32>>>) -> CountedPayload {
        CountedPayload {
            id,
            dropped: dropped.clone(),
        }
    }

    /// audit-v035-conc M1: a present that panicked killed the thread with its job still
    /// counted, so the next frame's `wait_idle` blocked forever.
    #[test]
    fn a_present_that_panics_does_not_hang_wait_idle() {
        let worker = PresentWorker::new(Instant::now(), |id: u32| {
            assert_ne!(id, 1, "simulated present failure");
        })
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(worker.submit(0, 0).is_ok());
        worker.wait_idle();
        assert!(worker.submit(1, 0).is_ok());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.wait_idle();
            let refused = worker.submit(2, 0).is_err();
            let _ = tx.send(refused);
        });
        // Bounds a hang only.
        let refused = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("wait_idle never returned after the present thread died");
        assert!(refused, "a dead worker refuses new work");
    }

    /// audit-g-render3-platform: between frames the worker usually still holds the last image
    /// (its present waits for the pacer's time on the worker thread). A suspend destroys the
    /// swap chain while the image is held, so `cancel` must drop every queued payload — the
    /// one the thread took and the one still in the channel — without calling `present`, and
    /// end the thread without waiting out the target.
    #[test]
    fn cancel_drops_held_and_queued_payloads_without_presenting() {
        let epoch = Instant::now();
        let presented: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let dropped: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let log = presented.clone();
        let worker = PresentWorker::new(epoch, move |p: CountedPayload| {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(p.id);
        })
        .unwrap_or_else(|e| panic!("{e}"));
        // `u64::MAX` is past the cap: without a cancel each job would sit for the whole
        // MAX_PRESENT_WAIT_NS.
        worker
            .submit(payload(1, &dropped), u64::MAX)
            .unwrap_or_else(|_| panic!("the worker is running"));
        worker
            .submit(payload(2, &dropped), u64::MAX)
            .unwrap_or_else(|_| panic!("the worker is running"));
        worker.cancel();
        assert!(
            presented
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a cancelled present must not reach the surface"
        );
        assert_eq!(
            dropped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            [1, 2],
            "both payloads dropped without presenting"
        );
    }

    /// A cancel with nothing queued ends a parked worker as well, without touching `present`.
    #[test]
    fn cancel_with_an_idle_worker_ends_it_cleanly() {
        let epoch = Instant::now();
        let presented: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let dropped: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let log = presented.clone();
        let worker = PresentWorker::new(epoch, move |p: CountedPayload| {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(p.id);
        })
        .unwrap_or_else(|e| panic!("{e}"));
        worker.cancel();
        assert!(
            presented
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        assert!(
            dropped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    /// deep-zai-present P2 (ADR-8930): `submit` found the thread running, then the thread ended
    /// before the send. The send must fail and hand the payload back: a present accepted by a
    /// thread that has ended is never shown, and the caller would not requeue its screenshots.
    /// The hooks order the race (no timing): `submit` parks after its check, the thread ends
    /// (a present that panics) and parks once it marked itself ended, then `submit` sends.
    #[test]
    fn a_submit_that_races_the_thread_ending_gets_its_payload_back() {
        let (checked, submit_checked, resume_submit) = Park::pair();
        let (ended, thread_ended, resume_thread) = Park::pair();
        let hooks = Arc::new(RaceHooks {
            checked: Some(checked),
            ended: Some(ended),
        });
        let (in_present, presenting) = std::sync::mpsc::channel();
        let (fail, failing) = std::sync::mpsc::channel::<()>();
        let failing = Mutex::new(failing);
        let worker = PresentWorker::start(
            Instant::now(),
            move |id: u32| {
                if id == 1 {
                    let _ = in_present.send(());
                    let _ = failing
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .recv();
                    panic!("simulated present failure");
                }
            },
            hooks,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        // `submit(1)` parks after its check as well: let it through.
        let first = std::thread::scope(|scope| {
            let first = scope.spawn(|| worker.submit(1, 0).is_ok());
            submit_checked
                .recv()
                .unwrap_or_else(|_| panic!("submit(1) never reached its check"));
            let _ = resume_submit.send(());
            first.join().unwrap_or(false)
        });
        assert!(first, "a running worker takes the first present");
        presenting
            .recv()
            .unwrap_or_else(|_| panic!("the worker never started the present"));
        let refused = std::thread::scope(|scope| {
            let second = scope.spawn(|| worker.submit(2, 0));
            // `submit(2)` saw the thread running and holds a counted job.
            submit_checked
                .recv()
                .unwrap_or_else(|_| panic!("submit(2) never reached its check"));
            // The present fails: the thread unwinds and marks itself ended.
            let _ = fail.send(());
            thread_ended
                .recv()
                .unwrap_or_else(|_| panic!("the thread never ended"));
            // Now `submit(2)` sends to a thread that has ended.
            let _ = resume_submit.send(());
            let refused = second.join().unwrap_or(Ok(()));
            let _ = resume_thread.send(());
            refused
        });
        assert_eq!(
            refused,
            Err(2),
            "a present was accepted after the present thread ended"
        );
        // The job `submit(2)` counted does not hold `wait_idle`.
        worker.wait_idle();
        assert!(worker.submit(3, 0).is_err(), "an ended worker refuses new work");
        drop(worker);
    }

    #[test]
    fn a_wait_is_capped_and_never_negative() {
        assert_eq!(wait_ns(100, 40), 0);
        assert_eq!(wait_ns(100, 150), 50);
        assert_eq!(wait_ns(0, u64::MAX), MAX_PRESENT_WAIT_NS);
    }

    #[test]
    fn the_worker_presents_in_order_and_never_early() {
        let epoch = Instant::now();
        let seen: Arc<Mutex<Vec<(u32, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let worker = PresentWorker::new(epoch, move |id: u32| {
            let at = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id, at));
        })
        .unwrap_or_else(|e| panic!("{e}"));
        let base = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(0);
        // Targets in the future (they must wait), then one in the past (it must not).
        let targets = [base + 2_000_000, base + 3_000_000, base];
        for (id, at) in targets.iter().enumerate() {
            worker
                .submit(id as u32, *at)
                .unwrap_or_else(|_| panic!("the worker is running"));
        }
        worker.wait_idle();
        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(seen.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [0, 1, 2]);
        for ((_, presented), target) in seen.iter().zip(targets) {
            assert!(*presented >= target, "{presented} before {target}");
        }
        // Three presents, two gaps.
        let gaps = worker.gaps();
        assert_eq!(gaps.count, 2);
        assert!(gaps.max_ms >= gaps.mean_ms);
        worker.reset_gaps();
        assert_eq!(worker.gaps().count, 0);
        // Idle stays idle, and a drop joins the thread.
        worker.wait_idle();
        drop(worker);
    }
}
