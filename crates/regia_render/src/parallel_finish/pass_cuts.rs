// SPDX-License-Identifier: MPL-2.0
//! Pass-boundary cuts for the Android command-buffer pool experiment (ADR-4657).
//!
//! The scope owns its device/queue handles, never borrowed renderer fields. The encoder's
//! address is compared only: it is never dereferenced. The caller keeps that encoder in
//! its original local slot until `finish`, so unrelated encoders on this thread are excluded.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{FinishWorkers, Job, Pending, Reply, Segments, Shared};

/// Five passes leave room for their transition buffers in a 16-buffer Vulkan batch.
const PASS_LIMIT: usize = 5;

struct Active {
    device: wgpu::Device,
    encoder_address: usize,
    shared: Option<Arc<Shared>>,
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    // Empty reply slots and vector capacity survive a scope; GPU handles do not.
    replies: Vec<Arc<Reply>>,
    pending: Vec<Pending>,
    next_reply: usize,
    passes: usize,
    total_passes: usize,
    max_passes: usize,
    completed_segments: usize,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[cfg(test)]
thread_local! {
    static TEST_ENABLED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The experiment is Android-only; tests can exercise the identical recording path.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(enabled) = TEST_ENABLED.get() {
        return enabled;
    }
    cfg!(target_os = "android")
}

impl State {
    fn matches(&self, encoder: &wgpu::CommandEncoder) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.encoder_address == std::ptr::from_ref(encoder).addr())
    }

    fn boundary(&mut self) {
        self.max_passes = self.max_passes.max(self.passes);
        self.passes = 0;
        self.completed_segments += 1;
    }
}

/// A frame-local scope, confined to its recording thread. Nested scopes stay inactive.
pub(crate) struct PassCuts {
    active: bool,
    _thread: PhantomData<Rc<()>>,
}

impl PassCuts {
    pub(crate) fn start(
        device: &wgpu::Device,
        encoder: &wgpu::CommandEncoder,
        workers: &mut FinishWorkers,
    ) -> Self {
        let active = STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            if state.active.is_some() {
                return false;
            }
            if !workers.started {
                workers.start();
            }
            state.active = Some(Active {
                device: device.clone(),
                encoder_address: std::ptr::from_ref(encoder).addr(),
                shared: workers.shared.clone(),
            });
            state.next_reply = 0;
            state.passes = 0;
            state.total_passes = 0;
            state.max_passes = 0;
            state.completed_segments = 0;
            true
        });
        Self {
            active,
            _thread: PhantomData,
        }
    }

    pub(crate) fn finish(self, segments: &mut Segments) {
        if self.active {
            STATE.with(|slot| {
                let mut state = slot.borrow_mut();
                transfer(&mut state, segments);
                tracing::debug!(
                    target: "regia_render",
                    passes = state.total_passes,
                    segments = state.completed_segments + 1,
                    max_passes = state.max_passes.max(state.passes),
                    "frame pass segments"
                );
            });
        }
    }
}

impl Drop for PassCuts {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let (active, mut pending) = STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            (state.active.take(), std::mem::take(&mut state.pending))
        });
        // An early return/unwind has not transferred these replies to Segments. Drain
        // work exactly as Segments::drop does, including jobs left by stopped workers.
        if let Some(shared) = active.as_ref().and_then(|active| active.shared.as_ref()) {
            while let Some(job) = shared.pop() {
                job.run();
            }
        }
        for entry in pending.drain(..) {
            let _ = entry.take();
        }
        STATE.with(|slot| slot.borrow_mut().pending = pending);
    }
}

fn transfer(state: &mut State, segments: &mut Segments) {
    if !state.pending.is_empty() {
        segments.pending.append(&mut state.pending);
        if segments.shared.is_none() {
            segments.shared = state.active.as_ref().and_then(|active| active.shared.clone());
        }
    }
}

/// Append automatic cuts before an explicit cut or a separately finished vendor buffer.
pub(crate) fn flush(encoder: &wgpu::CommandEncoder, segments: &mut Segments) {
    STATE.with(|slot| {
        let mut state = slot.borrow_mut();
        if state.matches(encoder) {
            transfer(&mut state, segments);
        }
    });
}

/// Observe a successful explicit replacement of the scoped encoder.
pub(crate) fn boundary(encoder: &wgpu::CommandEncoder) {
    STATE.with(|slot| {
        let mut state = slot.borrow_mut();
        if state.matches(encoder) {
            state.boundary();
        }
    });
}

/// Cut immediately before the next pass would exceed the scope's budget.
pub(crate) fn before_pass(encoder: &mut wgpu::CommandEncoder) {
    STATE.with(|slot| {
        let state = &mut *slot.borrow_mut();
        if !state.matches(encoder) {
            return;
        }
        if state.passes == PASS_LIMIT {
            let _span = tracing::info_span!("finish_pass_cut").entered();
            let active = state.active.as_ref().expect("matching active scope");
            let fresh = active
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("frame pass segment"),
                });
            let done = std::mem::replace(encoder, fresh);
            if let Some(shared) = active.shared.as_ref() {
                let index = state.next_reply;
                if state.replies.len() <= index {
                    state.replies.push(Arc::new(Reply::new()));
                }
                let reply = Arc::clone(&state.replies[index]);
                shared.unfinished.fetch_add(1, Ordering::AcqRel);
                shared
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back(Job {
                        encoder: Some(done),
                        reply: Arc::clone(&reply),
                        unfinished: Arc::clone(&shared.unfinished),
                    });
                shared.ready.notify_one();
                state.pending.push(Pending::Worker(reply));
                state.next_reply += 1;
            } else {
                state.pending.push(Pending::Ready(done.finish()));
            }
            state.boundary();
        }
        state.passes += 1;
        state.total_passes += 1;
    });
}

/// Observe frame-module pass beginnings without changing the published work ratchet.
pub(crate) trait SegmentedEncoder {
    fn segmented_render_pass(&mut self, desc: &wgpu::RenderPassDescriptor<'_>) -> wgpu::RenderPass<'_>;
    fn segmented_compute_pass(&mut self, desc: &wgpu::ComputePassDescriptor<'_>) -> wgpu::ComputePass<'_>;
}

impl SegmentedEncoder for wgpu::CommandEncoder {
    fn segmented_render_pass(&mut self, desc: &wgpu::RenderPassDescriptor<'_>) -> wgpu::RenderPass<'_> {
        before_pass(self);
        self.begin_render_pass(desc)
    }

    fn segmented_compute_pass(&mut self, desc: &wgpu::ComputePassDescriptor<'_>) -> wgpu::ComputePass<'_> {
        before_pass(self);
        self.begin_compute_pass(desc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_work::counted::CountedEncoder;

    fn compute(encoder: &mut wgpu::CommandEncoder, passes: usize) {
        for _ in 0..passes {
            drop(encoder.counted_compute_pass(&wgpu::ComputePassDescriptor::default()));
        }
    }

    #[test]
    fn shadow_and_post_frame_is_identical_with_bounded_segments() {
        use crate::{
            DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
            PostSettings, RenderProfile, RenderProfilePreset,
        };

        struct Enable;
        impl Drop for Enable {
            fn drop(&mut self) {
                TEST_ENABLED.set(None);
            }
        }

        let device = crate::RenderDevice::headless().expect("headless GPU required (lavapipe)");
        let (vertices, indices) = crate::box_mesh([1.0, 1.0, 1.0]);
        let scene = LabScene {
            width: 64,
            height: 64,
            eye: [0.0, 3.0, 8.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 300.0,
            depth_prepass: false,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 15_000.0,
            double_sided: false,
            post: Some(PostSettings {
                taa: false,
                ..PostSettings::default()
            }),
            directional: DirectionalLightUniform::new([0.3, -0.85, 0.25], [1.0; 3], 90_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial::default()],
            meshes: vec![LabMesh {
                vertices,
                indices,
                ..LabMesh::unskinned()
            }],
        };
        let quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh));
        let frame = scene.as_frame_scene_with_quality(quality);
        let _enable = Enable;
        TEST_ENABLED.set(Some(false));
        let baseline = FrameRenderer::new()
            .render_offscreen(&device, &frame)
            .expect("baseline frame");
        TEST_ENABLED.set(Some(true));
        let segmented = FrameRenderer::new()
            .render_offscreen(&device, &frame)
            .expect("segmented frame");
        assert_eq!(
            segmented.pixels, baseline.pixels,
            "segmentation must preserve pixels exactly"
        );
        STATE.with(|slot| {
            let state = slot.borrow();
            assert!(state.total_passes > 10, "exercise a real shadow/post chain");
            assert!(state.max_passes.max(state.passes) <= PASS_LIMIT);
            assert!(state.active.is_none());
        });
    }

    #[test]
    fn automatic_and_manual_segments_preserve_copy_order() {
        let device = crate::RenderDevice::headless().expect("headless GPU required (lavapipe)");
        let d = &device.device;
        let source = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 12,
            usage: wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: true,
        });
        source
            .slice(..)
            .get_mapped_range_mut()
            .expect("mapped source")
            .copy_from_slice(&[1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
        source.unmap();
        let destination = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut workers = FinishWorkers::default();
        workers.set_threads(1);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut segments = Segments::default();
        let scope = PassCuts::start(d, &encoder, &mut workers);
        compute(&mut encoder, 5);
        encoder.copy_buffer_to_buffer(&source, 0, &destination, 0, 4);
        compute(&mut encoder, 1); // Automatic cut before this pass.
        encoder.copy_buffer_to_buffer(&source, 4, &destination, 0, 4);
        workers.cut(d, &mut encoder, &mut segments);
        assert_eq!(segments.len(), 2, "automatic prefix precedes the manual cut");
        compute(&mut encoder, 5); // A successful manual cut resets the pass budget.
        encoder.copy_buffer_to_buffer(&source, 8, &destination, 0, 4);
        scope.finish(&mut segments);
        assert_eq!(segments.len(), 2, "no extra cut at the exact budget");
        let buffers = workers.join(segments, encoder).expect("finish ordered segments");
        device.queue.submit(buffers);
        destination.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        d.poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU completion");
        assert_eq!(
            &*destination.slice(..).get_mapped_range().expect("mapped output"),
            &[3; 4]
        );
    }

    #[test]
    fn foreign_encoders_and_nested_scopes_do_not_enter_the_frame() {
        let device = crate::RenderDevice::headless().expect("headless GPU required (lavapipe)");
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(0);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut foreign = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut segments = Segments::default();
        let scope = PassCuts::start(d, &encoder, &mut workers);
        {
            let nested = PassCuts::start(d, &foreign, &mut workers);
            compute(&mut foreign, 21);
            nested.finish(&mut segments);
        }
        compute(&mut encoder, 6);
        scope.finish(&mut segments);
        assert_eq!(segments.len(), 1, "only the frame's sixth pass cuts");
        let buffers = workers.join(segments, encoder).expect("finish frame");
        assert_eq!(
            buffers.len(),
            2,
            "zero workers still respects the explicit scope's bound"
        );
        device.queue.submit(buffers);
        device.queue.submit([foreign.finish()]);
        d.poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU completion");
    }

    #[test]
    fn an_unwind_drains_jobs_and_releases_the_scope() {
        let device = crate::RenderDevice::headless().expect("headless GPU required (lavapipe)");
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(1);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            let _scope = PassCuts::start(d, &encoder, &mut workers);
            compute(&mut encoder, 41);
            panic!("test cancellation");
        }));
        assert!(caught.is_err());
        STATE.with(|slot| {
            let state = slot.borrow();
            assert!(state.active.is_none(), "the device must not remain in TLS");
            assert!(state.pending.is_empty());
            for reply in &state.replies {
                assert!(matches!(
                    *reply
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    super::super::ReplyState::Empty
                ));
            }
        });
        if let Some(shared) = workers.shared.as_ref() {
            assert_eq!(shared.unfinished.load(Ordering::Acquire), 0);
            assert!(shared.pop().is_none());
        }
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut segments = Segments::default();
        let scope = PassCuts::start(d, &encoder, &mut workers);
        compute(&mut encoder, 6);
        scope.finish(&mut segments);
        assert_eq!(segments.len(), 1, "the next frame can reuse the scope");
        let buffers = workers.join(segments, encoder).expect("next frame");
        device.queue.submit(buffers);
        d.poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU completion");
    }

    #[test]
    fn destroying_the_device_does_not_leave_automatic_jobs_or_tls_handles() {
        static GATE: std::sync::OnceLock<std::sync::Barrier> = std::sync::OnceLock::new();
        fn pause_worker() {
            let gate = GATE.get().expect("test installed the worker gate");
            gate.wait();
            gate.wait();
        }
        struct ReleaseWorker(&'static std::sync::Barrier);
        impl Drop for ReleaseWorker {
            fn drop(&mut self) {
                self.0.wait();
            }
        }

        let device = crate::RenderDevice::headless().expect("headless GPU required (lavapipe)");
        let d = &device.device;
        let mut workers = FinishWorkers::default();
        workers.set_threads(1);
        let gate = GATE.get_or_init(|| std::sync::Barrier::new(2));
        workers.set_start_hook(pause_worker);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let scope = PassCuts::start(d, &encoder, &mut workers);
        assert_eq!(workers.threads, 1, "the test needs its gated worker");
        gate.wait();
        let _release = ReleaseWorker(gate);
        compute(&mut encoder, 41);
        let shared = workers.shared.as_ref().expect("worker queue");
        assert_eq!(shared.unfinished.load(Ordering::Acquire), 8);
        // Explicit device destruction exercises cleanup without inducing a driver fault.
        // The worker cannot finish anything yet. Scope teardown must drain every queued
        // job itself; any finish failure is caught and discarded, without a timing race.
        d.destroy();
        drop(scope);
        drop(encoder);
        STATE.with(|slot| {
            let state = slot.borrow();
            assert!(state.active.is_none());
            assert!(state.pending.is_empty());
            for reply in &state.replies {
                assert!(matches!(
                    *reply
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    super::super::ReplyState::Empty
                ));
            }
        });
        assert_eq!(shared.unfinished.load(Ordering::Acquire), 0);
        assert!(shared.pop().is_none());
    }

    #[test]
    fn counted_passes_cut_inside_one_module_call() {
        let device = crate::RenderDevice::headless().expect("headless GPU required (lavapipe)");
        let d = &device.device;
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("pass boundary regression"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut workers = FinishWorkers::default();
        workers.set_threads(1);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let mut segments = Segments::default();
        let cuts = PassCuts::start(d, &encoder, &mut workers);

        // One module call records more than two segments' worth of passes. There is no
        // module-boundary cut inside it: this is the gap in the two ADR-3953 experiments.
        for index in 0u32..13 {
            if index.is_multiple_of(2) {
                drop(encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("shadow or post stage"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                }));
            } else {
                drop(encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("compute stage"),
                    timestamp_writes: None,
                }));
            }
        }
        cuts.finish(&mut segments);
        assert_eq!(segments.len(), 2, "13 passes require cuts before pass 6 and 11");
        let buffers = workers.join(segments, encoder).expect("finish segments");
        assert_eq!(buffers.len(), 3);
        device.queue.submit(buffers);
        d.poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU completion");
    }
}
