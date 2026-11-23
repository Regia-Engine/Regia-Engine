// SPDX-License-Identifier: MPL-2.0
//! A render thread for a host that drives [`FrameRenderer`] itself (ADR-2074, `specs/22` T2.12).
//!
//! The host's thread keeps simulating while another thread records and submits the last frame.
//! The two meet at a channel of capacity one ([`RenderThreadSettings::queued_frames`]): the frame the
//! host hands over waits there until the render thread takes it, and the time the host spends blocked
//! behind a full channel is [`RenderThreadStats::host_wait_ms`]. So the host is at most one frame
//! ahead of the render thread and one frame queued, and a frame that takes long stops the host instead
//! of piling up.
//!
//! The queued frame costs latency. When the render thread is the slower side, a frame waits in the
//! channel for one frame and is then recorded for another, so it reaches the display about two frames
//! later than inline. With `queued_frames: 0` the channel is a rendezvous: the host waits until the
//! render thread takes the frame, the simulation of the next frame still overlaps the recording of
//! this one, and the added latency is at most one frame (ADR-2798, measured in a window by
//! `tests/frame_pipelining_s0.rs`).
//!
//! [`with_render_thread`] borrows the renderer for the whole scope, which is what makes this safe:
//! the host cannot reach the renderer while a frame records except through
//! [`RenderThreadHost::with_renderer`], which runs on the render thread in frame order. Frames are
//! taken in order and the renderer sees exactly the calls it would have seen inline, so the frames
//! are byte-identical to inline ones (`tests/render_thread.rs`). Every frame runs on one thread, so
//! the renderer's per-thread counters (`frame_stats`) add up as they do inline; read them on the
//! render thread.
//!
//! A frame's error stops nothing: it is logged, and the host reads it with
//! [`RenderThreadHost::take_error`]. A panic
//! on the render thread closes the channel, so [`RenderThreadHost::submit`] returns an error, and it
//! is raised again on the host's thread when the scope ends.
//!
//! The thread is scoped, not a registered GPU worker (`regia_platform::GpuWorkerBuilder`, ADR-287,
//! whose bodies are `'static`): the scope has joined it before the host can return to the end of
//! `main`, so it cannot be in the driver during the exit. A host must not call the process exit from
//! inside the scope.
//!
//! [`crate::RenderPlugin`] does not use this thread: `render_windows` still runs inline, because
//! systems across the engine take `ResMut<RuntimeRenderer>` during the simulation, which is exactly
//! when this thread would hold the renderer (ADR-2074, "Remaining").

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use crate::{FrameRenderer, FrameScene, FrameTarget, RenderDevice};

/// Frame errors kept for [`RenderThreadHost::take_error`]; older ones are dropped (each is logged,
/// and [`RenderThreadStats::failed`] counts them all).
const KEPT_ERRORS: usize = 16;

/// A call that runs on the render thread with the renderer ([`RenderThreadHost::with_renderer`]).
type RendererCall<'env> = Box<dyn FnOnce(&mut FrameRenderer) + Send + 'env>;

enum Command<'env> {
    Frame(Box<(FrameScene<'env>, FrameTarget<'env>)>),
    Call(RendererCall<'env>),
}

/// [`with_render_thread`]'s settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderThreadSettings {
    /// Frames the GPU may be behind before the render thread waits for the oldest. A host with a
    /// swapchain gets this from its acquire and passes 0 (no wait here).
    pub frames_in_flight: usize,
    /// Frames the channel holds while the render thread works on another (default 1). 0 makes the
    /// hand-over a rendezvous: [`RenderThreadHost::submit`] and [`RenderThreadHost::post`] return
    /// once the render thread has taken the command, which keeps the overlap and adds at most one
    /// frame of latency instead of two.
    pub queued_frames: usize,
}

impl Default for RenderThreadSettings {
    fn default() -> Self {
        Self {
            frames_in_flight: 2,
            queued_frames: 1,
        }
    }
}

/// What a scope did ([`with_render_thread`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RenderThreadStats {
    /// Frames rendered, failed ones included.
    pub frames: u64,
    /// Frames that returned an error.
    pub failed: u64,
    /// Milliseconds the host spent blocked behind a full channel.
    pub host_wait_ms: f64,
}

/// Why [`RenderThreadHost::submit`] did not hand the frame over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderThreadError {
    /// The render thread panicked; the panic is raised again when the scope ends.
    Stopped,
    /// The OS refused the thread ([`with_render_thread`] did not run the host).
    Spawn,
}

impl RenderThreadError {
    /// The agent-protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Stopped => "render_thread_stopped",
            Self::Spawn => "render_thread_spawn_failed",
        }
    }
}

impl std::fmt::Display for RenderThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "the render thread stopped"),
            Self::Spawn => write!(f, "the OS refused the render thread"),
        }
    }
}

impl std::error::Error for RenderThreadError {}

/// The host's side of a render thread ([`with_render_thread`]).
pub struct RenderThreadHost<'env> {
    send: mpsc::SyncSender<Command<'env>>,
    errors: Arc<Mutex<VecDeque<String>>>,
    host_wait_ms: f64,
    frames: u64,
}

impl<'env> RenderThreadHost<'env> {
    /// Hands a frame over. Blocks while the channel holds an earlier frame.
    ///
    /// # Errors
    ///
    /// [`RenderThreadError::Stopped`]: the render thread panicked.
    pub fn submit(
        &mut self,
        scene: FrameScene<'env>,
        target: FrameTarget<'env>,
    ) -> Result<(), RenderThreadError> {
        self.send_timed(Command::Frame(Box::new((scene, target))))?;
        self.frames += 1;
        Ok(())
    }

    /// Runs `call` on the render thread after every frame handed over so far, and returns its
    /// result: the way to reach the renderer (settings, reports) in a scope. It also waits for the
    /// frames before it, so it serves as a barrier (read a target back after it).
    ///
    /// # Errors
    ///
    /// [`RenderThreadError::Stopped`]: the render thread panicked.
    pub fn with_renderer<T: Send + 'env>(
        &mut self,
        call: impl FnOnce(&mut FrameRenderer) -> T + Send + 'env,
    ) -> Result<T, RenderThreadError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send_timed(Command::Call(Box::new(move |renderer| {
            // The host waits below; if it stopped waiting there is nobody to tell.
            let _ = reply.send(call(renderer));
        })))?;
        result.recv().map_err(|_| RenderThreadError::Stopped)
    }

    /// Queues `call` to run on the render thread after every frame and call handed over so far,
    /// without waiting for it. Blocks only while the channel holds an earlier command, as
    /// [`Self::submit`] does.
    ///
    /// A host that presents to a swapchain posts its whole frame this way (acquire, `render_into`,
    /// present), so the acquire's wait for the display falls on the render thread and not on the
    /// host. The call reports its own errors: it returns nothing.
    ///
    /// # Errors
    ///
    /// [`RenderThreadError::Stopped`]: the render thread panicked.
    pub fn post(
        &mut self,
        call: impl FnOnce(&mut FrameRenderer) + Send + 'env,
    ) -> Result<(), RenderThreadError> {
        self.send_timed(Command::Call(Box::new(call)))
    }

    /// The oldest frame error not read yet (a frame that failed does not stop the thread). Holds
    /// the last 16; each is also logged.
    pub fn take_error(&mut self) -> Option<String> {
        self.errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Milliseconds this host has spent blocked behind a full channel so far.
    #[must_use]
    pub fn host_wait_ms(&self) -> f64 {
        self.host_wait_ms
    }

    fn send_timed(&mut self, command: Command<'env>) -> Result<(), RenderThreadError> {
        let started = Instant::now();
        let sent = self.send.send(command);
        self.host_wait_ms += started.elapsed().as_secs_f64() * 1000.0;
        sent.map_err(|_| RenderThreadError::Stopped)
    }
}

/// Runs `host` on this thread with a render thread that owns `renderer` until `host` returns, then
/// waits for every frame handed over. Returns `host`'s result and what the scope did.
///
/// The thread is named `regia-render`; each frame opens a `render_thread_frame` span.
///
/// # Errors
///
/// [`RenderThreadError::Spawn`]: the OS refused the thread, and `host` did not run.
///
/// # Panics
///
/// Raises a panic of the render thread again on this thread.
pub fn with_render_thread<'env, R>(
    renderer: &'env mut FrameRenderer,
    device: &'env RenderDevice,
    settings: RenderThreadSettings,
    host: impl FnOnce(&mut RenderThreadHost<'env>) -> R,
) -> Result<(R, RenderThreadStats), RenderThreadError> {
    let (send, receive) = mpsc::sync_channel::<Command<'env>>(settings.queued_frames);
    let errors = Arc::new(Mutex::new(VecDeque::new()));
    let thread_errors = Arc::clone(&errors);
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .name("regia-render".into())
            .spawn_scoped(scope, move || {
                render_loop(renderer, device, settings, &receive, &thread_errors)
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(err) => {
                tracing::warn!(target: "regia_render", "the render thread did not start: {err}");
                return Err(RenderThreadError::Spawn);
            }
        };
        let mut side = RenderThreadHost {
            send,
            errors,
            host_wait_ms: 0.0,
            frames: 0,
        };
        let result = host(&mut side);
        let RenderThreadHost {
            send,
            host_wait_ms,
            frames,
            ..
        } = side;
        // The channel closes, the thread drains it and returns.
        drop(send);
        let failed = thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        Ok((
            result,
            RenderThreadStats {
                frames,
                failed,
                host_wait_ms,
            },
        ))
    })
}

/// The render thread: takes commands in order until the host closes the channel. Returns the
/// failed frame count.
fn render_loop<'env>(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    settings: RenderThreadSettings,
    receive: &mpsc::Receiver<Command<'env>>,
    errors: &Mutex<VecDeque<String>>,
) -> u64 {
    // Its spans are frame-thread time for telemetry's hitch cause, not background work.
    regia_core::profiling::mark_render_thread();
    let mut in_flight = VecDeque::with_capacity(settings.frames_in_flight + 1);
    let mut failed = 0;
    for command in receive {
        match command {
            Command::Frame(frame) => {
                let _span = tracing::info_span!("render_thread_frame").entered();
                let (scene, target) = *frame;
                if let Err(err) = renderer.render_into(device, &scene, target) {
                    failed += 1;
                    tracing::warn!(target: "regia_render", "render thread: a frame failed: {err}");
                    let mut errors = errors.lock().unwrap_or_else(PoisonError::into_inner);
                    if errors.len() == KEPT_ERRORS {
                        errors.pop_front();
                    }
                    errors.push_back(err.to_string());
                }
                if settings.frames_in_flight == 0 {
                    continue;
                }
                in_flight.extend(renderer.take_submission());
                if in_flight.len() > settings.frames_in_flight
                    && let Some(oldest) = in_flight.pop_front()
                    && device
                        .device
                        .poll(wgpu::PollType::Wait {
                            submission_index: Some(oldest),
                            timeout: None,
                        })
                        .is_err()
                {
                    tracing::warn!(target: "regia_render", "render thread: the wait for a frame failed");
                }
            }
            Command::Call(call) => call(renderer),
        }
    }
    failed
}
