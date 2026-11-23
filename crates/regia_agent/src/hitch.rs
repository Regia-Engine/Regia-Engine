// SPDX-License-Identifier: MPL-2.0
//! `play.hitch_report` (`specs/27` § 5): the § 2.4 hitch metrics of the running app over the
//! next `seconds`, after a warm-up — frame-time percentiles against the median, frames over
//! 2 × p50, the longest frame, CPU and GPU percentiles, the renderer's frame-path device
//! calls (pipelines, bind groups, buffers, textures, blocking polls, inline sky bakes) and the
//! § 2.4 streaming waits (texture streaming held/backing-off frames and the render gate's
//! GPU-residency waits, in `streaming_waits`).
//!
//! The frame interval is [`RealTime::raw_delta`] (what a player sees, not clamped to
//! `MAX_DELTA`), and the warm-up and window are wall-clock seconds summed from it. CPU and GPU
//! times come from
//! [`regia_render::RuntimeRenderer`]; the report turns the renderer's GPU timestamps on for
//! its duration (when the device has `TIMESTAMP_QUERY`) and back off after, so `gpu` is `null`
//! only on devices without timestamp queries. Device calls are the
//! process-wide counters, so a create on any thread shows. `creates` names them: every
//! frame-path create after the warm-up by kind and label, with its first and last measured frame
//! (ADR-2303), read from the renderer's create ring once a frame. `blocking_poll_sites` names
//! where the window's blocking polls were called (`file:line`, ADR-4050). `drive` names a scripted drive:
//! the method inserts [`HitchDrive`] for the run so a game can start it, and removes it after.
//!
//! `work` is the frame's own cost, not its presented interval (ADR-4947): per measured frame, the
//! larger of the CPU work (the display pacer's newest [`FramePacingTrace`] row: the frame's cost
//! without the time blocked in the swapchain) and the GPU frame time. A vsync-paced interval can
//! never be shorter than the panel's refresh interval, so only `work` says whether the frame fits
//! its slot. It is `null` where no display pacer fills the trace (desktop, headless).

use crossbeam_channel::Sender;
use regia_app::RealTime;
use regia_app::prelude::*;
use regia_platform::frame_pacing::FramePacingTrace;
use regia_render::{HitchRecorder, HitchReport, HitchTargets};
use serde_json::{Value, json};

use crate::protocol::{Response, RpcError};

/// Present while `play.hitch_report` runs with a `drive`: the game starts the named scripted
/// drive (camera path, cuts, effects) when it sees this, and may stop when it is removed.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct HitchDrive {
    /// The `drive` parameter.
    pub name: String,
}

/// Frame-path creates by kind and label, in order of first appearance.
#[derive(Default)]
struct CreateTally {
    /// `(kind, label, count, first measured frame, last measured frame)`.
    rows: Vec<(regia_render::FramePathKind, String, u64, usize, usize)>,
    /// Creates the renderer's ring overwrote before a frame read them (more than
    /// [`regia_render::RECENT_CREATES`] at once): counted, not named.
    dropped: u64,
    /// The newest create sequence number read.
    seq: u64,
}

impl CreateTally {
    /// Starts after every create so far (the warm-up's).
    fn starting_now() -> Self {
        Self {
            seq: regia_render::create_seq(),
            ..Self::default()
        }
    }

    /// Counts one create seen on measured frame `frame`.
    fn note(&mut self, kind: regia_render::FramePathKind, label: &str, frame: usize) {
        match self.rows.iter_mut().find(|row| row.0 == kind && row.1 == label) {
            Some(row) => {
                row.2 += 1;
                row.4 = frame;
            }
            None => self.rows.push((kind, label.to_owned(), 1, frame, frame)),
        }
    }

    /// Reads the creates since the last call into measured frame `frame`. Never allocates
    /// unless a new label appears.
    fn read(&mut self, frame: usize) {
        let mut ring = regia_render::create_buffer();
        let n = regia_render::recent_creates(self.seq, &mut ring);
        for record in &ring[..n] {
            // Sequence numbers are consecutive: a gap is what the ring overwrote.
            self.dropped += record.seq.saturating_sub(self.seq.saturating_add(1));
            self.seq = record.seq;
            self.note(record.kind, record.label(), frame);
        }
    }

    fn json(&self) -> Value {
        Value::Array(
            self.rows
                .iter()
                .map(|(kind, label, count, first, last)| {
                    json!({
                        "kind": kind_name(*kind),
                        "label": label,
                        "count": count,
                        "first_frame": first,
                        "last_frame": last,
                    })
                })
                .collect(),
        )
    }
}

/// `blocking_poll_sites`: where the measured window's blocking polls were called, most first
/// (ADR-4050). `site` is `file:line` of the `TrackedDevice::poll` call.
fn poll_sites_json(polls: &regia_render::BlockingPollSites) -> Value {
    let mut sites = polls.sites.clone();
    sites.sort_by_key(|s| std::cmp::Reverse(s.count));
    Value::Array(
        sites
            .iter()
            .map(|s| json!({ "site": format!("{}:{}", s.file, s.line), "count": s.count }))
            .collect(),
    )
}

/// The `kind` of a `creates` row.
fn kind_name(kind: regia_render::FramePathKind) -> &'static str {
    match kind {
        regia_render::FramePathKind::Pipeline => "pipeline",
        regia_render::FramePathKind::BindGroup => "bind_group",
        regia_render::FramePathKind::Buffer => "buffer",
        regia_render::FramePathKind::Texture => "texture",
        regia_render::FramePathKind::BlockingPoll => "blocking_poll",
    }
}

/// One running report.
struct Session {
    /// Exclude the interval that ended before this request was accepted.
    initial_elapsed: Option<std::time::Duration>,
    /// Wall-clock seconds since the start, summed from the unclamped frame times (the clamped
    /// `RealTime::elapsed` runs slow on a phone at 600 ms a frame, ADR-1749).
    elapsed_s: f64,
    warmup_s: f64,
    seconds: f64,
    /// Frames recorded when the warm-up ended (`None` while it runs).
    warmup_frames: Option<usize>,
    /// The creates after the warm-up (`None` while it runs).
    creates: Option<CreateTally>,
    /// Blocking-poll call sites when the warm-up ended (`None` while it runs, ADR-4050).
    poll_sites: Option<regia_render::BlockingPollSites>,
    recorder: HitchRecorder,
    /// Per recorded frame: its CPU work in ms from the display pacer's trace, `None` when no new
    /// trace row arrived for it (ADR-4947). Index matches `recorder.frames()`.
    cpu_work: Vec<Option<f32>>,
    /// `start_ns` of the newest trace row already counted.
    last_trace_start: Option<u64>,
    /// The residency keeper's counters when the run started (`None` without a keeper).
    residency_start: Option<regia_core::residency::ResidencyStats>,
    /// The stream render gate's wait counters when the run started (`None` without the render
    /// plugin): deltas over the window are the § 2.4 streaming waits.
    stream_residency_start: Option<(u64, u64)>,
    /// Measured-window frames texture streaming ran held / backing off (ADR-2317, the § 2.4
    /// "no frame waits on a load" row of the hitch table).
    texture_held_frames: u64,
    texture_backing_off_frames: u64,
    /// Frames counted into `texture_*_frames` (the measured window).
    streaming_frames: u64,
    drive: Option<String>,
    /// The report switched GPU timing on and switches it off when done.
    timing_was_off: bool,
    id: Value,
    reply: Sender<Response>,
}

/// Turns the runtime renderer's GPU timing on or off; returns whether it was on before
/// (`None` without a renderer or device).
fn set_runtime_gpu_timing(world: &mut World, enabled: bool) -> Option<bool> {
    if !world.contains_resource::<regia_render::RenderDevice>()
        || !world.contains_resource::<regia_render::RuntimeRenderer>()
    {
        return None;
    }
    world.resource_scope(|world, mut renderer: Mut<regia_render::RuntimeRenderer>| {
        let device = world.get_resource::<regia_render::RenderDevice>()?;
        let was = renderer.0.gpu_timing_enabled();
        renderer.0.set_gpu_timing(device, enabled);
        Some(was)
    })
}

/// The running `play.hitch_report`, advanced once per frame in `Last` by the agent plugin.
#[derive(Resource, Default)]
pub struct HitchSessions {
    running: Option<Session>,
}

/// `play.hitch_report`: starts measuring; the reply comes when `seconds` have passed.
pub(crate) fn start(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Result<(), RpcError> {
    let number = |name: &str, default: f64| -> Result<f64, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number"))),
        }
    };
    let seconds = number("seconds", 10.0)?;
    if !(0.1..=600.0).contains(&seconds) {
        return Err(RpcError::invalid_params("`seconds` must be within 0.1..=600"));
    }
    let warmup_s = number("warmup_s", 2.0)?.clamp(0.0, 60.0);
    let drive = match params.get("drive") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(name.clone()),
        Some(_) => return Err(RpcError::invalid_params("`drive` must be a string")),
    };
    let residency_start = world
        .get_resource::<regia_app::Residency>()
        .map(regia_app::Residency::stats);
    let stream_residency_start = world
        .get_resource::<regia_render::StreamRenderResidency>()
        .map(|residency| (residency.waits, residency.timeouts));
    let sessions = world.get_resource_or_insert_with(HitchSessions::default);
    if sessions.running.is_some() {
        return Err(RpcError::new(
            crate::protocol::INVALID_REQUEST,
            "F0017",
            "a hitch report is already running",
            "wait for its reply, then ask again",
        ));
    }
    // Before the recorder exists, so the timer's query set and buffers are not counted as
    // frame-path creates.
    let timing_was_off = set_runtime_gpu_timing(world, true) == Some(false);
    let initial_elapsed = world.get_resource::<RealTime>().map(RealTime::elapsed);
    world.resource_mut::<HitchSessions>().running = Some(Session {
        initial_elapsed,
        elapsed_s: 0.0,
        warmup_s,
        seconds,
        warmup_frames: None,
        creates: None,
        poll_sites: None,
        recorder: HitchRecorder::new(),
        cpu_work: Vec::new(),
        last_trace_start: world
            .get_resource::<FramePacingTrace>()
            .and_then(|t| t.rows().next_back())
            .map(|row| row.start_ns),
        residency_start,
        stream_residency_start,
        texture_held_frames: 0,
        texture_backing_off_frames: 0,
        streaming_frames: 0,
        drive: drive.clone(),
        timing_was_off,
        id,
        reply,
    });
    if let Some(name) = drive {
        world.insert_resource(HitchDrive { name });
    }
    Ok(())
}

/// This frame's wall-clock time in ms: `RealTime::raw_delta`, not the delta clamped to
/// `MAX_DELTA` (250 ms), so a phone at 600 ms a frame reports 600 ms and not 250 (ADR-1749).
fn frame_ms(world: &World) -> f32 {
    world
        .get_resource::<RealTime>()
        .map(|t| t.raw_delta().as_secs_f32() * 1000.0)
        .or_else(|| world.get_resource::<Time>().map(|t| t.delta_secs() * 1000.0))
        .unwrap_or(0.0)
}

/// `Last`: records this frame into the running report and answers when it is done.
pub fn advance_hitch_report(world: &mut World) {
    if world
        .get_resource::<HitchSessions>()
        .is_none_or(|s| s.running.is_none())
    {
        return;
    }
    let elapsed_at_frame = world.get_resource::<RealTime>().map(RealTime::elapsed);
    let frame_ms = frame_ms(world);
    let (cpu_ms, gpu_ms) = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map_or((0.0, 0.0), |r| {
            (r.0.cpu_timings().total_ms, r.0.gpu_timings().frame_ms)
        });
    // § 2.4 streaming waits (reported, not judged — see `streaming_waits_json`): texture
    // streaming held/backing-off this frame (memory-pressure signals, counted over the measured
    // window); the render gate's residency wait counters are differenced over the same window.
    let (texture_held, texture_backing_off) = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map_or((false, false), |r| {
            (r.0.texture_streaming_held(), r.0.texture_streaming_backing_off())
        });
    // Captured every frame; the sample kept is the one from the frame the warm-up completes, so
    // the render-residency deltas cover the measured window like the texture counters.
    let stream_residency_now = world
        .get_resource::<regia_render::StreamRenderResidency>()
        .map(|residency| (residency.waits, residency.timeouts));
    let newest_row = world
        .get_resource::<FramePacingTrace>()
        .and_then(|t| t.rows().next_back().copied());
    let mut sessions = world.resource_mut::<HitchSessions>();
    let Some(session) = sessions.running.as_mut() else {
        return;
    };
    if session.initial_elapsed.is_some() && session.initial_elapsed == elapsed_at_frame {
        return;
    }
    session.initial_elapsed = None;
    session.recorder.frame(frame_ms, cpu_ms, gpu_ms);
    // The pacer pushes a frame's row after the frame's update, so the newest row is the frame
    // whose interval `frame_ms` closes. A row already counted is not counted twice.
    let work = newest_row
        .filter(|row| session.last_trace_start != Some(row.start_ns))
        .map(|row| {
            session.last_trace_start = Some(row.start_ns);
            row.cost_ns.saturating_sub(row.swapchain_wait_ns) as f32 / 1e6
        });
    // The recorder's first call only primes its counters and records no frame: keep one entry
    // per recorded frame so the warm-up index means the same frame in both.
    if session.cpu_work.len() < session.recorder.frames().len() {
        session.cpu_work.push(work);
    }
    session.elapsed_s += f64::from(frame_ms) / 1000.0;
    let elapsed = session.elapsed_s;
    if let (Some(warmup), Some(creates)) = (session.warmup_frames, session.creates.as_mut()) {
        creates.read(session.recorder.frames().len().saturating_sub(warmup + 1));
        session.streaming_frames += 1;
        if texture_held {
            session.texture_held_frames += 1;
        }
        if texture_backing_off {
            session.texture_backing_off_frames += 1;
        }
    }
    if session.warmup_frames.is_none() && elapsed >= session.warmup_s {
        session.warmup_frames = Some(session.recorder.frames().len());
        session.creates = Some(CreateTally::starting_now());
        session.poll_sites = Some(regia_render::blocking_poll_sites());
        // The measured window starts here: the residency deltas must exclude the warm-up like
        // the texture-held counters do.
        session.stream_residency_start = stream_residency_now;
    }
    if elapsed < session.warmup_s + session.seconds {
        return;
    }
    let Some(session) = sessions.running.take() else {
        return;
    };
    let warmup = session.warmup_frames.unwrap_or(0);
    let report = session.recorder.report(warmup, HitchTargets::SPEC);
    let residency = world
        .get_resource::<regia_app::Residency>()
        .map(regia_app::Residency::stats);
    let mut body = report_json(&report, &session);
    let creates = session.creates.as_ref();
    body["creates"] = creates.map_or_else(|| Value::Array(Vec::new()), CreateTally::json);
    body["creates_dropped"] = json!(creates.map_or(0, |c| c.dropped));
    let polls = session
        .poll_sites
        .as_ref()
        .map(|start| regia_render::blocking_poll_sites().since(start))
        .unwrap_or_default();
    body["blocking_poll_sites"] = poll_sites_json(&polls);
    body["blocking_poll_sites_unnamed"] = json!(polls.unnamed);
    body["bound"] = bound_json(session.recorder.frames().get(warmup..).unwrap_or(&[]));
    body["work"] = work_json(
        session.recorder.frames().get(warmup..).unwrap_or(&[]),
        session.cpu_work.get(warmup..).unwrap_or(&[]),
    );
    // The last frame's GPU passes: where the GPU time goes (null without timestamps).
    body["gpu_passes"] = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| r.0.gpu_timings())
        .filter(|g| g.frame_ms > 0.0)
        .map_or(Value::Null, |g| {
            json!({
                "shadows_ms": g.shadows_ms,
                "clusters_ms": g.clusters_ms,
                "gi_ms": g.gi_ms,
                "prepass_ms": g.prepass_ms,
                "forward_ms": g.forward_ms,
                "cull_ms": g.cull_ms,
                "ssao_ms": g.ssao_ms,
                "contact_shadows_ms": g.contact_shadows_ms,
                "ssgi_ms": g.ssgi_ms,
                "ssr_ms": g.ssr_ms,
                "taau_ms": g.taau_ms,
                "atmosphere_ms": g.atmosphere_ms,
                "fog_ms": g.fog_ms,
                "scene_ms": g.scene_ms,
                "post_ms": g.post_ms,
                "frame_ms": g.frame_ms,
                "nodes": gpu_nodes_json(&g),
                "stamps_outside_nodes": stamps_outside_json(&g),
            })
        });
    body["residency"] = residency_json(session.residency_start, residency);
    body["streaming_waits"] = streaming_waits_json(
        session.streaming_frames,
        session.texture_held_frames,
        session.texture_backing_off_frames,
        session.stream_residency_start,
        world,
    );
    let _ = session.reply.send(Response::ok(session.id.clone(), body));
    if session.drive.is_some() {
        world.remove_resource::<HitchDrive>();
    }
    if session.timing_was_off {
        set_runtime_gpu_timing(world, false);
    }
}

/// The sample's GPU-timed passes named after the frame-graph nodes that record them, in graph
/// order (ADR-5386): `[{ node, pass, ms }]`.
fn gpu_nodes_json(g: &regia_render::GpuTimings) -> Value {
    Value::Array(
        g.node_passes()
            .iter()
            .map(|p| json!({ "node": p.node, "pass": p.pass, "ms": p.ms }))
            .collect(),
    )
}

/// The sample's stamps written outside their node's record site (ADR-5386; empty when every
/// pass time is its node's own): `[{ query, owner, written_in }]`.
fn stamps_outside_json(g: &regia_render::GpuTimings) -> Value {
    Value::Array(
        g.stamps()
            .iter()
            .filter(|s| !s.in_owner)
            .map(|s| json!({ "query": s.query, "owner": s.owner, "written_in": s.written_in }))
            .collect(),
    )
}

/// § 2.4 streaming context over the measured window. This is **not** what the § 2.4 "Streaming"
/// row's words name ("no frame waits on a load"): what the renderer exposes today is memory
/// pressure, not a per-frame upload budget —
///
/// - `texture_held_frames`: frames the texture streamer held back mip uploads, within
///   `MIP_HOLD` of its last out-of-memory signal (`gpu_health::texture_mips_held`).
/// - `texture_backing_off_frames`: frames the streamer backed off after a streamed texture
///   failed to allocate.
/// - `render_residency`: deltas of the stream render gate's GPU-residency hold counters
///   (`StreamRenderResidency::{waits, timeouts}`) over the measured window (captured when the
///   warm-up completes, like the texture counters), plus the cells held back now. `null`
///   without the render plugin.
///
/// These counters are reported for context; they do not feed `pass`/`failures` — the § 2.4
/// streaming row has no numeric target yet, so nothing judges it. When any of them is nonzero,
/// the actionable knobs are texture-memory headroom (pool size, quality) and prewarming, not an
/// upload budget.
fn streaming_waits_json(
    frames: u64,
    texture_held_frames: u64,
    texture_backing_off_frames: u64,
    stream_residency_start: Option<(u64, u64)>,
    world: &World,
) -> Value {
    let mut body = json!({
        "frames": frames,
        "texture_held_frames": texture_held_frames,
        "texture_backing_off_frames": texture_backing_off_frames,
    });
    if let (Some((waits0, timeouts0)), Some(residency)) = (
        stream_residency_start,
        world.get_resource::<regia_render::StreamRenderResidency>(),
    ) {
        body["render_residency"] = json!({
            "on": residency.enabled,
            "waits": residency.waits.saturating_sub(waits0),
            "timeouts": residency.timeouts.saturating_sub(timeouts0),
            "waiting_cells": residency.waiting_cells,
        });
    } else {
        body["render_residency"] = Value::Null;
    }
    body
}

/// The keeper's work over the run: pages it found paged out and read back (each a major fault
/// a frame did not take), what it covers and its sweep cost. `null` without a keeper.
fn residency_json(
    start: Option<regia_core::residency::ResidencyStats>,
    end: Option<regia_core::residency::ResidencyStats>,
) -> Value {
    let Some(end) = end else {
        return Value::Null;
    };
    let start = start.unwrap_or_default();
    json!({
        "data_bytes": end.data_bytes,
        "code_bytes": end.code_bytes,
        "pages": end.pages,
        "sweeps": end.sweeps.saturating_sub(start.sweeps),
        "restored_pages": end.restored_pages.saturating_sub(start.restored_pages),
        "last_sweep_us": end.last_sweep_us,
        "max_sweep_us": end.max_sweep_us,
    })
}

/// What limited the measured frames: each frame's GPU time against its interval
/// ([`regia_telemetry::FrameBound::classify`]), counted per class, plus the class most frames
/// fell in (`unknown` without GPU timing, `mixed` when no class has a strict majority of the
/// timed frames). Tells a hitch hunter whether to look at the CPU or the GPU first.
fn bound_json(frames: &[regia_render::HitchFrame]) -> Value {
    use regia_telemetry::FrameBound;
    let (mut cpu, mut gpu, mut balanced, mut unknown) = (0u64, 0u64, 0u64, 0u64);
    for f in frames {
        match FrameBound::classify(f.frame_ms, f.gpu_ms) {
            FrameBound::CpuBound => cpu += 1,
            FrameBound::GpuBound => gpu += 1,
            FrameBound::Balanced => balanced += 1,
            FrameBound::Unknown => unknown += 1,
        }
    }
    let timed = cpu + gpu + balanced;
    let verdict = if timed == 0 {
        FrameBound::Unknown.name()
    } else if cpu * 2 > timed {
        FrameBound::CpuBound.name()
    } else if gpu * 2 > timed {
        FrameBound::GpuBound.name()
    } else if balanced * 2 > timed {
        FrameBound::Balanced.name()
    } else {
        "mixed"
    };
    json!({
        "verdict": verdict,
        "cpu_bound_frames": cpu,
        "gpu_bound_frames": gpu,
        "balanced_frames": balanced,
        "unknown_frames": unknown,
        "gpu_bound_pct": if timed == 0 { Value::Null } else { json!(gpu as f64 * 100.0 / timed as f64) },
        "cpu_bound_pct": if timed == 0 { Value::Null } else { json!(cpu as f64 * 100.0 / timed as f64) },
    })
}

/// `work` (ADR-4947): per frame with a pacer row, the larger of its CPU work and its GPU frame
/// time, as p50 / p99 over the measured frames; `null` when no measured frame has a pacer row.
/// GPU timestamps resolve a frame or two late, so a frame pairs its CPU work with the newest
/// resolved GPU time.
fn work_json(frames: &[regia_render::HitchFrame], cpu_work: &[Option<f32>]) -> Value {
    let mut work = Vec::with_capacity(cpu_work.len());
    let mut cpu = Vec::with_capacity(cpu_work.len());
    let mut gpu_timed = 0u64;
    for (frame, cpu_ms) in frames.iter().zip(cpu_work) {
        let Some(cpu_ms) = *cpu_ms else {
            continue;
        };
        if frame.gpu_ms > 0.0 {
            gpu_timed += 1;
        }
        cpu.push(cpu_ms);
        work.push(cpu_ms.max(frame.gpu_ms));
    }
    if work.is_empty() {
        return Value::Null;
    }
    work.sort_by(f32::total_cmp);
    cpu.sort_by(f32::total_cmp);
    let pct = |v: &[f32], p: f32| f64::from(regia_render::hitch::percentile_sorted(v, p));
    json!({
        "p50_ms": pct(&work, 0.5),
        "p99_ms": pct(&work, 0.99),
        "longest_ms": pct(&work, 1.0),
        "cpu_p50_ms": pct(&cpu, 0.5),
        "cpu_p99_ms": pct(&cpu, 0.99),
        "frames": work.len(),
        "gpu_timed_frames": gpu_timed,
        "source": "pacing_trace",
    })
}

fn report_json(report: &HitchReport, session: &Session) -> Value {
    let p50 = f64::from(report.p50_ms).max(1e-6);
    let c = report.counters;
    let gpu = (report.gpu_p50_ms > 0.0)
        .then(|| json!({ "p50_ms": report.gpu_p50_ms, "p99_ms": report.gpu_p99_ms }));
    let cpu = (report.cpu_p50_ms > 0.0)
        .then(|| json!({ "p50_ms": report.cpu_p50_ms, "p99_ms": report.cpu_p99_ms }));
    let failures = report.failures();
    json!({
        "seconds": session.seconds,
        "warmup_s": session.warmup_s,
        "drive": session.drive,
        "frames": report.frames,
        "p50_ms": report.p50_ms,
        "p99_ms": report.p99_ms,
        "p999_ms": report.p999_ms,
        "p99_over_p50": f64::from(report.p99_ms) / p50,
        "p999_over_p50": f64::from(report.p999_ms) / p50,
        "longest_ms": report.longest_ms,
        "longest_frame": report.longest_frame,
        "longest_over_p50": f64::from(report.longest_ms) / p50,
        "over_2x_p50": report.over_2x,
        "over_2x_p50_pct": report.over_2x as f64 * 100.0 / report.frames.max(1) as f64,
        "over_50ms": report.over_max,
        "cpu": cpu,
        "gpu": gpu,
        "device_calls": {
            "pipelines": c.pipelines,
            "bind_groups": c.bind_groups,
            "blocking_polls": c.blocking_polls,
            "buffers": c.buffers,
            "buffer_bytes": c.buffer_bytes,
            "textures": c.textures,
            "texture_bytes": c.texture_bytes,
            "upload_bytes": c.upload_bytes,
            "map_asyncs": c.map_asyncs,
            "submits": c.submits,
            "inline_sky_bakes": c.inline_bakes,
        },
        "submits_per_frame": c.submits as f64 / report.frames.max(1) as f64,
        "frames_with_creates": report.frames_with_creates,
        "spikes": report.spikes.iter().map(|s| json!({
            "frame": s.frame,
            "frame_ms": s.frame_ms,
            "cpu_ms": s.cpu_ms,
            "gpu_ms": s.gpu_ms,
            "bound": regia_telemetry::FrameBound::classify(s.frame_ms, s.gpu_ms).name(),
            "pipelines": s.counters.pipelines,
            "bind_groups": s.counters.bind_groups,
            "buffers": s.counters.buffers,
            "textures": s.counters.textures,
            "inline_sky_bakes": s.counters.inline_bakes,
        })).collect::<Vec<_>>(),
        "targets": {
            "over_2x_p50_pct": report.targets.over_2x_fraction * 100.0,
            "p99_over_p50": report.targets.p99_ratio,
            "p999_over_p50": report.targets.p999_ratio,
            "longest_over_p50": report.targets.longest_ratio,
            "max_frame_ms": report.targets.max_frame_ms,
        },
        "pass": failures.is_empty(),
        "failures": failures,
        "summary": report.summary(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.init_resource::<HitchSessions>()
            .add_systems(Last, advance_hitch_report);
        app
    }

    #[test]
    fn steady_frames_report_after_warmup_and_drive_is_scoped() {
        let mut app = app();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            app.world_mut(),
            &json!({ "seconds": 1.0, "warmup_s": 0.5, "drive": "chase" }),
            json!(7),
            tx,
        )
        .expect("starts");
        assert!(app.world().contains_resource::<HitchDrive>());
        let mut answer = None;
        for _ in 0..200 {
            app.update();
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let response = answer.expect("answers within 200 frames");
        let body = serde_json::to_value(&response).expect("json");
        let result = &body["result"];
        // 16 ms frames: 1 s measured after a 0.5 s warm-up is ~62 frames, all at p50.
        let frames = result["frames"].as_u64().expect("frames");
        assert!((55..=70).contains(&frames), "{result}");
        assert!(
            (result["p50_ms"].as_f64().expect("p50") - 16.0).abs() < 0.5,
            "{result}"
        );
        assert_eq!(result["over_2x_p50"], 0);
        assert_eq!(result["drive"], "chase");
        assert!(result["targets"]["p99_over_p50"].as_f64().is_some());
        assert!(!app.world().contains_resource::<HitchDrive>());
    }

    /// Specs/27 § 2.4: `streaming waits` — the frames of the window the render gate held for GPU
    /// residency and texture streaming ran held or backing off. Without a renderer the counters
    /// are zero and the render-residency block is null.
    #[test]
    fn the_report_counts_streaming_waits() {
        let mut app = app();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            app.world_mut(),
            &json!({ "seconds": 0.5, "warmup_s": 0.1 }),
            json!(9),
            tx,
        )
        .expect("starts");
        let mut answer = None;
        for _ in 0..100 {
            app.update();
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let body = serde_json::to_value(answer.expect("answers")).expect("json");
        let result = &body["result"];
        let waits = &result["streaming_waits"];
        assert!(
            waits["frames"].as_u64().is_some_and(|frames| frames > 0),
            "per measured frame: {waits}"
        );
        assert_eq!(waits["texture_held_frames"], 0);
        assert_eq!(waits["texture_backing_off_frames"], 0);
        assert!(waits["render_residency"].is_null(), "no render plugin: {waits}");
    }

    /// Specs/27 § 2.4: `render_residency` reports the deltas of the stream render gate's
    /// residency counters over the measured window: bumps during the warm-up are differenced
    /// out, only the increments while measuring are reported.
    #[test]
    fn render_residency_deltas_cover_the_measured_window() {
        use regia_render::StreamRenderResidency;
        let mut app = app();
        app.insert_resource(StreamRenderResidency {
            enabled: true,
            waits: 1_000,
            timeouts: 40,
            waiting_cells: 0,
        });
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            app.world_mut(),
            &json!({ "seconds": 0.3, "warmup_s": 0.1 }),
            json!(11),
            tx,
        )
        .expect("starts");
        // Still inside the 0.1 s warm-up: these bumps must not reach the deltas.
        for _ in 0..4 {
            app.update();
        }
        {
            let mut residency = app.world_mut().resource_mut::<StreamRenderResidency>();
            residency.waits += 3;
            residency.timeouts += 2;
        }
        // Into the measured window.
        for _ in 0..6 {
            app.update();
        }
        {
            let mut residency = app.world_mut().resource_mut::<StreamRenderResidency>();
            residency.waits += 5;
            residency.timeouts += 1;
        }
        let mut answer = None;
        for _ in 0..100 {
            app.update();
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let body = serde_json::to_value(answer.expect("answers")).expect("json");
        let residency = &body["result"]["streaming_waits"]["render_residency"];
        assert_eq!(
            residency["waits"], 5,
            "the warm-up bump is differenced out: {residency}"
        );
        assert_eq!(residency["timeouts"], 1, "{residency}");
    }

    /// Dispatcher e2e: `play.hitch_report` through `process_requests` (envelope, permission and
    /// contract gates included) answers with the `streaming_waits` block.
    #[test]
    fn hitch_report_through_the_dispatcher_reports_streaming_waits() {
        use crate::dispatch::{AgentInbox, RateLimit, process_requests};
        use crate::protocol::Request;
        use crate::safety::AgentSession;
        use crate::server::Inbound;

        let mut app = app();
        app.init_resource::<AgentSession>().init_resource::<RateLimit>();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(5)),
                    method: "play.hitch_report".into(),
                    params: json!({ "seconds": 0.5, "warmup_s": 0.1 }),
                },
                reply: tx,
            })
            .expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        process_requests(app.world_mut());
        drop(inbox_tx);
        let mut answer = None;
        for _ in 0..100 {
            app.update();
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let body = serde_json::to_value(answer.expect("answers")).expect("json");
        assert!(body["result"]["frames"].as_u64().is_some_and(|frames| frames > 0));
        assert!(
            body["result"]["streaming_waits"]["frames"].is_u64(),
            "streaming waits reported: {}",
            body["result"]
        );
    }

    #[test]
    fn create_tally_names_each_label_once_with_its_frames() {
        use regia_render::FramePathKind;
        let mut tally = CreateTally::default();
        tally.note(FramePathKind::BindGroup, "post bg", 3);
        tally.note(FramePathKind::BindGroup, "hist bg", 3);
        tally.note(FramePathKind::BindGroup, "post bg", 9);
        tally.note(FramePathKind::Texture, "post bg", 9);
        let rows = tally.json();
        assert_eq!(
            rows,
            json!([
                { "kind": "bind_group", "label": "post bg", "count": 2, "first_frame": 3, "last_frame": 9 },
                { "kind": "bind_group", "label": "hist bg", "count": 1, "first_frame": 3, "last_frame": 3 },
                { "kind": "texture", "label": "post bg", "count": 1, "first_frame": 9, "last_frame": 9 },
            ])
        );
    }

    /// ADR-2303: a bind group created through the renderer's tracked device after the warm-up
    /// is named in `creates` (lavapipe). The create ring is process-wide: if parallel tests
    /// overflowed it in that frame, the create is counted in `creates_dropped` instead.
    #[test]
    fn the_report_names_a_create_after_the_warmup() {
        let device = match regia_render::RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
        };
        let tracked = regia_render::TrackedDevice::new(device.device.clone());
        let layout = device
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("hitch report test bgl"),
                entries: &[],
            });
        let mut app = app();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            app.world_mut(),
            &json!({ "seconds": 0.5, "warmup_s": 0.1 }),
            json!(3),
            tx,
        )
        .expect("starts");
        let mut answer = None;
        for frame in 0..100 {
            if frame == 15 {
                let _group = tracked.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("hitch report test bg"),
                    layout: &layout,
                    entries: &[],
                });
            }
            app.update();
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let body = serde_json::to_value(answer.expect("answers")).expect("json");
        let result = &body["result"];
        let named = result["creates"]
            .as_array()
            .expect("creates")
            .iter()
            .any(|row| row["label"] == "hitch report test bg" && row["kind"] == "bind_group");
        let dropped = result["creates_dropped"].as_u64().expect("creates_dropped");
        assert!(named || dropped > 0, "{result}");
        assert!(
            result["device_calls"]["bind_groups"]
                .as_u64()
                .expect("bind_groups")
                >= 1
        );
    }

    /// ADR-4050: a blocking poll after the warm-up is named by its call site in
    /// `blocking_poll_sites` (lavapipe). The site table is process-wide: if parallel tests filled
    /// it first, the wait is counted in `blocking_poll_sites_unnamed` instead.
    #[test]
    fn the_report_names_a_blocking_poll_by_its_call_site() {
        let device = match regia_render::RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
        };
        let tracked = regia_render::TrackedDevice::new(device.device.clone());
        let mut app = app();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            app.world_mut(),
            &json!({ "seconds": 0.5, "warmup_s": 0.1 }),
            json!(4),
            tx,
        )
        .expect("starts");
        let mut answer = None;
        let mut site = String::new();
        for frame in 0..100 {
            if frame == 15 {
                site = format!("{}:{}", file!(), line!() + 1);
                let _ = tracked.poll(wgpu::PollType::wait_indefinitely());
            }
            app.update();
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let body = serde_json::to_value(answer.expect("answers")).expect("json");
        let result = &body["result"];
        let named = result["blocking_poll_sites"]
            .as_array()
            .expect("blocking_poll_sites")
            .iter()
            .any(|row| row["site"] == site.as_str() && row["count"].as_u64() >= Some(1));
        let unnamed = result["blocking_poll_sites_unnamed"].as_u64().expect("unnamed");
        assert!(named || unnamed > 0, "{site} not in {result}");
        assert!(
            result["device_calls"]["blocking_polls"]
                .as_u64()
                .expect("blocking_polls")
                >= 1
        );
    }

    /// ADR-1749: a frame slower than `MAX_DELTA` is reported at its real length (the Pixel 6
    /// showed every frame at exactly 250 ms while the GPU took 330-645 ms).
    #[test]
    fn frames_slower_than_the_delta_clamp_report_their_real_time() {
        let mut world = World::new();
        let mut time = RealTime::default();
        let start = std::time::Instant::now();
        time.tick(start);
        time.tick(start + Duration::from_millis(600));
        assert_eq!(
            time.delta(),
            regia_app::MAX_DELTA,
            "the gameplay delta stays clamped"
        );
        world.insert_resource(time);
        assert!((frame_ms(&world) - 600.0).abs() < 0.01, "{}", frame_ms(&world));
    }

    /// ADR-1749: the window is wall-clock time. At 600 ms a frame a 3 s report takes 5 frames;
    /// on the clamped clock (250 ms a frame) it took 12 and outlived the client's timeout.
    #[test]
    fn the_window_runs_on_wall_clock_time_at_slow_frame_rates() {
        let mut world = World::new();
        world.init_resource::<HitchSessions>();
        let start_at = std::time::Instant::now();
        let mut time = RealTime::default();
        time.tick(start_at);
        world.insert_resource(time);
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            &mut world,
            &json!({ "seconds": 3.0, "warmup_s": 0.0 }),
            json!(1),
            tx,
        )
        .expect("starts");
        let mut frames = 0;
        for n in 1..=20u32 {
            world
                .resource_mut::<RealTime>()
                .tick(start_at + Duration::from_millis(600) * n);
            advance_hitch_report(&mut world);
            frames = n;
            if rx.try_recv().is_ok() {
                break;
            }
        }
        assert_eq!(frames, 5, "3 s at 600 ms a frame");
    }

    /// ADR-4947: on a 60 Hz panel every presented interval is ~16.67 ms whatever the frame
    /// costs; `work` reads the pacer's per-frame cost without the swapchain wait instead, and
    /// counts each pacer row once.
    #[test]
    fn work_reads_the_pacer_cost_not_the_presented_interval() {
        use regia_platform::frame_pacing::PacingTraceRow;
        let mut world = World::new();
        world.init_resource::<HitchSessions>();
        world.init_resource::<FramePacingTrace>();
        // A row from before the request is not part of the window.
        world.resource_mut::<FramePacingTrace>().push(PacingTraceRow {
            start_ns: 1,
            cost_ns: 90_000_000,
            ..PacingTraceRow::default()
        });
        let start_at = std::time::Instant::now();
        let mut time = RealTime::default();
        time.tick(start_at);
        world.insert_resource(time);
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            &mut world,
            &json!({ "seconds": 1.0, "warmup_s": 0.0 }),
            json!(1),
            tx,
        )
        .expect("starts");
        let interval = Duration::from_micros(16_667);
        let mut answer = None;
        let mut last = 0;
        for n in 1..=200u32 {
            last = n;
            // Work 10 ms on most frames, 13 ms on every tenth; the swapchain blocks 5 ms more.
            // Every fourth frame the pacer pushes no new row.
            if !n.is_multiple_of(4) {
                let work_ns = if n.is_multiple_of(10) {
                    13_000_000
                } else {
                    10_000_000
                };
                world.resource_mut::<FramePacingTrace>().push(PacingTraceRow {
                    start_ns: 1_000 + u64::from(n),
                    cost_ns: work_ns + 5_000_000,
                    swapchain_wait_ns: 5_000_000,
                    ..PacingTraceRow::default()
                });
            }
            world.resource_mut::<RealTime>().tick(start_at + interval * n);
            advance_hitch_report(&mut world);
            if let Ok(response) = rx.try_recv() {
                answer = Some(response);
                break;
            }
        }
        let body = serde_json::to_value(answer.expect("answers")).expect("json");
        let result = &body["result"];
        let p99 = result["p99_ms"].as_f64().expect("p99");
        assert!((p99 - 16.667).abs() < 0.01, "presented interval {p99}");
        let work = &result["work"];
        assert_eq!(work["source"], "pacing_trace");
        assert!(
            (work["p50_ms"].as_f64().expect("p50") - 10.0).abs() < 1e-3,
            "{work}"
        );
        assert!(
            (work["p99_ms"].as_f64().expect("p99") - 13.0).abs() < 1e-3,
            "{work}"
        );
        assert!(
            (work["longest_ms"].as_f64().expect("longest") - 13.0).abs() < 1e-3,
            "{work}"
        );
        // The recorder primes on the first frame and records from the second; each recorded
        // frame with a new pacer row has one work sample, aligned with the recorded frames.
        let expected = (2..=last).filter(|n| !n.is_multiple_of(4)).count();
        assert_eq!(result["frames"].as_u64(), Some(u64::from(last - 1)));
        assert_eq!(
            work["frames"].as_u64(),
            Some(expected as u64),
            "one sample per new pacer row"
        );
        assert_eq!(work["gpu_timed_frames"], 0, "no renderer, no GPU time");
    }

    /// ADR-4947: the frame's work is the larger of its CPU work and GPU time; frames without a
    /// pacer row have no work sample, and no rows at all give `null`.
    #[test]
    fn work_takes_the_larger_of_cpu_and_gpu_per_frame() {
        let frame = |gpu_ms: f32| regia_render::HitchFrame {
            frame_ms: 16.7,
            cpu_ms: 2.0,
            gpu_ms,
            counters: Default::default(),
        };
        let frames = [frame(6.0), frame(14.0), frame(0.0), frame(30.0)];
        let cpu = [Some(9.0), Some(8.0), Some(11.0), None];
        let work = work_json(&frames, &cpu);
        assert_eq!(work["frames"], 3);
        assert_eq!(work["gpu_timed_frames"], 2);
        assert_eq!(work["p50_ms"], 11.0, "{work}");
        assert_eq!(
            work["longest_ms"], 14.0,
            "the GPU-bound frame counts its GPU time"
        );
        assert_eq!(work["cpu_p99_ms"], 11.0);
        assert_eq!(work_json(&frames, &[None, None, None, None]), Value::Null);
        assert_eq!(work_json(&[], &[]), Value::Null);
    }

    #[test]
    fn a_capture_started_mid_frame_does_not_count_the_prior_interval() {
        let mut world = World::new();
        world.init_resource::<HitchSessions>();
        let first = std::time::Instant::now();
        let request_at = first + Duration::from_secs(23);
        let mut time = RealTime::default();
        time.tick(first);
        time.tick(request_at);
        world.insert_resource(time);
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            &mut world,
            &json!({ "seconds": 3.0, "warmup_s": 0.0, "drive": "spawns" }),
            json!(1),
            tx,
        )
        .expect("starts");

        // The request arrives after the clock tick, before Last. The 23-second
        // interval ended before the request and cannot consume its capture window.
        advance_hitch_report(&mut world);
        assert!(rx.try_recv().is_err(), "a new capture must remain active");
        assert!(world.contains_resource::<HitchDrive>());
        for n in 1..=5u32 {
            world
                .resource_mut::<RealTime>()
                .tick(request_at + Duration::from_millis(600) * n);
            advance_hitch_report(&mut world);
            if n < 5 {
                assert!(rx.try_recv().is_err(), "capture ended before three seconds");
            }
        }
        assert!(rx.try_recv().is_ok(), "three seconds of new frames complete it");
        assert!(!world.contains_resource::<HitchDrive>());
    }

    fn frame(frame_ms: f32, gpu_ms: f32) -> regia_render::HitchFrame {
        regia_render::HitchFrame {
            frame_ms,
            gpu_ms,
            ..regia_render::HitchFrame::default()
        }
    }

    #[test]
    fn bound_verdict_counts_each_class_and_names_the_majority() {
        // GPU share 0.5 (cpu), 0.95 (gpu), 0.8 (balanced), no timing (unknown).
        let frames = [
            frame(10.0, 5.0),
            frame(10.0, 5.0),
            frame(10.0, 5.0),
            frame(10.0, 9.5),
            frame(10.0, 8.0),
            frame(10.0, 0.0),
        ];
        let b = bound_json(&frames);
        assert_eq!(b["verdict"], "cpu_bound");
        assert_eq!(b["cpu_bound_frames"], 3);
        assert_eq!(b["gpu_bound_frames"], 1);
        assert_eq!(b["balanced_frames"], 1);
        assert_eq!(b["unknown_frames"], 1);
        assert!((b["cpu_bound_pct"].as_f64().expect("pct") - 60.0).abs() < 1e-9);
    }

    #[test]
    fn bound_verdict_gpu_mixed_and_unknown() {
        assert_eq!(
            bound_json(&[frame(10.0, 9.8), frame(10.0, 9.6), frame(10.0, 5.0)])["verdict"],
            "gpu_bound"
        );
        assert_eq!(
            bound_json(&[frame(10.0, 5.0), frame(10.0, 9.8), frame(10.0, 8.0)])["verdict"],
            "mixed"
        );
        let none = bound_json(&[frame(10.0, 0.0)]);
        assert_eq!(none["verdict"], "unknown");
        assert!(none["gpu_bound_pct"].is_null());
        assert_eq!(bound_json(&[])["verdict"], "unknown");
    }

    #[test]
    fn one_report_at_a_time_and_bounded_seconds() {
        let mut app = app();
        app.update();
        let (tx, _rx) = crossbeam_channel::bounded(1);
        start(app.world_mut(), &json!({ "seconds": 5.0 }), json!(1), tx.clone()).expect("starts");
        assert!(start(app.world_mut(), &json!({}), json!(2), tx.clone()).is_err());
        let mut app = self::app();
        assert!(start(app.world_mut(), &json!({ "seconds": 900.0 }), json!(3), tx).is_err());
    }

    #[test]
    fn parameter_bounds_types_and_a_refused_start_leaves_no_session() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        for (seconds, ok) in [
            (0.09, false),
            (0.1, true),
            (600.0, true),
            (600.1, false),
            (-1.0, false),
            (0.0, false),
        ] {
            let mut app = app();
            app.update();
            let res = start(
                app.world_mut(),
                &json!({ "seconds": seconds }),
                json!(1),
                tx.clone(),
            );
            assert_eq!(res.is_ok(), ok, "seconds {seconds}");
        }
        for bad in [
            json!({ "seconds": "5" }),
            json!({ "seconds": [] }),
            json!({ "warmup_s": "long" }),
            json!({ "drive": 4 }),
            json!({ "drive": ["a"] }),
        ] {
            let mut app = app();
            app.update();
            let err = start(app.world_mut(), &bad, json!(1), tx.clone()).expect_err("type");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
            assert!(
                app.world().resource::<HitchSessions>().running.is_none(),
                "{bad} must not start a session"
            );
            assert!(!app.world().contains_resource::<HitchDrive>());
        }
        // Nulls are the defaults, and the second start reports the stable busy code.
        let mut app = app();
        app.update();
        start(
            app.world_mut(),
            &json!({ "seconds": null, "warmup_s": null, "drive": null }),
            json!(1),
            tx.clone(),
        )
        .expect("nulls");
        let busy = start(app.world_mut(), &json!({}), json!(2), tx).expect_err("busy");
        assert_eq!(busy.code, crate::protocol::INVALID_REQUEST);
        assert_eq!(busy.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0017"));
    }

    #[test]
    fn warmup_is_clamped_and_a_dropped_reply_channel_does_not_panic() {
        let mut app = app();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        start(
            app.world_mut(),
            &json!({ "seconds": 0.1, "warmup_s": -5.0 }),
            json!(1),
            tx,
        )
        .expect("negative warmup clamps to 0");
        drop(rx);
        for _ in 0..40 {
            app.update();
        }
        assert!(
            app.world().resource::<HitchSessions>().running.is_none(),
            "the session ends even when nobody listens"
        );
    }
}
