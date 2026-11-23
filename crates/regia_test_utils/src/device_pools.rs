// SPDX-License-Identifier: MPL-2.0
//! wgpu's pool of usage scopes, filled on purpose, so that an allocation test's frames do not
//! meet its growth at a frame the scheduler picks (ADR-5718).
//!
//! wgpu-core keeps a pool of usage scopes per device. A pass takes one while
//! `CommandEncoder::finish` replays it and gives it back after, so the pool holds as many scopes
//! as passes were ever replayed at the same moment. The renderer finishes a frame's encode
//! segments on its encode workers and on the drawing thread, so the first frame in which one more
//! of those threads is inside a pass at the same time makes a new scope: six allocations (three
//! lists for the buffers and three for the textures the device tracks), once per thread, in a
//! frame that depends on the machine's load. A pooled scope's lists also grow when it is taken
//! again after the device has made more buffers or textures, which for the scope only a rare
//! overlap reaches is again a frame nobody chose.
//!
//! A scope also keeps a map of the textures a pass uses in part (one layer of an array, one mip
//! level), which grows the first time that scope serves a pass with more of them; which scope a
//! pass gets depends on which are out at that moment.
//!
//! [`fill_usage_scope_pool`] makes all of that happen at a known place. It starts as many threads
//! as the pool should hold scopes; each records a pass on one-pixel targets and finishes it.
//! wgpu-core logs every command it replays at trace level, and a logger installed here holds each
//! thread at that line, inside its pass with a scope taken, until all of them are there. The pool
//! then holds that many scopes, each sized for every buffer and texture the device has made so
//! far, and each with a map that has held as many textures used in part as a pass has color
//! targets (eight on a desktop device, which leaves the map room for 14).
//!
//! This leans on two things wgpu does not promise: that a pass is replayed with a scope taken,
//! and the text of one trace line. `regia_render`'s `usage_scope_pool_alloc` test holds both: it
//! fails when a wgpu upgrade changes either, and the steady-frame tests then still pass on their
//! slack, as they did before the fill.

use std::cell::Cell;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

/// What wgpu-core logs (trace level) when it replays `RenderPass::set_blend_constant`, with the
/// pass's usage scope taken.
const REPLAY_LINE: &str = "RenderPass::set_blend_constant";

/// The most color targets a fill pass takes (fewer where the device allows fewer). Each is one
/// layer of its own two-layer texture: a texture used in part, which is what a scope's map holds.
const MOST_PARTIAL_TARGETS: u32 = 8;

/// How long a thread waits inside its pass for the others. Not a measurement: every thread
/// arrives within its own `finish`, and this only ends a fill that something outside stalled (a
/// thread of the process destroying a resource takes a lock that the waiting passes hold open).
/// The fill then reports `false`.
const MEET_WITHIN: Duration = Duration::from_secs(60);

thread_local! {
    /// Whether this thread is one of a running fill's.
    static IN_FILL: Cell<bool> = const { Cell::new(false) };
}

/// Where the threads of a fill meet.
struct Meeting {
    /// Threads the running fill started.
    expected: usize,
    /// Threads that have reached the replayed command.
    arrived: usize,
    /// No fill is running, or the running one is decided.
    over: bool,
    /// Every thread of the last fill was inside its pass at the same moment.
    met: bool,
}

static MEETING: Mutex<Meeting> = Mutex::new(Meeting {
    expected: 0,
    arrived: 0,
    over: true,
    met: false,
});
static DECIDED: Condvar = Condvar::new();

fn meeting() -> MutexGuard<'static, Meeting> {
    MEETING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Called inside a pass's replay on a thread of the fill: waits until every thread is here.
fn meet() {
    let mut meeting = meeting();
    if meeting.over {
        return;
    }
    meeting.arrived += 1;
    if meeting.arrived >= meeting.expected {
        meeting.met = true;
        meeting.over = true;
        DECIDED.notify_all();
        return;
    }
    let (mut meeting, waited) = DECIDED
        .wait_timeout_while(meeting, MEET_WITHIN, |meeting| !meeting.over)
        .unwrap_or_else(PoisonError::into_inner);
    if waited.timed_out() && !meeting.over {
        meeting.over = true;
        DECIDED.notify_all();
    }
}

/// Ends a thread's part in the fill, also when it unwinds: a thread that leaves before everyone
/// met (its replay logged nothing) releases the others.
struct Leave;

impl Drop for Leave {
    fn drop(&mut self) {
        let _ = IN_FILL.try_with(|in_fill| in_fill.set(false));
        let mut meeting = meeting();
        if !meeting.over {
            meeting.over = true;
            DECIDED.notify_all();
        }
    }
}

/// The `log` logger that turns wgpu-core's trace line into the meeting point. It handles nothing
/// else and prints nothing.
struct ReplayHook;

impl log::Log for ReplayHook {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() == log::Level::Trace && IN_FILL.try_with(Cell::get).unwrap_or(false)
    }

    fn log(&self, record: &log::Record<'_>) {
        // A line without arguments is a `&'static str`: comparing it allocates nothing.
        if self.enabled(record.metadata()) && record.args().as_str() == Some(REPLAY_LINE) {
            meet();
        }
    }

    fn flush(&self) {}
}

/// Fills `device`'s usage-scope pool to `scopes` scopes, each sized for every buffer and texture
/// the device has made so far, and returns whether it did.
///
/// Call it for a renderer with `encode_threads() + 1` once the frame's resources exist (after the
/// warm frames: `SteadyAllocCheck::run_warmed`), and again for each renderer that makes resources
/// of its own on the same device. The frames after it start from different pooled encoders, so
/// measure after settling again.
///
/// `false` when the meeting could not be set up or did not happen: another `log` logger is
/// already installed in the process, the build strips trace-level `log` lines, or wgpu no longer
/// logs the line this waits for. Nothing is lost then but the certainty: the pool grows when the
/// threads first overlap, as without the call.
pub fn fill_usage_scope_pool(device: &wgpu::Device, scopes: usize) -> bool {
    fill_usage_scope_pool_on(device, scopes, || ())
}

/// [`fill_usage_scope_pool`], with `on_thread` called first on each thread it starts (a test of
/// the fill itself counts those threads: `alloc_counter::count_this_thread`).
pub fn fill_usage_scope_pool_on(device: &wgpu::Device, scopes: usize, on_thread: impl Fn() + Sync) -> bool {
    /// One fill at a time in a process: they share the meeting and the log level.
    static FILLING: Mutex<()> = Mutex::new(());
    static HOOKED: OnceLock<bool> = OnceLock::new();
    if scopes == 0 {
        return true;
    }
    let _one_at_a_time = FILLING.lock().unwrap_or_else(PoisonError::into_inner);
    if !*HOOKED.get_or_init(|| log::set_logger(&ReplayHook).is_ok()) {
        return false;
    }
    // One layer of a two-layer texture per target: the pass uses each texture in part.
    let partial_targets = device
        .limits()
        .max_color_attachments
        .clamp(1, MOST_PARTIAL_TARGETS);
    let views: Vec<wgpu::TextureView> = (0..partial_targets)
        .map(|_| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("usage scope pool fill"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 2,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor {
                    label: Some("usage scope pool fill"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: 0,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
        })
        .collect();
    let targets: Vec<Option<wgpu::RenderPassColorAttachment<'_>>> = views
        .iter()
        .map(|view| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })
        })
        .collect();
    {
        let mut meeting = meeting();
        *meeting = Meeting {
            expected: scopes,
            arrived: 0,
            over: false,
            met: false,
        };
    }
    // wgpu-core's lines are trace level; the level goes back to what it was after the fill, so
    // the rest of the process logs (or does not) as before.
    let level = log::max_level();
    log::set_max_level(log::LevelFilter::Trace);
    std::thread::scope(|threads| {
        for _ in 0..scopes {
            threads.spawn(|| {
                let _leave = Leave;
                on_thread();
                IN_FILL.with(|in_fill| in_fill.set(true));
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("usage scope pool fill"),
                });
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("usage scope pool fill"),
                        color_attachments: &targets,
                        ..Default::default()
                    });
                    pass.set_blend_constant(wgpu::Color::BLACK);
                }
                // The replay, and the meeting inside it. The buffer is never submitted.
                drop(encoder.finish());
            });
        }
    });
    log::set_max_level(level);
    let mut meeting = meeting();
    meeting.over = true;
    meeting.met
}
