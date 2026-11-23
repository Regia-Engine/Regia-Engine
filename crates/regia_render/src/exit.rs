// SPDX-License-Identifier: MPL-2.0
//! The renderer's part of the clean exit (ADR-188; the sequence is
//! [`regia_platform::run_clean_exit`]'s).
//!
//! - [`ExitStep::StopBackgroundWork`]: the pipeline warm queue shuts down (its workers release the
//!   device) and the runtime renderer's material-graph workers are told to stop (a build in
//!   flight ends during the next steps); from here on a device loss is not reported as one.
//! - [`ExitStep::WaitGpuIdle`]: `device.poll` waits for the last submission, bounded by
//!   [`CleanExitSettings::gpu_idle_timeout`]; a timeout is logged and the exit goes on.
//! - [`ExitStep::DropGpuResources`]: the runtime renderer moves to a helper thread that drops it
//!   part by part ([`crate::FrameRenderer::drop_timed`]) while the sequence clears the world on
//!   this thread, and the [`RenderDevice`] moves out of the world. Freeing a big map's GPU
//!   memory object by object takes the driver up to seconds (DO27 v0.1.8: 0.36–1.73 s); the GPU
//!   is idle by then, so nothing else waits for it.
//! - [`ExitStep::DropDevice`]: the renderer's drop is waited for, warm workers the shutdown left
//!   inside a compile are joined, the pipeline cache's bytes are read from the driver, then the
//!   device drops on a helper thread; all of it within
//!   [`CleanExitSettings::device_drop_timeout`] (wgpu waits for the queue to go idle without a
//!   bound when the device drops). The wgpu objects still alive afterwards are counted and
//!   logged ([`LiveGpuObjects`]). When none is left, the process's hold on the Vulkan drivers
//!   is released (ADR-5794): the last instance unloads them here, not in the process's exit
//!   handlers.
//! - [`ExitStep::SaveCaches`]: the pipeline cache file is written.
//!
//! [`last_gpu_exit_report`] tells what happened (tests, hosts' quit telemetry).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use regia_app::prelude::*;
use regia_platform::{CleanExitSettings, ExitHooks, ExitStep};

use crate::RenderDevice;
use crate::pipeline_cache::PendingCacheSave;

/// wgpu objects of one instance whose handles are still alive, from wgpu's own registry report
/// (`wgpu::Instance::generate_report`). All zero after a clean exit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveGpuObjects {
    /// Adapters.
    pub adapters: usize,
    /// Devices.
    pub devices: usize,
    /// Queues.
    pub queues: usize,
    /// Buffers.
    pub buffers: usize,
    /// Textures.
    pub textures: usize,
    /// Texture views.
    pub texture_views: usize,
    /// Samplers.
    pub samplers: usize,
    /// Bind groups and bind group layouts.
    pub bind_groups: usize,
    /// Pipeline layouts, render and compute pipelines, shader modules and pipeline caches.
    pub pipelines: usize,
    /// Query sets, command encoders and command buffers.
    pub other: usize,
}

impl LiveGpuObjects {
    /// The live objects of `instance`; `None` on backends without a registry (WebGPU).
    #[must_use]
    pub fn of(instance: &wgpu::Instance) -> Option<Self> {
        let report = instance.generate_report()?;
        let hub = &report.hub;
        Some(Self {
            adapters: hub.adapters.num_kept_from_user,
            devices: hub.devices.num_kept_from_user,
            queues: hub.queues.num_kept_from_user,
            buffers: hub.buffers.num_kept_from_user,
            textures: hub.textures.num_kept_from_user,
            texture_views: hub.texture_views.num_kept_from_user,
            samplers: hub.samplers.num_kept_from_user,
            bind_groups: hub.bind_groups.num_kept_from_user + hub.bind_group_layouts.num_kept_from_user,
            pipelines: hub.pipeline_layouts.num_kept_from_user
                + hub.render_pipelines.num_kept_from_user
                + hub.compute_pipelines.num_kept_from_user
                + hub.shader_modules.num_kept_from_user
                + hub.pipeline_caches.num_kept_from_user,
            other: hub.query_sets.num_kept_from_user
                + hub.command_encoders.num_kept_from_user
                + hub.command_buffers.num_kept_from_user,
        })
    }

    /// All live objects.
    #[must_use]
    pub fn total(&self) -> usize {
        self.adapters
            + self.devices
            + self.queues
            + self.buffers
            + self.textures
            + self.texture_views
            + self.samplers
            + self.bind_groups
            + self.pipelines
            + self.other
    }
}

/// What the renderer's exit steps did in the process's last clean exit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuExitReport {
    /// The pipeline warm queue was shut down.
    pub warm_stopped: bool,
    /// The GPU went idle within the timeout (`None`: there was no device).
    pub gpu_idle: Option<bool>,
    /// A device was taken out of the world for the device step.
    pub had_device: bool,
    /// The device drop finished within its timeout.
    pub device_dropped: bool,
    /// The runtime renderer's drop finished within the device step's time. False: the helper
    /// thread was still freeing GPU objects when the device step went on (it is detached).
    pub renderer_dropped: bool,
    /// wgpu objects of the device's instance still alive after the drop.
    pub live_after: Option<LiveGpuObjects>,
    /// Bytes of the pipeline cache file written (`None`: nothing to save, or the write failed).
    pub cache_saved_bytes: Option<u64>,
}

static LAST_REPORT: Mutex<Option<GpuExitReport>> = Mutex::new(None);

/// The renderer's part of the last clean exit in this process (`None` before one ran).
#[must_use]
pub fn last_gpu_exit_report() -> Option<GpuExitReport> {
    LAST_REPORT.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

#[derive(Default)]
struct ExitState {
    device: Option<RenderDevice>,
    /// The runtime renderer's drop on its helper thread: the report arrives when it is done.
    renderer_drop: Option<RendererDrop>,
    pending_cache: Option<PendingCacheSave>,
    report: GpuExitReport,
}

/// Drops `renderer` part by part, then the warm renderers `parked` in the game-frame warm.
fn drop_all(renderer: crate::FrameRenderer, parked: Vec<crate::FrameRenderer>) -> crate::RendererDropReport {
    let mut report = renderer.drop_timed();
    if !parked.is_empty() {
        let t = Instant::now();
        drop(parked);
        let elapsed = t.elapsed();
        report.parts.push(("parked warm renderers", elapsed));
        report.total += elapsed;
    }
    report
}

/// A runtime renderer being dropped on the `regia-exit-renderer` thread.
struct RendererDrop {
    done: std::sync::mpsc::Receiver<crate::RendererDropReport>,
    /// The device's live wgpu objects before the drop (`None`: no device or no registry).
    live_before: Option<LiveGpuObjects>,
    instance: Option<wgpu::Instance>,
}

impl RendererDrop {
    /// Starts dropping `renderer` on a helper thread (inline when none can start).
    fn start(
        renderer: crate::FrameRenderer,
        parked: Vec<crate::FrameRenderer>,
        instance: Option<wgpu::Instance>,
    ) -> Self {
        let live_before = instance.as_ref().and_then(LiveGpuObjects::of);
        let (tx, done) = std::sync::mpsc::channel();
        let slot = Arc::new(Mutex::new(Some((renderer, parked))));
        let theirs = slot.clone();
        // Registered (ADR-287): the end of the process waits for these frees.
        let spawned = regia_platform::GpuWorkerBuilder::new("regia-exit-renderer").spawn(move |_| {
            let renderers = theirs.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((renderer, parked)) = renderers {
                let _ = tx.send(drop_all(renderer, parked));
            }
        });
        if let Err(err) = spawned {
            tracing::warn!(target: "regia_render", "regia exit: no renderer-drop thread ({err}); dropped inline");
            let renderers = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((renderer, parked)) = renderers {
                let report = drop_all(renderer, parked);
                Self::log(&report, live_before, instance.as_ref());
            }
            let (_, done) = std::sync::mpsc::channel();
            return Self {
                done,
                live_before: None,
                instance: None,
            };
        }
        Self {
            done,
            live_before,
            instance,
        }
    }

    /// Waits for the drop until `deadline`; true when it finished (or ran inline).
    fn wait(self, deadline: Instant) -> bool {
        match self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(report) => {
                Self::log(&report, self.live_before, self.instance.as_ref());
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    target: "regia_render",
                    "regia exit: the runtime renderer is still being freed; the device drop goes on \
                     (the device is destroyed when the renderer's last resource is)"
                );
                false
            }
        }
    }

    fn log(
        report: &crate::RendererDropReport,
        before: Option<LiveGpuObjects>,
        instance: Option<&wgpu::Instance>,
    ) {
        let freed = match (before, instance.and_then(LiveGpuObjects::of)) {
            (Some(b), Some(a)) => format!(
                "; freed {} buffers, {} textures, {} texture views, {} bind groups, {} pipelines",
                b.buffers.saturating_sub(a.buffers),
                b.textures.saturating_sub(a.textures),
                b.texture_views.saturating_sub(a.texture_views),
                b.bind_groups.saturating_sub(a.bind_groups),
                b.pipelines.saturating_sub(a.pipelines)
            ),
            _ => String::new(),
        };
        tracing::info!(
            target: "regia_render",
            "regia exit: runtime renderer dropped in {:.1} ms (off the event-loop thread{freed}); slowest: {}",
            report.total.as_secs_f64() * 1e3,
            report.slowest_text(4)
        );
    }
}

impl ExitState {
    fn publish(&self) {
        *LAST_REPORT.lock().unwrap_or_else(PoisonError::into_inner) = Some(self.report.clone());
    }
}

fn settings(world: &World) -> CleanExitSettings {
    world
        .get_resource::<CleanExitSettings>()
        .cloned()
        .unwrap_or_default()
}

/// Adds the renderer's hooks to the app's [`ExitHooks`].
pub(crate) fn register(app: &mut App) {
    let state = Arc::new(Mutex::new(ExitState::default()));
    let mut hooks = app.world_mut().get_resource_or_init::<ExitHooks>();

    let s = state.clone();
    hooks.add(
        ExitStep::StopBackgroundWork,
        "pipeline warm queue",
        move |world| {
            crate::gpu_health::note_exiting();
            // The end of the process's wait for GPU workers shares one budget with this exit.
            regia_platform::note_exit_started();
            if let Some(mut renderer) = world.get_resource_mut::<crate::RuntimeRenderer>() {
                renderer.0.stop_background_workers();
            }
            let stopped = world
                .get_resource::<crate::PipelineWarmQueue>()
                .map(crate::PipelineWarmQueue::shutdown)
                .is_some();
            let mut st = s.lock().unwrap_or_else(PoisonError::into_inner);
            st.report.warm_stopped = stopped;
            st.publish();
        },
    );

    let s = state.clone();
    hooks.add(ExitStep::WaitGpuIdle, "GPU idle", move |world| {
        let Some(device) = world.get_resource::<RenderDevice>() else {
            return;
        };
        let timeout = settings(world).gpu_idle_timeout;
        let t = Instant::now();
        let idle = match device.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(timeout),
        }) {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!(
                    target: "regia_render",
                    "regia exit: the GPU did not go idle ({err}) after {:.0} ms; continuing",
                    t.elapsed().as_secs_f64() * 1e3
                );
                false
            }
        };
        let mut st = s.lock().unwrap_or_else(PoisonError::into_inner);
        st.report.gpu_idle = Some(idle);
        st.publish();
    });

    let s = state.clone();
    hooks.add(ExitStep::DropGpuResources, "renderers", move |world| {
        let mut st = s.lock().unwrap_or_else(PoisonError::into_inner);
        st.device = world.remove_resource::<RenderDevice>();
        st.report.had_device = st.device.is_some();
        // The GPU is idle (or the wait timed out): the renderer's frees no longer wait for it.
        // They run on a helper thread while the sequence clears the world here; the device
        // step waits for them.
        if let Some(renderer) = world.remove_resource::<crate::RuntimeRenderer>() {
            let parked = world
                .get_resource_mut::<crate::GameFrameWarm>()
                .map(|mut warm| warm.take_parked_renderers())
                .unwrap_or_default();
            let instance = st.device.as_ref().map(|d| d.instance.clone());
            st.renderer_drop = Some(RendererDrop::start(renderer.0, parked, instance));
        }
        st.publish();
    });

    let s = state.clone();
    hooks.add(ExitStep::DropDevice, "device", move |world| {
        let timeout = settings(world).device_drop_timeout;
        let deadline = Instant::now() + timeout;
        // The renderer's resources hold the device: their drop finishes first, within the
        // step's time.
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let renderer_drop = s.lock().unwrap_or_else(PoisonError::into_inner).renderer_drop.take();
        let renderer_dropped = renderer_drop.is_none_or(|pending| pending.wait(deadline));
        let renderer_ms = ms(t);
        // Warm workers still inside a compile after the warm's shutdown hold device clones:
        // join them next, within the step's time.
        let still = crate::pipeline_warm::join_leftover_workers(deadline.saturating_duration_since(Instant::now()));
        if still > 0 {
            tracing::warn!(
                target: "regia_render",
                "regia exit: {still} pipeline warm worker(s) still compiling; the device lives until they end \
                 (joined at process exit)"
            );
        }
        let mut st = s.lock().unwrap_or_else(PoisonError::into_inner);
        // The cache's bytes come from the driver: read them while the device lives.
        let t = Instant::now();
        st.pending_cache = crate::pipeline_cache::snapshot_installed();
        crate::pipeline_cache::uninstall();
        let cache_ms = ms(t);
        let Some(device) = st.device.take() else {
            // No device of the world to destroy: the step cannot tell what else in the process
            // still uses the driver, so the driver pin stays (ADR-5794).
            st.publish();
            return;
        };
        let instance = device.instance.clone();
        let t = Instant::now();
        let (done_tx, done) = std::sync::mpsc::channel();
        // wgpu waits for the queue without a bound when the device drops: drop it on a helper
        // thread and stop waiting at the timeout. When the thread cannot start, the closure (and
        // the device) drops right here.
        // Registered (ADR-287): a device drop past the step's time is waited for at the end of
        // the process, not left in the driver under its exit handlers.
        let spawned = regia_platform::GpuWorkerBuilder::new("regia-exit-device").spawn(move |_| {
            drop(device);
            let _ = done_tx.send(());
        });
        let dropped = match spawned {
            // At least a short wait, even when the workers used the step's time.
            Ok(_) => done
                .recv_timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .max(std::time::Duration::from_millis(100)),
                )
                .is_ok(),
            Err(err) => {
                tracing::warn!(target: "regia_render", "regia exit: no device-drop thread ({err}); dropped inline");
                true
            }
        };
        if !dropped {
            tracing::warn!(
                target: "regia_render",
                "regia exit: the GPU device was not destroyed within {} ms; continuing (a host watchdog \
                 ends the process if the driver stays stuck)",
                timeout.as_millis()
            );
        }
        let device_ms = ms(t);
        let live = LiveGpuObjects::of(&instance);
        let dropped = dropped || late_drop_confirmed(live.as_ref(), &done);
        drop(instance);
        // The process is ending: with the device destroyed the driver pin goes too (ADR-5794), so
        // the last instance unloads the drivers here, as before the pin, and none is left loaded
        // for the process's exit handlers. While any object of the instance lives (a device drop
        // past its time, a warm worker still compiling on a clone) the pin stays: no instance is
        // destroyed beside a thread that may be inside the driver.
        if dropped && live.is_none_or(|live| live.total() == 0) {
            crate::device::release_driver_pin();
        }
        let parts = format!(
            "renderer wait {renderer_ms:.1} ms, pipeline cache read {cache_ms:.1} ms, device drop {device_ms:.1} ms"
        );
        match live {
            Some(live) if live.total() == 0 => {
                tracing::info!(target: "regia_render", "regia exit: GPU device destroyed; no wgpu objects left ({parts})");
            }
            Some(live) => {
                tracing::warn!(target: "regia_render", "regia exit: wgpu objects still alive: {live:?} ({parts})");
            }
            None => tracing::info!(target: "regia_render", "regia exit: device step: {parts}"),
        }
        st.report.device_dropped = dropped;
        st.report.renderer_dropped = renderer_dropped;
        st.report.live_after = live;
        st.publish();
    });

    let s = state;
    hooks.add(ExitStep::SaveCaches, "pipeline cache", move |_| {
        let mut st = s.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pending) = st.pending_cache.take() {
            match pending.write() {
                Ok(bytes) => st.report.cache_saved_bytes = Some(bytes),
                Err(err) => {
                    tracing::warn!(target: "regia_render", "pipeline cache save at exit failed: {err}");
                }
            }
        }
        st.publish();
    });
}

/// A device drop that ended just past the step's wait: the instance already counts no device, so
/// the drop thread's signal is (about to be) sent. Waits for it a short, bounded time and reports
/// the device as destroyed before return when it comes (a loaded host finished the drop a few
/// milliseconds late: the device was gone, the report said it was not; pregate 8aeb615ef
/// `clean_exit_sequence`, `clean_exit_renderer_drop`). A device still alive is never confirmed.
fn late_drop_confirmed(live: Option<&LiveGpuObjects>, done: &std::sync::mpsc::Receiver<()>) -> bool {
    live.is_some_and(|live| live.devices == 0)
        && done
            .recv_timeout(std::time::Duration::from_millis(LATE_DROP_WAIT_MS))
            .is_ok()
}

/// How long [`late_drop_confirmed`] waits for the drop thread's signal once no device is left.
const LATE_DROP_WAIT_MS: u64 = 200;

#[cfg(test)]
mod late_drop_tests {
    use super::*;

    fn live(devices: usize) -> LiveGpuObjects {
        LiveGpuObjects {
            devices,
            ..LiveGpuObjects::default()
        }
    }

    #[test]
    fn a_drop_that_finished_late_is_confirmed_and_a_live_device_never_is() {
        // The drop thread finished (its signal is waiting) and the instance counts no device.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(()).expect("send");
        assert!(late_drop_confirmed(Some(&live(0)), &rx));
        // A device still alive is not confirmed, whatever the channel holds.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(()).expect("send");
        assert!(!late_drop_confirmed(Some(&live(1)), &rx));
        // No signal (the thread is gone without sending): not confirmed; the disconnected channel
        // answers at once.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        drop(tx);
        assert!(!late_drop_confirmed(Some(&live(0)), &rx));
        // No count from the backend: not confirmed.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(()).expect("send");
        assert!(!late_drop_confirmed(None, &rx));
    }
}
