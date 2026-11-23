// SPDX-License-Identifier: MPL-2.0
//! Start-up work and idle memory on the agent protocol (ADR-2987, `book/agents/startup-idle.md`).
//!
//! - `app.startup_work`: what the app did from process start to its first presented frame, as
//!   counts: pipelines created, live GPU objects (shader modules, bind group layouts, pipelines),
//!   GPU buffers and textures the tracked device created, files opened and bytes read through the
//!   VFS and the asset server, live threads, and heap allocations and peak heap (with the tagging
//!   allocator). [`StartupWorkPlugin`] takes the snapshot once, in `Last` of the first frame that
//!   presented a window (`first_present` boot mark) or rendered a texture camera (headless hosts);
//!   an app that does neither within [`STARTUP_FRAME_LIMIT`] frames is captured then.
//! - `memory.idle_ledger`: lets the app run `frames` frames untouched and answers with the heap
//!   bytes each subsystem holds at the end, what each gained over the window, and the allocations
//!   per idle frame. `memory.report` stays a snapshot that answers at once; this method waits.
//!
//! Every figure is a counter the engine already keeps or one added at a wrapper (VFS, asset
//! server); nothing here is timed except the informational `ms_since_process_start`. The start-up
//! ratchet (`crates/regia/tests/startup_idle_ratchet.rs`) compares the same figures against a
//! committed table.

use crossbeam_channel::Sender;
use regia_app::prelude::*;
use regia_core::memory::{self, MemorySnapshot, OwnerSnapshot, Subsystem};
use serde_json::{Map, Value, json};

use crate::protocol::{self, Response, RpcError};

/// Frames after which an app that never presented nor rendered a texture camera (a server, a
/// headless tool without views) takes its start-up snapshot anyway.
pub const STARTUP_FRAME_LIMIT: u64 = 600;

/// Largest `frames` a `memory.idle_ledger` call may ask for.
pub const IDLE_LEDGER_MAX_FRAMES: u64 = 100_000;

/// `frames` when a `memory.idle_ledger` call names none.
pub const IDLE_LEDGER_DEFAULT_FRAMES: u64 = 120;

/// What ended the start-up window ([`StartupCapture::trigger`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupTrigger {
    /// The first window frame (splash or game) was presented (`first_present` boot mark).
    FirstPresent,
    /// The first texture-target camera was rendered (headless and offscreen hosts).
    FirstTextureView,
    /// Neither happened within [`STARTUP_FRAME_LIMIT`] frames.
    FrameLimit,
}

impl StartupTrigger {
    /// Stable snake-case name (`trigger` in the answer).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FirstPresent => "first_present",
            Self::FirstTextureView => "first_texture_view",
            Self::FrameLimit => "frame_limit",
        }
    }
}

/// Live wgpu objects of one instance, from wgpu's registry report (handles the engine still holds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuLiveObjects {
    /// Shader modules.
    pub shader_modules: u64,
    /// Bind group layouts (wgpu shares equal layouts, so this counts distinct ones).
    pub bind_group_layouts: u64,
    /// Pipeline layouts.
    pub pipeline_layouts: u64,
    /// Render pipelines.
    pub render_pipelines: u64,
    /// Compute pipelines.
    pub compute_pipelines: u64,
    /// Bind groups.
    pub bind_groups: u64,
    /// Buffers.
    pub buffers: u64,
    /// Textures.
    pub textures: u64,
    /// Texture views.
    pub texture_views: u64,
    /// Samplers.
    pub samplers: u64,
}

impl GpuLiveObjects {
    /// The live objects of `instance`; `None` on a backend without a registry report (WebGPU).
    #[must_use]
    pub fn of(instance: &wgpu::Instance) -> Option<Self> {
        let report = instance.generate_report()?;
        let hub = &report.hub;
        let n = |count: usize| count as u64;
        Some(Self {
            shader_modules: n(hub.shader_modules.num_kept_from_user),
            bind_group_layouts: n(hub.bind_group_layouts.num_kept_from_user),
            pipeline_layouts: n(hub.pipeline_layouts.num_kept_from_user),
            render_pipelines: n(hub.render_pipelines.num_kept_from_user),
            compute_pipelines: n(hub.compute_pipelines.num_kept_from_user),
            bind_groups: n(hub.bind_groups.num_kept_from_user),
            buffers: n(hub.buffers.num_kept_from_user),
            textures: n(hub.textures.num_kept_from_user),
            texture_views: n(hub.texture_views.num_kept_from_user),
            samplers: n(hub.samplers.num_kept_from_user),
        })
    }
}

/// Start-up counts at one moment, all since process start ([`StartupCounts::now`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartupCounts {
    /// Pipelines created through the engine's creation sites (`pipeline_cache::cache_for`).
    pub pipelines_created: u64,
    /// GPU creations through the tracked device (the frame-path counters, every thread).
    pub tracked: regia_render::FramePathCounters,
    /// Live wgpu objects; `None` without a render device or a registry report.
    pub gpu_live: Option<GpuLiveObjects>,
    /// File work through the platform VFS.
    pub vfs: regia_platform::VfsStats,
    /// The asset server's loads and cooked bytes read.
    pub assets: regia_asset::load_stats::AssetLoadStats,
    /// Live threads of the process (Linux and Android; `None` elsewhere).
    pub threads_live: Option<u64>,
    /// The heap ledger; `tracking` is false without the tagging allocator.
    pub heap: MemorySnapshot,
}

impl StartupCounts {
    /// The counters now. `device` gives the live GPU objects.
    #[must_use]
    pub fn now(device: Option<&regia_render::RenderDevice>) -> Self {
        Self {
            pipelines_created: regia_render::pipeline_cache::pipelines_created(),
            tracked: regia_render::frame_path_counters_all_threads(),
            gpu_live: device.and_then(|d| GpuLiveObjects::of(&d.instance)),
            vfs: regia_platform::vfs_stats(),
            assets: regia_asset::load_stats::load_stats(),
            threads_live: live_threads(),
            heap: memory::snapshot(),
        }
    }

    /// Every figure by its stable name (the answer's `figures` and the ratchet's table columns),
    /// `None` where it is not available (no device, no tagging allocator, no thread count).
    #[must_use]
    pub fn figures(&self) -> Vec<(&'static str, Option<u64>)> {
        let gpu = self.gpu_live;
        let g = |f: fn(&GpuLiveObjects) -> u64| gpu.as_ref().map(f);
        let heap = self.heap.tracking.then_some(&self.heap);
        let total = |f: fn(&memory::SubsystemMemory) -> u64| {
            heap.map(|h| h.subsystems.iter().map(|(_, m)| f(m)).sum::<u64>())
        };
        vec![
            ("pipelines_created", Some(self.pipelines_created)),
            ("gpu_live_shader_modules", g(|o| o.shader_modules)),
            ("gpu_live_bind_group_layouts", g(|o| o.bind_group_layouts)),
            ("gpu_live_pipeline_layouts", g(|o| o.pipeline_layouts)),
            ("gpu_live_render_pipelines", g(|o| o.render_pipelines)),
            ("gpu_live_compute_pipelines", g(|o| o.compute_pipelines)),
            ("gpu_live_bind_groups", g(|o| o.bind_groups)),
            ("gpu_live_buffers", g(|o| o.buffers)),
            ("gpu_live_textures", g(|o| o.textures)),
            ("gpu_live_texture_views", g(|o| o.texture_views)),
            ("gpu_live_samplers", g(|o| o.samplers)),
            ("tracked_buffers", Some(self.tracked.buffers)),
            ("tracked_buffer_bytes", Some(self.tracked.buffer_bytes)),
            ("tracked_textures", Some(self.tracked.textures)),
            ("tracked_texture_bytes", Some(self.tracked.texture_bytes)),
            ("tracked_bind_groups", Some(self.tracked.bind_groups)),
            ("tracked_upload_bytes", Some(self.tracked.upload_bytes)),
            ("tracked_submits", Some(self.tracked.submits)),
            ("vfs_opens", Some(self.vfs.opens)),
            ("vfs_failed_opens", Some(self.vfs.failed_opens)),
            ("vfs_bytes_read", Some(self.vfs.bytes_read)),
            ("vfs_writes", Some(self.vfs.writes)),
            ("vfs_bytes_written", Some(self.vfs.bytes_written)),
            ("asset_loads", Some(self.assets.loads)),
            ("asset_failed_loads", Some(self.assets.failed)),
            ("asset_files_read", Some(self.assets.files_read)),
            ("asset_bytes_read", Some(self.assets.bytes_read)),
            ("threads_live", self.threads_live),
            ("heap_allocations", total(|m| m.total_allocations)),
            ("heap_allocated_bytes", total(|m| m.total_bytes)),
            ("heap_peak_bytes", heap.map(|h| h.peak_bytes)),
            ("heap_live_bytes", heap.map(|h| h.live_bytes)),
        ]
    }
}

/// Live threads of this process: the `Threads:` line of `/proc/self/status`.
fn live_threads() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .and_then(|n| n.trim().parse().ok())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

/// The start-up snapshot [`StartupWorkPlugin`] took.
#[derive(Clone, Debug, PartialEq)]
pub struct StartupCapture {
    /// What ended the start-up window.
    pub trigger: StartupTrigger,
    /// App frames run when it was taken (1 = the first frame).
    pub frame: u64,
    /// Milliseconds from process start (informational; never budgeted).
    pub ms_since_process_start: f64,
    /// The counts.
    pub counts: StartupCounts,
}

/// The start-up snapshot, once taken (`None` before the first presented frame).
#[derive(Resource, Clone, Debug, Default)]
pub struct StartupWorkRecord {
    /// The snapshot.
    pub capture: Option<StartupCapture>,
    /// Frames seen while waiting.
    frames: u64,
    /// Boot marks seen at the last check (a new one may be `first_present`).
    marks_seen: usize,
}

/// `Last`: takes the start-up snapshot on the first frame that presented or rendered a texture
/// camera. After that it is one `Option` check per frame.
fn capture_startup_work(
    mut record: ResMut<StartupWorkRecord>,
    device: Option<Res<regia_render::RenderDevice>>,
) {
    if record.capture.is_some() {
        return;
    }
    record.frames += 1;
    let marks = regia_render::boot::mark_count();
    let presented = marks != record.marks_seen && regia_render::boot::mark_ms("first_present").is_some();
    record.marks_seen = marks;
    let trigger = if presented {
        Some(StartupTrigger::FirstPresent)
    } else if regia_render::with_texture_view_stats(|s| s.rendered > 0) {
        Some(StartupTrigger::FirstTextureView)
    } else if record.frames >= STARTUP_FRAME_LIMIT {
        Some(StartupTrigger::FrameLimit)
    } else {
        None
    };
    let Some(trigger) = trigger else { return };
    let _span = tracing::debug_span!("startup_work_capture").entered();
    let counts = StartupCounts::now(device.as_deref());
    record.capture = Some(StartupCapture {
        trigger,
        frame: record.frames,
        ms_since_process_start: regia_render::boot::since_process_start().as_secs_f64() * 1000.0,
        counts,
    });
}

/// Takes the start-up snapshot ([`StartupWorkRecord`]) and runs `memory.idle_ledger` calls.
/// [`crate::AgentPlugin`] adds it; a host without the agent server may add it on its own to read
/// [`StartupWorkRecord`] (the start-up ratchet does).
#[derive(Default)]
pub struct StartupWorkPlugin;

impl Plugin for StartupWorkPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<StartupWorkRecord>() {
            return;
        }
        app.init_resource::<StartupWorkRecord>()
            .init_resource::<IdleLedgerRuns>()
            .add_systems(Last, (capture_startup_work, advance_idle_ledger));
    }
}

fn figures_json(figures: &[(&'static str, Option<u64>)]) -> Value {
    Value::Object(
        figures
            .iter()
            .map(|(name, v)| ((*name).to_owned(), json!(v)))
            .collect::<Map<_, _>>(),
    )
}

/// `app.startup_work`: the start-up snapshot, or `captured: false` before the first presented
/// frame (and in an app without [`StartupWorkPlugin`]).
pub fn startup_work(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let capture = world
        .get_resource::<StartupWorkRecord>()
        .and_then(|r| r.capture.clone());
    let heap_tracked = memory::tracking_installed();
    let hint = if world.contains_resource::<StartupWorkRecord>() {
        "the first frame has not been presented yet; ask again after it"
    } else {
        "this app has no StartupWorkPlugin (regia_agent::AgentPlugin adds it)"
    };
    Ok(match capture {
        Some(c) => json!({
            "captured": true,
            "trigger": c.trigger.name(),
            "frame": c.frame,
            "ms_since_process_start": c.ms_since_process_start,
            "heap_tracked": c.counts.heap.tracking,
            "figures": figures_json(&c.counts.figures()),
        }),
        None => json!({
            "captured": false,
            "trigger": null,
            "frame": null,
            "ms_since_process_start": null,
            "heap_tracked": heap_tracked,
            "figures": null,
            "hint": hint,
        }),
    })
}

/// One subsystem's line of an [`IdleLedger`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleSubsystem {
    /// The subsystem.
    pub subsystem: Option<Subsystem>,
    /// Heap bytes it holds at the end of the window.
    pub live_bytes: u64,
    /// Live allocations at the end.
    pub live_allocations: u64,
    /// Change of `live_bytes` over the window.
    pub growth_bytes: i64,
    /// Allocations it made during the window (freed or not).
    pub allocations: u64,
}

/// What each subsystem holds after an idle window ([`IdleLedger::between`]).
#[derive(Clone, Debug, PartialEq)]
pub struct IdleLedger {
    /// Frames the window ran.
    pub frames: u64,
    /// Whether the tagging allocator is installed (every figure is 0 when not).
    pub tracking: bool,
    /// Heap bytes held at the end, all subsystems.
    pub live_bytes: u64,
    /// Change over the window, all subsystems.
    pub growth_bytes: i64,
    /// Allocations made during the window, all subsystems.
    pub allocations: u64,
    /// Bytes allocated during the window (growth by reallocation included).
    pub allocated_bytes: u64,
    /// Per subsystem, in [`Subsystem::ALL`] order.
    pub subsystems: Vec<IdleSubsystem>,
    /// Per named owner (ADR-3351: `render.shader_modules`, ...), largest held bytes first; empty
    /// until [`IdleLedger::with_owners`].
    pub owners: Vec<IdleOwner>,
}

/// One named heap owner's figures over an idle window ([`IdleLedger::with_owners`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleOwner {
    /// The owner's name (`regia_core::memory::MemoryOwner::name`).
    pub name: &'static str,
    /// The subsystem its bytes also count against.
    pub subsystem: Subsystem,
    /// Heap bytes it holds at the end of the window.
    pub live_bytes: u64,
    /// Live allocations at the end.
    pub live_allocations: u64,
    /// Change of `live_bytes` over the window.
    pub growth_bytes: i64,
    /// Allocations it made during the window (freed or not).
    pub allocations: u64,
}

impl IdleLedger {
    /// The ledger of a window from `start` to `end` that ran `frames` frames.
    #[must_use]
    pub fn between(start: &MemorySnapshot, end: &MemorySnapshot, frames: u64) -> Self {
        let subsystems: Vec<IdleSubsystem> = end
            .subsystems
            .iter()
            .zip(start.subsystems.iter())
            .map(|((subsystem, now), (_, then))| IdleSubsystem {
                subsystem: Some(*subsystem),
                live_bytes: now.live_bytes,
                live_allocations: now.live_allocations,
                growth_bytes: now.live_bytes as i64 - then.live_bytes as i64,
                allocations: now.total_allocations.saturating_sub(then.total_allocations),
            })
            .collect();
        let allocated_bytes = end
            .subsystems
            .iter()
            .zip(start.subsystems.iter())
            .map(|((_, now), (_, then))| now.total_bytes.saturating_sub(then.total_bytes))
            .sum();
        Self {
            frames,
            tracking: end.tracking,
            live_bytes: end.live_bytes,
            growth_bytes: end.live_bytes as i64 - start.live_bytes as i64,
            allocations: subsystems.iter().map(|s| s.allocations).sum(),
            allocated_bytes,
            subsystems,
            owners: Vec::new(),
        }
    }

    /// The same ledger with the named owners' rows from `start` to `end`, largest held first.
    #[must_use]
    pub fn with_owners(mut self, start: &OwnerSnapshot, end: &OwnerSnapshot) -> Self {
        let mut owners: Vec<IdleOwner> = end
            .iter()
            .map(|o| {
                let then = start.get(o.name).map(|b| b.memory).unwrap_or_default();
                IdleOwner {
                    name: o.name,
                    subsystem: o.subsystem,
                    live_bytes: o.memory.live_bytes,
                    live_allocations: o.memory.live_allocations,
                    growth_bytes: o.memory.live_bytes as i64 - then.live_bytes as i64,
                    allocations: o.memory.total_allocations.saturating_sub(then.total_allocations),
                }
            })
            .collect();
        owners.sort_by(|a, b| b.live_bytes.cmp(&a.live_bytes).then_with(|| a.name.cmp(b.name)));
        self.owners = owners;
        self
    }

    /// The subsystem holding the most heap bytes (`None` when nothing is held or not tracked).
    #[must_use]
    pub fn largest(&self) -> Option<&IdleSubsystem> {
        self.subsystems
            .iter()
            .filter(|s| s.live_bytes > 0)
            .max_by_key(|s| s.live_bytes)
    }

    /// The `memory.idle_ledger` answer.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let tracked = self.tracking;
        let per_frame = |n: u64| (self.frames > 0).then(|| n as f64 / self.frames as f64);
        let subsystems: Vec<Value> = self
            .subsystems
            .iter()
            .map(|s| {
                json!({
                    "name": s.subsystem.map_or("unscoped", Subsystem::name),
                    "live_bytes": tracked.then_some(s.live_bytes),
                    "live_allocations": tracked.then_some(s.live_allocations),
                    "growth_bytes": tracked.then_some(s.growth_bytes),
                    "allocations": tracked.then_some(s.allocations),
                })
            })
            .collect();
        let owners: Vec<Value> = self
            .owners
            .iter()
            .filter(|_| tracked)
            .map(|o| {
                json!({
                    "name": o.name,
                    "subsystem": o.subsystem.name(),
                    "live_bytes": o.live_bytes,
                    "live_allocations": o.live_allocations,
                    "growth_bytes": o.growth_bytes,
                    "allocations": o.allocations,
                })
            })
            .collect();
        json!({
            "frames": self.frames,
            "heap_tracked": tracked,
            "hint": (!tracked).then_some(
                "heap bytes need regia_core::memory::TaggingAllocator as the host's #[global_allocator]"
            ),
            "live_bytes": tracked.then_some(self.live_bytes),
            "growth_bytes": tracked.then_some(self.growth_bytes),
            "allocations": tracked.then_some(self.allocations),
            "allocations_per_frame": if tracked { per_frame(self.allocations) } else { None },
            "allocated_bytes_per_frame": if tracked { per_frame(self.allocated_bytes) } else { None },
            "largest": self.largest().filter(|_| tracked).and_then(|s| s.subsystem).map(Subsystem::name),
            "subsystems": subsystems,
            "largest_owner": self.owners.first().filter(|o| tracked && o.live_bytes > 0).map(|o| o.name),
            "owners": owners,
        })
    }
}

/// A `memory.idle_ledger` call waiting for its frames.
struct IdleRun {
    start: MemorySnapshot,
    owners_start: OwnerSnapshot,
    frames: u64,
    seen: u64,
    id: Value,
    reply: Sender<Response>,
}

/// The running `memory.idle_ledger` call (one at a time), advanced in `Last`.
#[derive(Resource, Default)]
pub struct IdleLedgerRuns {
    running: Option<IdleRun>,
}

/// `memory.idle_ledger`: starts the idle window; the reply comes after `frames` frames.
pub(crate) fn idle_ledger_start(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Result<(), RpcError> {
    let frames = match params.get("frames") {
        None | Some(Value::Null) => IDLE_LEDGER_DEFAULT_FRAMES,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`frames` must be a positive integer"))?,
    };
    if !(1..=IDLE_LEDGER_MAX_FRAMES).contains(&frames) {
        return Err(RpcError::invalid_params(format!(
            "`frames` must be within 1..={IDLE_LEDGER_MAX_FRAMES}"
        )));
    }
    if !world.contains_resource::<StartupWorkRecord>() {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "idle_ledger_not_installed",
            "this app has no StartupWorkPlugin, so nothing counts its frames",
            "add regia_agent::startup_idle::StartupWorkPlugin (regia_agent::AgentPlugin adds it)",
        ));
    }
    let runs = world.get_resource_or_insert_with(IdleLedgerRuns::default);
    if runs.running.is_some() {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "idle_ledger_running",
            "a memory.idle_ledger call is already waiting for its frames",
            "wait for its reply, then ask again",
        ));
    }
    world.resource_mut::<IdleLedgerRuns>().running = Some(IdleRun {
        start: memory::snapshot(),
        owners_start: memory::owner_snapshot(),
        frames,
        seen: 0,
        id,
        reply,
    });
    Ok(())
}

/// `Last`: counts the running idle window's frames and answers when they have run.
fn advance_idle_ledger(mut runs: ResMut<IdleLedgerRuns>) {
    let Some(run) = runs.running.as_mut() else {
        return;
    };
    run.seen += 1;
    if run.seen < run.frames {
        return;
    }
    let _span = tracing::debug_span!("idle_ledger_reply").entered();
    let Some(run) = runs.running.take() else {
        return;
    };
    let ledger = IdleLedger::between(&run.start, &memory::snapshot(), run.frames)
        .with_owners(&run.owners_start, &memory::owner_snapshot());
    // A client that hung up has no one to answer; nothing else to do.
    let _ = run.reply.send(Response::ok(run.id, ledger.to_json()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::memory::SubsystemMemory;

    fn ledger(scene_live: u64, scene_total: u64) -> MemorySnapshot {
        let mut heap = MemorySnapshot {
            tracking: true,
            live_bytes: scene_live + 1_000,
            peak_bytes: 9_000_000,
            ..MemorySnapshot::default()
        };
        heap.subsystems[Subsystem::Unscoped as usize].1 = SubsystemMemory {
            live_bytes: 1_000,
            peak_bytes: 1_000,
            live_allocations: 10,
            total_allocations: 10,
            total_bytes: 1_000,
        };
        heap.subsystems[Subsystem::Scene as usize].1 = SubsystemMemory {
            live_bytes: scene_live,
            peak_bytes: scene_live,
            live_allocations: scene_live / 100,
            total_allocations: scene_total,
            total_bytes: scene_total * 64,
        };
        heap
    }

    #[test]
    fn the_idle_ledger_names_the_holder_and_the_per_frame_rate() {
        let ledger = IdleLedger::between(&ledger(4_000, 100), &ledger(20_000, 340), 120);
        assert_eq!(ledger.growth_bytes, 16_000);
        assert_eq!(ledger.allocations, 240);
        let v = ledger.to_json();
        assert_eq!(v["frames"], 120);
        assert_eq!(v["largest"], "scene");
        assert_eq!(v["allocations_per_frame"], 2.0);
        assert_eq!(v["allocated_bytes_per_frame"], 128.0);
        let scene = &v["subsystems"][Subsystem::Scene as usize];
        assert_eq!(scene["name"], "scene");
        assert_eq!(scene["live_bytes"], 20_000);
        assert_eq!(scene["growth_bytes"], 16_000);
        assert_eq!(v["subsystems"].as_array().map(Vec::len), Some(Subsystem::COUNT));
    }

    #[test]
    fn the_idle_ledger_lists_named_owners_largest_first() {
        use regia_core::memory::{OwnerMemory, SubsystemMemory};
        let row = |name, live: u64, total: u64| OwnerMemory {
            name,
            subsystem: Subsystem::Render,
            memory: SubsystemMemory {
                live_bytes: live,
                live_allocations: live / 100,
                total_allocations: total,
                ..SubsystemMemory::default()
            },
        };
        let start = OwnerSnapshot::from_rows(&[row("render.shader_modules", 9_000, 50)]);
        let end = OwnerSnapshot::from_rows(&[
            row("render.shader_modules", 9_000, 50),
            row("render.frame", 12_000, 400),
        ]);
        let ledger =
            IdleLedger::between(&ledger(4_000, 100), &ledger(4_000, 100), 10).with_owners(&start, &end);
        let v = ledger.to_json();
        assert_eq!(v["largest_owner"], "render.frame");
        assert_eq!(v["owners"][0]["name"], "render.frame");
        assert_eq!(v["owners"][0]["subsystem"], "render");
        assert_eq!(v["owners"][0]["growth_bytes"], 12_000);
        assert_eq!(v["owners"][0]["allocations"], 400);
        assert_eq!(v["owners"][1]["name"], "render.shader_modules");
        assert_eq!(v["owners"][1]["growth_bytes"], 0);
        assert_eq!(v["owners"][1]["allocations"], 0);
        // Untracked: no owner rows.
        let empty = MemorySnapshot::default();
        let v = IdleLedger::between(&empty, &empty, 1)
            .with_owners(&start, &end)
            .to_json();
        assert_eq!(v["owners"].as_array().map(Vec::len), Some(0));
        assert!(v["largest_owner"].is_null());
    }

    #[test]
    fn an_untracked_heap_answers_nulls_and_a_hint() {
        let empty = MemorySnapshot::default();
        let v = IdleLedger::between(&empty, &empty, 5).to_json();
        assert_eq!(v["heap_tracked"], false);
        assert!(v["live_bytes"].is_null());
        assert!(v["largest"].is_null());
        assert!(v["hint"].as_str().is_some_and(|h| h.contains("TaggingAllocator")));
    }

    #[test]
    fn figures_have_unique_names_and_nulls_where_unavailable() {
        let counts = StartupCounts::default();
        let figures = counts.figures();
        let mut names: Vec<&str> = figures.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), figures.len(), "duplicate figure names");
        let get = |name: &str| figures.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
        assert_eq!(get("gpu_live_shader_modules"), Some(None), "no device");
        assert_eq!(get("heap_peak_bytes"), Some(None), "not tracked");
        assert_eq!(get("pipelines_created"), Some(Some(0)));
    }

    #[test]
    fn startup_capture_waits_for_a_frame_then_answers_once() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(StartupWorkPlugin);
        let before = startup_work(app.world_mut(), &json!({})).expect("answer");
        assert_eq!(before["captured"], false);
        // No window and no texture camera of its own: the frame limit ends the window, unless a
        // test on another thread of this binary rendered a texture camera first (the stats are
        // process-wide), which ends it at once.
        for _ in 0..STARTUP_FRAME_LIMIT {
            app.update();
        }
        let after = startup_work(app.world_mut(), &json!({})).expect("answer");
        assert_eq!(after["captured"], true, "{after}");
        let trigger = after["trigger"].as_str().unwrap_or("");
        assert!(
            ["frame_limit", "first_texture_view", "first_present"].contains(&trigger),
            "{after}"
        );
        if trigger == "frame_limit" {
            assert_eq!(after["frame"], STARTUP_FRAME_LIMIT);
        }
        assert!(after["figures"]["pipelines_created"].is_u64());
        assert!(after["figures"]["gpu_live_shader_modules"].is_null(), "no device");
        app.update();
        let again = startup_work(app.world_mut(), &json!({})).expect("answer");
        assert_eq!(again["frame"], after["frame"], "taken once");
    }

    #[test]
    fn idle_ledger_answers_after_its_frames_and_refuses_a_second_run() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(StartupWorkPlugin);
        let bad = |params: Value| {
            let (tx, _rx) = crossbeam_channel::bounded(1);
            let mut world = World::new();
            world.init_resource::<StartupWorkRecord>();
            idle_ledger_start(&mut world, &params, json!(0), tx).is_err()
        };
        assert!(bad(json!({ "frames": 0 })));
        assert!(bad(json!({ "frames": "many" })));
        assert!(bad(json!({ "frames": IDLE_LEDGER_MAX_FRAMES + 1 })));
        let (tx, rx) = crossbeam_channel::bounded(1);
        idle_ledger_start(app.world_mut(), &json!({ "frames": 3 }), json!(7), tx).expect("started");
        let (tx2, _rx2) = crossbeam_channel::bounded(1);
        let second = idle_ledger_start(app.world_mut(), &json!({}), json!(8), tx2);
        let code = second
            .err()
            .and_then(|e| e.data)
            .and_then(|d| d.get("code").and_then(Value::as_str).map(str::to_owned));
        assert_eq!(code.as_deref(), Some("idle_ledger_running"));
        app.update();
        app.update();
        assert!(rx.try_recv().is_err(), "two of three frames");
        app.update();
        let answer = rx.try_recv().expect("answered after the third frame");
        let result = answer.result.expect("ok");
        assert_eq!(result["frames"], 3);
        assert_eq!(
            result["subsystems"].as_array().map(Vec::len),
            Some(Subsystem::COUNT)
        );
        let mut world = World::new();
        let (tx3, _rx3) = crossbeam_channel::bounded(1);
        let missing = idle_ledger_start(&mut world, &json!({}), json!(9), tx3);
        let code = missing
            .err()
            .and_then(|e| e.data)
            .and_then(|d| d.get("code").and_then(Value::as_str).map(str::to_owned));
        assert_eq!(code.as_deref(), Some("idle_ledger_not_installed"));
    }
}
