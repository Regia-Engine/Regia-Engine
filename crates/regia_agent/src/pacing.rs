// SPDX-License-Identifier: MPL-2.0
//! Frame pacing on the agent protocol (N12, ADR-1942, book `agents/frame-pacing.md`): `pacing.get`
//! reports what paces the frames ([`FramePacingReport`]), `pacing.set` picks the mode
//! ([`FramePacingSettings`]), and `pacing.simulate` runs the pacing model on virtual time, so a
//! change to the model can be checked on a desktop that has no vsync ticks.

use regia_app::prelude::*;
use regia_platform::frame_pacing::{
    FramePacingReport, FramePacingSettings, FramePacingTrace, FramePerfHint, FrameSteadyStart,
    FrameWakeAhead, IntervalStep, MAX_START_OFFSET_NS, MAX_SWAP_INTERVAL, MAX_WAKE_AHEAD_NS, PacingBreakdown,
    PacingCost, PacingHint, PacingMode, PacingSim, PacingSource, PacingTraceRow, SwapchainSim,
    TRACE_CAPACITY, pacing_breakdown, simulate,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Frames `pacing.simulate` runs when the call names no `frames`.
const DEFAULT_SIM_FRAMES: u64 = 600;
/// Most frames one `pacing.simulate` call runs.
pub(crate) const MAX_SIM_FRAMES: u64 = 20_000;

fn mode_json(mode: PacingMode) -> (&'static str, Value) {
    match mode {
        PacingMode::Auto => ("auto", Value::Null),
        PacingMode::Off => ("off", Value::Null),
        PacingMode::Interval(n) => ("interval", json!(n)),
    }
}

fn pacing_json(world: &World) -> Value {
    let settings = world
        .get_resource::<FramePacingSettings>()
        .copied()
        .unwrap_or_default();
    let report = world
        .get_resource::<FramePacingReport>()
        .copied()
        .unwrap_or_default();
    let (mode, interval) = mode_json(settings.mode);
    let stats = report.stats;
    let late_pct = (stats.frames > 0).then(|| stats.late as f64 * 100.0 / stats.frames as f64);
    let refresh = report.refresh_hz;
    let on_display = report.source != PacingSource::Software;
    json!({
        "mode": mode,
        "interval": interval,
        "cost": settings.cost.name(),
        "hint": settings.hint.name(),
        "source": report.source.name(),
        "available": report.ticks > 0,
        "ticks": report.ticks,
        "refresh_hz": refresh,
        "swap_interval": on_display.then_some(report.swap_interval),
        "slot_ms": refresh
            .filter(|_| on_display)
            .map(|hz| f64::from(report.swap_interval) * 1000.0 / f64::from(hz)),
        "frames": stats.frames,
        "late": stats.late,
        "late_pct": late_pct,
        "held": stats.held,
        "probes": stats.probes,
        "drains": stats.drains,
        "grid_moves": stats.grid_moves,
        "timeouts": report.timeouts,
        "interval_changes": stats.interval_changes,
        "refresh_changes": stats.refresh_changes,
        "frame_rate_hint": report.frame_rate_hint,
        "max_fps": world.get_resource::<regia_app::FramePacing>().and_then(|p| p.target_fps),
        "perf_hint": perf_hint_json(world),
        "wake_ahead_ms": world
            .get_resource::<FrameWakeAhead>()
            .copied()
            .unwrap_or_default()
            .lead_ns
            .map(ms),
        "wake_ahead_polled": world.get_resource::<FrameWakeAhead>().map_or(0, |w| w.polled_releases),
        "wake_ahead_predicted": world.get_resource::<FrameWakeAhead>().map_or(0, |w| w.predicted_releases),
        "steady_start": steady_start_json(world),
    })
}

/// The steady frame start settings and the frames released on the grid so far (ADR-5946).
fn steady_start_json(world: &World) -> Value {
    let s = world
        .get_resource::<FrameSteadyStart>()
        .copied()
        .unwrap_or_default();
    json!({
        "release": if s.grid { "grid" } else { "tick" },
        "grid_releases": s.grid_releases,
        "grid_follow": s.grid_follow,
        "start_offset_ms": s.start_offset_ns as f64 / 1e6,
        "drain_run": s.drain_run,
    })
}

/// The frame loop's performance hint session (ADR-5461), `null` where the runner keeps none.
fn perf_hint_json(world: &World) -> Value {
    world.get_resource::<FramePerfHint>().map_or(Value::Null, |h| {
        json!({
            "enabled": h.enabled,
            "active": h.active,
            "unavailable": h.unavailable,
            "target_ms": h.target_ns.map(ms),
            "reports": h.reports,
            "failures": h.failures,
        })
    })
}

/// `pacing.get`: the mode, what paces the frames right now, and the counters.
pub(crate) fn pacing_get(world: &mut World) -> Result<Value, RpcError> {
    Ok(pacing_json(world))
}

fn parse_mode(name: &str, interval: Option<&Value>) -> Result<PacingMode, RpcError> {
    Ok(match name {
        "auto" | "off" if interval.is_some() => {
            return Err(RpcError::invalid_params("`interval` goes with mode \"interval\""));
        }
        "auto" => PacingMode::Auto,
        "off" => PacingMode::Off,
        "interval" => {
            let n = interval
                .and_then(Value::as_u64)
                .filter(|n| (1..=u64::from(MAX_SWAP_INTERVAL)).contains(n))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`interval` must be an integer in 1..={MAX_SWAP_INTERVAL}"
                    ))
                })?;
            PacingMode::Interval(n as u32)
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown mode {other:?}: auto | off | interval"
            )));
        }
    })
}

/// `pacing.set`: `mode` is `auto`, `off`, or `interval` with `interval`; `cost` is `work` or
/// `frame` (what `auto` counts as a frame's cost, ADR-2320); `hint` is `double`, `slot` or `off`
/// (the rate asked of the compositor, ADR-2339); `perf_hint` is a boolean (the frame loop's
/// performance hint session, ADR-5461). At least one of them; returns `{ before, after }`.
pub(crate) fn pacing_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let interval = params.get("interval").filter(|v| !v.is_null());
    let mode = match params.get("mode").filter(|v| !v.is_null()) {
        None if interval.is_some() => {
            return Err(RpcError::invalid_params("`interval` goes with mode \"interval\""));
        }
        None => None,
        Some(v) => {
            let name = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`mode` must be a string: auto | off | interval"))?;
            Some(parse_mode(name, interval)?)
        }
    };
    let cost = match params.get("cost").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(match v.as_str() {
            Some("work") => PacingCost::Work,
            Some("frame") => PacingCost::Frame,
            _ => return Err(RpcError::invalid_params("`cost` must be \"work\" or \"frame\"")),
        }),
    };
    let hint = params
        .get("hint")
        .filter(|v| !v.is_null())
        .map(parse_hint)
        .transpose()?;
    let perf_hint = match params.get("perf_hint").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`perf_hint` must be a boolean"))?,
        ),
    };
    // `wake_ahead_ms`: a number sets the lead, 0 or null turns the wake-ahead off (ADR-5461).
    let wake_ahead = match params.get("wake_ahead_ms") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let max_ms = ms(MAX_WAKE_AHEAD_NS);
            let phase = v
                .as_f64()
                .filter(|n| n.is_finite() && (0.0..=max_ms).contains(n))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`wake_ahead_ms` must be a number in 0..={max_ms}"))
                })?;
            Some(Some((phase * 1e6).round() as u64).filter(|ns| *ns > 0))
        }
    };
    // ADR-5946: `release` grid | tick, `grid_follow` a boolean, `start_offset_ms` a number,
    // `drain_run` 1..=1024.
    let release = match params.get("release").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(match v.as_str() {
            Some("grid") => true,
            Some("tick") => false,
            _ => return Err(RpcError::invalid_params("`release` must be \"grid\" or \"tick\"")),
        }),
    };
    let grid_follow = match params.get("grid_follow").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`grid_follow` must be a boolean"))?,
        ),
    };
    let start_offset = match params.get("start_offset_ms").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let max_ms = ms(MAX_START_OFFSET_NS.unsigned_abs());
            let offset = v
                .as_f64()
                .filter(|n| n.is_finite() && (-max_ms..=max_ms).contains(n))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`start_offset_ms` must be a number in -{max_ms}..={max_ms}"
                    ))
                })?;
            Some((offset * 1e6).round() as i64)
        }
    };
    let drain_run = match params.get("drain_run").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=1024).contains(n))
                .ok_or_else(|| RpcError::invalid_params("`drain_run` must be an integer in 1..=1024"))?
                as u32,
        ),
    };
    if mode.is_none()
        && cost.is_none()
        && hint.is_none()
        && perf_hint.is_none()
        && wake_ahead.is_none()
        && release.is_none()
        && grid_follow.is_none()
        && start_offset.is_none()
        && drain_run.is_none()
    {
        return Err(RpcError::invalid_params(
            "`mode` (auto | off | interval), `cost` (work | frame), `hint` (double | slot | off), `perf_hint` (true | false), `wake_ahead_ms`, `release` (grid | tick), `grid_follow` (true | false), `start_offset_ms` or `drain_run` is required",
        ));
    }
    let before = pacing_json(world);
    {
        let mut settings = world.get_resource_or_insert_with(FramePacingSettings::default);
        if let Some(mode) = mode {
            settings.mode = mode;
        }
        if let Some(cost) = cost {
            settings.cost = cost;
        }
        if let Some(hint) = hint {
            settings.hint = hint;
        }
    }
    if let Some(enabled) = perf_hint {
        world.get_resource_or_insert_with(FramePerfHint::default).enabled = enabled;
    }
    if let Some(lead_ns) = wake_ahead {
        world.get_resource_or_insert_with(FrameWakeAhead::default).lead_ns = lead_ns;
    }
    if release.is_some() || grid_follow.is_some() || start_offset.is_some() || drain_run.is_some() {
        let mut steady = world.get_resource_or_insert_with(FrameSteadyStart::default);
        if let Some(grid) = release {
            steady.grid = grid;
        }
        if let Some(follow) = grid_follow {
            steady.grid_follow = follow;
        }
        if let Some(offset_ns) = start_offset {
            steady.start_offset_ns = offset_ns;
        }
        if let Some(run) = drain_run {
            steady.drain_run = run;
        }
    }
    Ok(json!({ "before": before, "after": pacing_json(world) }))
}

fn parse_hint(value: &Value) -> Result<PacingHint, RpcError> {
    match value.as_str() {
        Some("double") => Ok(PacingHint::Double),
        Some("slot") => Ok(PacingHint::Slot),
        Some("off") => Ok(PacingHint::Off),
        _ => Err(RpcError::invalid_params(
            "`hint` must be \"double\", \"slot\" or \"off\"",
        )),
    }
}

/// Frames `pacing.trace` returns when the call names no `frames`.
const DEFAULT_TRACE_FRAMES: u64 = 120;

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn row_json(row: &PacingTraceRow, origin: u64) -> Value {
    json!({
        "start_ms": ms(row.start_ns.saturating_sub(origin)),
        "cost_ms": ms(row.cost_ns),
        "swapchain_wait_ms": ms(row.swapchain_wait_ns),
        "work_ms": ms(row.cost_ns.saturating_sub(row.swapchain_wait_ns)),
        "waited_ms": ms(row.waited_ns),
        "release_tick_ms": row.release_tick_ns.map(|t| ms(t.saturating_sub(origin))),
        "ticks": row.ticks,
        "period_ms": row.period_ns.map(ms),
        "interval": row.interval,
        "hint_hz": row.hint_hz,
        "paced": row.paced,
        "tick_gap_max_ms": ms(row.tick_gap_max_ns),
        "missed_ticks": row.missed_ticks,
        "cap_waited_ms": ms(row.cap_waited_ns),
        "tick_delivered_ms": row.tick_delivered_ns.map(|t| ms(t.saturating_sub(origin))),
        "woke_ms": row.woke_ns.map(|t| ms(t.saturating_sub(origin))),
        "release": row.release.map(|r| r.name()),
        "sleep_late_ms": row.sleep_late_ns.map(ms),
        "work_run_delay_ms": row.work_run_delay_ns.map(ms),
        "wait_run_delay_ms": row.wait_run_delay_ns.map(ms),
        "preemptions": row.preemptions,
        "wake_cpu": row.wake_cpu,
    })
}

/// `pacing.trace`'s `breakdown` (ADR-4913): where the time between frame starts went, the tick
/// cadence apart, and what held the frames back the most.
fn breakdown_json(b: &PacingBreakdown) -> Value {
    let [work, swapchain, wait, cap, outside] = b.shares;
    json!({
        "intervals": b.intervals,
        "interval_p50_ms": ms(b.interval_p50_ns),
        "work_p50_ms": ms(b.work_p50_ns),
        "swapchain_p50_ms": ms(b.swapchain_p50_ns),
        "wait_p50_ms": ms(b.wait_p50_ns),
        "cap_p50_ms": ms(b.cap_p50_ns),
        "loop_p50_ms": ms(b.loop_p50_ns),
        "shares": {
            "work": work,
            "swapchain": swapchain,
            "wait": wait,
            "cap": cap,
            "loop": outside,
        },
        "period_ms": b.period_ns.map(ms),
        "vsyncs_per_frame": b.vsyncs_per_frame,
        "ticks": b.ticks,
        "missed_ticks": b.missed_ticks,
        "tick_gap_max_ms": ms(b.tick_gap_max_ns),
        "limit": b.limit.name(),
    })
}

/// The median of `values` (0 when empty).
fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// `pacing.trace`: the display pacer's last `frames` frames (default 120, at most
/// `TRACE_CAPACITY`), oldest first, with medians. `log: true|false` turns the per-frame log line
/// on or off (target `regia_pacing`: logcat on Android); `clear: true` forgets the rows after
/// reading them.
pub(crate) fn pacing_trace(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frames = match params.get("frames") {
        None | Some(Value::Null) => DEFAULT_TRACE_FRAMES,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=TRACE_CAPACITY as u64).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`frames` must be an integer in 1..={TRACE_CAPACITY}"))
            })?,
    };
    let flag = |name: &str| match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean"))),
    };
    let log = flag("log")?;
    let clear = flag("clear")?.unwrap_or(false);
    // A plain read leaves the world as it was; only `log` and `clear` write.
    if let Some(log) = log {
        world.get_resource_or_insert_with(FramePacingTrace::default).log = log;
    }
    let (rows, logging) = world
        .get_resource::<FramePacingTrace>()
        .map_or((Vec::new(), false), |trace| {
            let skip = trace.rows().len().saturating_sub(frames as usize);
            let rows: Vec<PacingTraceRow> = trace.rows().skip(skip).copied().collect();
            (rows, trace.log)
        });
    let origin = rows.first().map_or(0, |r| r.start_ns);
    let starts: Vec<u64> = rows.iter().map(|r| r.start_ns).collect();
    let gaps: Vec<f64> = starts.windows(2).map(|w| ms(w[1].saturating_sub(w[0]))).collect();
    let span_ms = ms(starts.last().copied().unwrap_or(0).saturating_sub(origin));
    let fps = (rows.len() > 1 && span_ms > 0.0).then(|| (rows.len() - 1) as f64 * 1000.0 / span_ms);
    let summary = json!({
        "frames": rows.len(),
        "fps": fps,
        "paced": rows.iter().filter(|r| r.paced).count(),
        "gap_p50_ms": median(gaps),
        "cost_p50_ms": median(rows.iter().map(|r| ms(r.cost_ns)).collect()),
        "swapchain_wait_p50_ms": median(rows.iter().map(|r| ms(r.swapchain_wait_ns)).collect()),
        "work_p50_ms": median(rows.iter().map(|r| ms(r.cost_ns.saturating_sub(r.swapchain_wait_ns))).collect()),
        "waited_p50_ms": median(rows.iter().map(|r| ms(r.waited_ns)).collect()),
        "breakdown": pacing_breakdown(&rows).as_ref().map(breakdown_json),
    });
    let out = json!({
        "log": logging,
        // Where the rows' times count from, on the clock of the vsync ticks (CLOCK_MONOTONIC on
        // Android): add it to a row's `start_ms` to line the row up with the compositor's own
        // timestamps (`dumpsys SurfaceFlinger --latency`, a perfetto trace). ADR-5946.
        "origin_ns": (!rows.is_empty()).then_some(origin),
        "summary": summary,
        "rows": rows.iter().map(|r| row_json(r, origin)).collect::<Vec<_>>(),
    });
    if clear && let Some(mut trace) = world.get_resource_mut::<FramePacingTrace>() {
        trace.clear();
    }
    Ok(out)
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| Some(n as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number"))),
    }
}

/// `pacing.simulate`: the pacing model on an even tick train and the given frame costs.
pub(crate) fn pacing_simulate(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let refresh_hz = number(params, "refresh_hz")?.unwrap_or(60.0);
    let frame_ms: Vec<f32> = match params.get("frame_ms") {
        Some(Value::Array(costs)) => costs
            .iter()
            .map(|c| c.as_f64().map(|c| c as f32))
            .collect::<Option<_>>()
            .ok_or_else(|| RpcError::invalid_params("`frame_ms` holds numbers"))?,
        Some(v) if v.is_number() => vec![v.as_f64().unwrap_or(0.0) as f32],
        None | Some(Value::Null) => vec![8.0],
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`frame_ms` is a number or an array of numbers",
            ));
        }
    };
    let frames = match params.get("frames") {
        None | Some(Value::Null) => DEFAULT_SIM_FRAMES,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_SIM_FRAMES).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`frames` must be an integer in 1..={MAX_SIM_FRAMES}"))
            })?,
    };
    let interval = match params.get("interval") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=u64::from(MAX_SWAP_INTERVAL)).contains(n))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`interval` must be an integer in 1..={MAX_SWAP_INTERVAL}"
                    ))
                })? as u32,
        ),
    };
    let refresh_switch = match (params.get("switch_frame"), number(params, "switch_hz")?) {
        (None | Some(Value::Null), None) => None,
        (Some(frame), Some(hz)) => {
            let frame = frame
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`switch_frame` must be an integer"))?;
            Some((u32::try_from(frame).unwrap_or(u32::MAX), hz))
        }
        _ => {
            return Err(RpcError::invalid_params(
                "`switch_frame` and `switch_hz` go together",
            ));
        }
    };
    let swapchain = match params.get("swapchain_images") {
        None | Some(Value::Null) => {
            if number(params, "gpu_ms")?.is_some() || number(params, "latch_ms")?.is_some() {
                return Err(RpcError::invalid_params(
                    "`gpu_ms` and `latch_ms` go with `swapchain_images`",
                ));
            }
            None
        }
        Some(v) => Some(SwapchainSim {
            images: v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`swapchain_images` must be an integer in 2..=8"))?,
            gpu_ms: number(params, "gpu_ms")?.unwrap_or(0.0),
            latch_ms: number(params, "latch_ms")?.unwrap_or(0.0),
        }),
    };
    let cost = match params.get("cost").filter(|v| !v.is_null()) {
        None => PacingCost::Work,
        Some(v) => match v.as_str() {
            Some("work") => PacingCost::Work,
            Some("frame") => PacingCost::Frame,
            _ => return Err(RpcError::invalid_params("`cost` must be \"work\" or \"frame\"")),
        },
    };
    let sim = PacingSim {
        refresh_hz,
        frame_ms,
        frames: frames as u32,
        cap_fps: number(params, "cap_fps")?,
        interval,
        refresh_switch,
        follow_hint: match params.get("follow_hint") {
            None | Some(Value::Null) => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`follow_hint` must be a boolean"))?,
        },
        swapchain,
        cost,
        stretch: number(params, "stretch")?.unwrap_or(0.0),
        hint: params
            .get("hint")
            .filter(|v| !v.is_null())
            .map(parse_hint)
            .transpose()?
            .unwrap_or_default(),
    };
    let report = simulate(&sim).ok_or_else(|| {
        RpcError::invalid_params(
            "`refresh_hz` and `switch_hz` must be in 5..=500, `cap_fps` must be above 0, `frame_ms` must hold costs in 0..=10000, `swapchain_images` must be in 2..=8 with `gpu_ms` in 0..=10000 and `latch_ms` in 0..=1000, `stretch` must be in 0..=0.95, and the run must stay under a million vsyncs",
        )
    })?;
    let steps = |s: &IntervalStep| json!({ "frame": s.frame, "interval": s.interval });
    Ok(json!({
        "period_ms": report.period_ms,
        "swap_interval": report.swap_interval,
        "steps": report.steps.iter().map(steps).collect::<Vec<_>>(),
        "frames": report.stats.frames,
        "late": report.stats.late,
        "held": report.stats.held,
        "probes": report.stats.probes,
        "interval_changes": report.stats.interval_changes,
        "refresh_changes": report.stats.refresh_changes,
        "tail_min_gap_ms": report.tail_min_gap_ms,
        "tail_max_gap_ms": report.tail_max_gap_ms,
        "tail_late": report.tail_late,
        "tail_mean_gap_ms": report.tail_mean_gap_ms,
        "tail_swapchain_wait_ms": report.tail_swapchain_wait_ms,
        "frame_rate_hint": report.frame_rate_hint,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_reports_the_software_path_on_a_desktop() {
        let mut world = World::new();
        let got = pacing_get(&mut world).expect("get");
        assert_eq!(got["mode"], "auto");
        assert_eq!(got["source"], "software");
        assert_eq!(got["available"], false);
        assert_eq!(got["swap_interval"], Value::Null);
        assert_eq!(got["slot_ms"], Value::Null);
        assert_eq!(got["late_pct"], Value::Null);
    }

    #[test]
    fn get_reports_a_paced_display() {
        let mut world = World::new();
        let mut report = FramePacingReport {
            source: PacingSource::Choreographer,
            ticks: 900,
            refresh_hz: Some(120.0),
            swap_interval: 3,
            frame_rate_hint: Some(40.0),
            ..FramePacingReport::default()
        };
        report.stats.frames = 200;
        report.stats.late = 5;
        world.insert_resource(report);
        world.insert_resource(regia_app::FramePacing::new(Some(45.0)));
        let got = pacing_get(&mut world).expect("get");
        assert_eq!(got["source"], "choreographer");
        assert_eq!(got["available"], true);
        assert_eq!(got["swap_interval"], 3);
        assert_eq!(got["slot_ms"], 25.0);
        assert_eq!(got["late_pct"], 2.5);
        assert_eq!(got["frame_rate_hint"], 40.0);
        assert_eq!(got["max_fps"], 45.0);
    }

    #[test]
    fn set_changes_the_mode_and_refuses_nonsense() {
        let mut world = World::new();
        let got = pacing_set(&mut world, &json!({ "mode": "interval", "interval": 2 })).expect("set");
        assert_eq!(got["before"]["mode"], "auto");
        assert_eq!(got["after"]["mode"], "interval");
        assert_eq!(got["after"]["interval"], 2);
        assert_eq!(
            world.resource::<FramePacingSettings>().mode,
            PacingMode::Interval(2)
        );
        let got = pacing_set(&mut world, &json!({ "mode": "off" })).expect("off");
        assert_eq!(got["after"]["mode"], "off");
        for bad in [
            json!({}),
            json!({ "mode": "fast" }),
            json!({ "mode": 3 }),
            json!({ "mode": "interval" }),
            json!({ "mode": "interval", "interval": 0 }),
            json!({ "mode": "interval", "interval": 99 }),
            json!({ "mode": "interval", "interval": 1.5 }),
            json!({ "mode": "auto", "interval": 2 }),
        ] {
            assert!(pacing_set(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world.resource::<FramePacingSettings>().mode,
            PacingMode::Off,
            "a refused call changes nothing"
        );
    }

    #[test]
    fn simulate_runs_the_model() {
        let mut world = World::new();
        let got = pacing_simulate(
            &mut world,
            &json!({ "refresh_hz": 60, "frame_ms": 20, "frames": 300 }),
        )
        .expect("simulate");
        assert_eq!(got["swap_interval"], 2);
        assert_eq!(got["steps"], json!([{ "frame": 3, "interval": 2 }]));
        assert_eq!(got["tail_late"], 0);
        let capped = pacing_simulate(
            &mut world,
            &json!({ "refresh_hz": 120, "frame_ms": [2, 3], "cap_fps": 30 }),
        )
        .expect("capped");
        assert_eq!(capped["swap_interval"], 4);
        let switched = pacing_simulate(
            &mut world,
            &json!({ "refresh_hz": 60, "frame_ms": 20, "frames": 900, "switch_frame": 200, "switch_hz": 120 }),
        )
        .expect("switched");
        assert_eq!(switched["refresh_changes"], 1);
        let following = pacing_simulate(
            &mut world,
            &json!({ "refresh_hz": 120, "frame_ms": 11, "frames": 2000, "follow_hint": true, "hint": "slot" }),
        )
        .expect("following");
        assert_eq!(following["refresh_changes"], 1, "{following}");
        assert_eq!(following["swap_interval"], 1, "{following}");
        assert_eq!(following["tail_late"], 0, "{following}");
        assert_eq!(following["frame_rate_hint"].as_f64().map(f64::round), Some(60.0));
        let forced = pacing_simulate(&mut world, &json!({ "frame_ms": 2, "interval": 3 })).expect("forced");
        assert_eq!(forced["swap_interval"], 3);
        let fast_cap = pacing_simulate(
            &mut world,
            &json!({ "frame_ms": 2, "frames": 30, "cap_fps": 2000 }),
        )
        .expect("2000 is inside the runner's 1..=10000");
        assert_eq!(fast_cap["swap_interval"], 1, "{fast_cap}");
        let lowest = pacing_simulate(
            &mut world,
            &json!({ "frame_ms": 2, "frames": 20, "cap_fps": 1e-30 }),
        )
        .expect("a tiny cap runs at the lowest rate");
        assert_eq!(lowest["swap_interval"], 60);
        let defaults = pacing_simulate(&mut world, &json!({})).expect("defaults");
        assert_eq!(defaults["frames"], DEFAULT_SIM_FRAMES);
    }

    #[test]
    fn simulate_refuses_nonsense() {
        let mut world = World::new();
        for bad in [
            json!({ "refresh_hz": 0 }),
            json!({ "refresh_hz": "60" }),
            json!({ "refresh_hz": 1e-30 }),
            json!({ "refresh_hz": 1000 }),
            json!({ "switch_frame": 1, "switch_hz": 1e-30 }),
            json!({ "frame_ms": [] }),
            json!({ "frame_ms": [-1] }),
            json!({ "frame_ms": [1e12] }),
            json!({ "frame_ms": ["x"] }),
            json!({ "frame_ms": "x" }),
            json!({ "frames": 0 }),
            json!({ "frames": 1_000_000 }),
            json!({ "cap_fps": 0 }),
            json!({ "interval": 0 }),
            json!({ "switch_frame": 10 }),
            json!({ "switch_hz": 120 }),
            json!({ "switch_frame": "x", "switch_hz": 120 }),
        ] {
            assert!(pacing_simulate(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn set_picks_the_cost_alone_or_with_a_mode() {
        let mut world = World::new();
        assert_eq!(pacing_get(&mut world).expect("get")["cost"], "work");
        let got = pacing_set(&mut world, &json!({ "cost": "frame" })).expect("cost alone");
        assert_eq!(got["before"]["cost"], "work");
        assert_eq!(got["after"]["cost"], "frame");
        assert_eq!(got["after"]["mode"], "auto", "the mode stays");
        let both = pacing_set(&mut world, &json!({ "mode": "off", "cost": "work" })).expect("both");
        assert_eq!(both["after"]["mode"], "off");
        assert_eq!(both["after"]["cost"], "work");
        for bad in [
            json!({ "cost": "gpu" }),
            json!({ "cost": 1 }),
            json!({ "interval": 2 }),
            json!({ "mode": "off", "cost": "fast" }),
        ] {
            assert!(pacing_set(&mut world, &bad).is_err(), "{bad}");
        }
        let settings = *world.resource::<FramePacingSettings>();
        assert_eq!(settings.mode, PacingMode::Off, "a refused call changes nothing");
        assert_eq!(settings.cost, PacingCost::Work);
    }

    #[test]
    fn set_picks_the_hint_and_get_reports_it() {
        let mut world = World::new();
        assert_eq!(pacing_get(&mut world).expect("get")["hint"], "double");
        let got = pacing_set(&mut world, &json!({ "hint": "slot" })).expect("hint alone");
        assert_eq!(got["before"]["hint"], "double");
        assert_eq!(got["after"]["hint"], "slot");
        assert_eq!(got["after"]["mode"], "auto", "the mode stays");
        let off = pacing_set(&mut world, &json!({ "mode": "auto", "hint": "off" })).expect("with a mode");
        assert_eq!(off["after"]["hint"], "off");
        for bad in [
            json!({ "hint": "half" }),
            json!({ "hint": 2 }),
            json!({ "cost": "work", "hint": "x" }),
        ] {
            assert!(pacing_set(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(world.resource::<FramePacingSettings>().hint, PacingHint::Off);
        assert!(pacing_simulate(&mut world, &json!({ "hint": "half" })).is_err());
        // 20 ms frames on 120 Hz: 3 vsyncs. The slot hint asks for 40, double asks for nothing.
        let run = |hint: &str| json!({ "refresh_hz": 120, "frame_ms": 20, "frames": 600, "hint": hint });
        let slot = pacing_simulate(&mut world, &run("slot")).expect("slot");
        assert_eq!(
            slot["frame_rate_hint"].as_f64().map(f64::round),
            Some(40.0),
            "{slot}"
        );
        let double = pacing_simulate(&mut world, &run("double")).expect("double");
        assert_eq!(double["frame_rate_hint"], Value::Null, "{double}");
    }

    #[test]
    fn set_switches_the_performance_hint_and_get_reports_it() {
        // ADR-5461.
        let mut world = World::new();
        assert_eq!(
            pacing_get(&mut world).expect("get")["perf_hint"],
            Value::Null,
            "no runner, no session"
        );
        world.insert_resource(FramePerfHint {
            enabled: true,
            active: true,
            target_ns: Some(16_666_667),
            reports: 480,
            ..FramePerfHint::default()
        });
        let got = pacing_set(&mut world, &json!({ "perf_hint": false })).expect("perf_hint alone");
        assert_eq!(got["before"]["perf_hint"]["enabled"], true);
        assert_eq!(got["before"]["perf_hint"]["active"], true);
        assert_eq!(got["before"]["perf_hint"]["reports"], 480);
        assert!((got["before"]["perf_hint"]["target_ms"].as_f64().unwrap_or(0.0) - 16.667).abs() < 0.001);
        assert_eq!(got["after"]["perf_hint"]["enabled"], false);
        assert_eq!(got["after"]["mode"], "auto", "the mode stays");
        assert!(!world.resource::<FramePerfHint>().enabled);
        for bad in [json!({ "perf_hint": "on" }), json!({ "perf_hint": 1 })] {
            assert!(pacing_set(&mut world, &bad).is_err(), "{bad}");
        }
        assert!(
            !world.resource::<FramePerfHint>().enabled,
            "a refused call changes nothing"
        );
        pacing_set(&mut world, &json!({ "perf_hint": true })).expect("back on");
        assert!(world.resource::<FramePerfHint>().enabled);
    }

    #[test]
    fn set_takes_a_wake_ahead_and_turns_it_off() {
        // ADR-5461: on by default, 2.5 ms.
        let mut world = World::new();
        let got = pacing_get(&mut world).expect("get");
        assert_eq!(got["wake_ahead_ms"], 2.5);
        assert_eq!(got["wake_ahead_polled"], 0);
        let got = pacing_set(&mut world, &json!({ "wake_ahead_ms": 1.5 })).expect("lead alone");
        assert_eq!(got["after"]["wake_ahead_ms"], 1.5);
        assert_eq!(world.resource::<FrameWakeAhead>().lead_ns, Some(1_500_000));
        for bad in [
            json!({ "wake_ahead_ms": 4.5 }),
            json!({ "wake_ahead_ms": -1 }),
            json!({ "wake_ahead_ms": "2" }),
        ] {
            assert!(pacing_set(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world.resource::<FrameWakeAhead>().lead_ns,
            Some(1_500_000),
            "unchanged"
        );
        pacing_set(&mut world, &json!({ "wake_ahead_ms": 0 })).expect("zero turns it off");
        assert_eq!(world.resource::<FrameWakeAhead>().lead_ns, None);
        assert_eq!(pacing_get(&mut world).expect("get")["wake_ahead_ms"], Value::Null);
        pacing_set(&mut world, &json!({ "wake_ahead_ms": 4 })).expect("the largest");
        assert_eq!(
            world.resource::<FrameWakeAhead>().lead_ns,
            Some(MAX_WAKE_AHEAD_NS)
        );
        pacing_set(&mut world, &json!({ "wake_ahead_ms": null })).expect("null turns it off");
        assert_eq!(world.resource::<FrameWakeAhead>().lead_ns, None);
        world.resource_mut::<FrameWakeAhead>().polled_releases = 7;
        world.resource_mut::<FrameWakeAhead>().predicted_releases = 2;
        let got = pacing_get(&mut world).expect("get");
        assert_eq!(got["wake_ahead_polled"], 7);
        assert_eq!(got["wake_ahead_predicted"], 2);
        assert!(
            pacing_set(&mut world, &json!({})).is_err(),
            "still one field at least"
        );
    }

    #[test]
    fn trace_rows_carry_the_delivery_and_wake_stamps() {
        // ADR-5461: on the trace's own origin, like start_ms and release_tick_ms.
        let mut world = World::new();
        {
            let mut trace = world.get_resource_or_insert_with(FramePacingTrace::default);
            trace.push(PacingTraceRow {
                start_ns: 1_000_000_000,
                release_tick_ns: Some(1_010_000_000),
                tick_delivered_ns: Some(1_010_700_000),
                woke_ns: Some(1_011_200_000),
                paced: true,
                ..PacingTraceRow::default()
            });
            trace.push(PacingTraceRow {
                start_ns: 1_011_500_000,
                ..PacingTraceRow::default()
            });
        }
        let got = pacing_trace(&mut world, &json!({})).expect("trace");
        let rows = got["rows"].as_array().cloned().unwrap_or_default();
        let near = |v: &Value, want: f64| (v.as_f64().unwrap_or(f64::NAN) - want).abs() < 1e-6;
        assert!(near(&rows[0]["release_tick_ms"], 10.0), "{got}");
        assert!(near(&rows[0]["tick_delivered_ms"], 10.7), "{got}");
        assert!(near(&rows[0]["woke_ms"], 11.2), "{got}");
        assert_eq!(rows[1]["tick_delivered_ms"], Value::Null);
        assert_eq!(rows[1]["woke_ms"], Value::Null);
    }

    #[test]
    fn set_picks_the_steady_start_and_get_reports_it() {
        // ADR-5946: the grid release and the grid follow are on by default, and a full display
        // queue is drained after two held late starts.
        let mut world = World::new();
        let defaults = FrameSteadyStart::default();
        let got = pacing_get(&mut world).expect("get");
        assert_eq!(
            got["steady_start"],
            json!({
                "release": "grid", "grid_releases": 0, "grid_follow": true,
                "start_offset_ms": defaults.start_offset_ns as f64 / 1e6, "drain_run": 2,
            })
        );
        assert_eq!(got["grid_moves"], 0);
        let got = pacing_set(&mut world, &json!({ "release": "tick" })).expect("release alone");
        assert_eq!(got["before"]["steady_start"]["release"], "grid");
        assert_eq!(got["after"]["steady_start"]["release"], "tick");
        assert!(!world.resource::<FrameSteadyStart>().grid);
        pacing_set(
            &mut world,
            &json!({ "release": "grid", "grid_follow": false, "start_offset_ms": -2.5, "drain_run": 8 }),
        )
        .expect("all of them");
        let steady = *world.resource::<FrameSteadyStart>();
        assert!(steady.grid && !steady.grid_follow);
        assert_eq!((steady.start_offset_ns, steady.drain_run), (-2_500_000, 8));
        let got = pacing_set(&mut world, &json!({ "start_offset_ms": 8 })).expect("the largest");
        assert_eq!(got["after"]["steady_start"]["start_offset_ms"], 8.0);
        assert_eq!(
            world.resource::<FrameSteadyStart>().drain_run,
            8,
            "the other fields stay"
        );
        for bad in [
            json!({ "release": "vsync" }),
            json!({ "release": true }),
            json!({ "grid_follow": "yes" }),
            json!({ "start_offset_ms": 8.5 }),
            json!({ "start_offset_ms": -9 }),
            json!({ "start_offset_ms": "early" }),
            json!({ "drain_run": 0 }),
            json!({ "drain_run": 1025 }),
            json!({ "drain_run": 2.5 }),
        ] {
            assert!(pacing_set(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world.resource::<FrameSteadyStart>().start_offset_ns,
            8_000_000,
            "unchanged"
        );
        // What the runner writes back.
        world.resource_mut::<FrameSteadyStart>().grid_releases = 480;
        let got = pacing_get(&mut world).expect("get");
        assert_eq!(got["steady_start"]["grid_releases"], 480);
    }

    #[test]
    fn trace_rows_carry_the_release_and_the_scheduler_stamps() {
        // ADR-5946.
        let mut world = World::new();
        {
            let mut trace = world.get_resource_or_insert_with(FramePacingTrace::default);
            trace.push(PacingTraceRow {
                start_ns: 1_000_000_000,
                paced: true,
                release: Some(regia_platform::frame_pacing::PacingRelease::Grid),
                sleep_late_ns: Some(1_700_000),
                work_run_delay_ns: Some(250_000),
                wait_run_delay_ns: Some(40_000),
                preemptions: Some(3),
                wake_cpu: Some(6),
                ..PacingTraceRow::default()
            });
            trace.push(PacingTraceRow {
                start_ns: 1_016_600_000,
                ..PacingTraceRow::default()
            });
        }
        let got = pacing_trace(&mut world, &json!({})).expect("trace");
        let rows = got["rows"].as_array().cloned().unwrap_or_default();
        let near = |v: &Value, want: f64| (v.as_f64().unwrap_or(f64::NAN) - want).abs() < 1e-6;
        assert_eq!(got["origin_ns"], 1_000_000_000_u64, "the first row's start");
        assert_eq!(rows[0]["release"], "grid");
        assert!(near(&rows[0]["sleep_late_ms"], 1.7), "{got}");
        assert!(near(&rows[0]["work_run_delay_ms"], 0.25), "{got}");
        assert!(near(&rows[0]["wait_run_delay_ms"], 0.04), "{got}");
        assert_eq!(rows[0]["preemptions"], 3);
        assert_eq!(rows[0]["wake_cpu"], 6);
        for field in [
            "release",
            "sleep_late_ms",
            "work_run_delay_ms",
            "wait_run_delay_ms",
            "preemptions",
            "wake_cpu",
        ] {
            assert_eq!(rows[1][field], Value::Null, "{field}");
        }
    }

    #[test]
    fn simulate_takes_a_swapchain_and_a_cost() {
        let mut world = World::new();
        let phone = |cost: &str| {
            json!({ "refresh_hz": 60, "frame_ms": 6, "frames": 2000, "follow_hint": true, "hint": "slot",
                    "swapchain_images": 2, "gpu_ms": 7, "latch_ms": 4, "cost": cost })
        };
        let old = pacing_simulate(&mut world, &phone("frame")).expect("frame");
        let gap = |v: &Value| v["tail_mean_gap_ms"].as_f64().unwrap_or(0.0);
        assert!((gap(&old) - 100.0).abs() < 0.5, "{old}");
        let new = pacing_simulate(&mut world, &phone("work")).expect("work");
        assert!((gap(&new) - 33.33).abs() < 0.5, "{new}");
        assert_eq!(new["frame_rate_hint"], Value::Null, "{new}");
        assert_eq!(new["tail_late"], 0, "{new}");
        assert!(new["held"].as_u64().unwrap_or(0) > 1900, "{new}");
        for bad in [
            json!({ "swapchain_images": 1 }),
            json!({ "swapchain_images": 9 }),
            json!({ "swapchain_images": "2" }),
            json!({ "swapchain_images": 2, "latch_ms": 5000 }),
            json!({ "gpu_ms": 5 }),
            json!({ "latch_ms": 5 }),
            json!({ "cost": "gpu" }),
        ] {
            assert!(pacing_simulate(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn simulate_takes_a_stretch_and_reports_probes() {
        // ADR-2339: a start-up stall, then 25 ms frames that grow into 60 % of the idle time their
        // recent frames leave, on a 60 Hz panel that follows the hint. The v0.2.10 model ends at
        // 10 fps, every frame late; the work cost probes its way to 30.
        let mut world = World::new();
        let mut frames = vec![300.0; 10];
        frames.extend(std::iter::repeat_n(25.0, 2990));
        let run = |cost: &str| {
            json!({ "refresh_hz": 60, "frame_ms": frames, "frames": 3000, "follow_hint": true, "hint": "slot",
                    "stretch": 0.6, "cost": cost })
        };
        let gap = |v: &Value| v["tail_mean_gap_ms"].as_f64().unwrap_or(0.0);
        let old = pacing_simulate(&mut world, &run("frame")).expect("frame");
        assert!(gap(&old) > 99.0, "{old}");
        assert_eq!(old["probes"], 0, "{old}");
        let new = pacing_simulate(&mut world, &run("work")).expect("work");
        assert!((gap(&new) - 33.33).abs() < 0.5, "{new}");
        assert!(new["probes"].as_u64().unwrap_or(0) >= 1, "{new}");
        assert!(pacing_get(&mut world).expect("get")["probes"].is_u64());
        // The default hint keeps two vsyncs a frame: the same phone without our hint's collapse.
        let double = pacing_simulate(
            &mut world,
            &json!({ "refresh_hz": 60, "frame_ms": 6, "frames": 2000, "follow_hint": true,
                     "swapchain_images": 2, "gpu_ms": 7, "latch_ms": 4, "cost": "frame" }),
        )
        .expect("double");
        assert!(gap(&double) < 34.0, "{double}");
        for bad in [
            json!({ "stretch": -0.1 }),
            json!({ "stretch": 0.99 }),
            json!({ "stretch": "x" }),
        ] {
            assert!(pacing_simulate(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn trace_returns_the_newest_rows_with_medians() {
        let mut world = World::new();
        let empty = pacing_trace(&mut world, &json!({})).expect("empty");
        assert_eq!(empty["summary"]["frames"], 0);
        assert_eq!(empty["summary"]["fps"], Value::Null);
        assert_eq!(empty["summary"]["breakdown"], Value::Null);
        let mut trace = FramePacingTrace::default();
        for i in 0..10_u64 {
            trace.push(PacingTraceRow {
                start_ns: 1_000_000_000 + i * 20_000_000,
                cost_ns: 12_000_000,
                swapchain_wait_ns: 4_000_000,
                waited_ns: 8_000_000,
                release_tick_ns: Some(1_000_000_000 + (i + 1) * 20_000_000),
                ticks: 2,
                period_ns: Some(10_000_000),
                interval: 2,
                hint_hz: Some(50.0),
                paced: true,
                ..PacingTraceRow::default()
            });
        }
        world.insert_resource(trace);
        let got = pacing_trace(&mut world, &json!({ "frames": 5, "log": true })).expect("trace");
        assert_eq!(got["log"], true);
        let rows = got["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0]["start_ms"], 0.0, "times start at the first row returned");
        assert_eq!(rows[4]["start_ms"], 80.0);
        assert_eq!(rows[0]["work_ms"], 8.0);
        assert_eq!(rows[0]["release_tick_ms"], 20.0);
        let summary = &got["summary"];
        assert_eq!(summary["frames"], 5);
        assert_eq!(summary["paced"], 5);
        assert_eq!(summary["gap_p50_ms"], 20.0);
        assert_eq!(summary["fps"], 50.0);
        assert_eq!(summary["swapchain_wait_p50_ms"], 4.0);
        // 20 ms between starts: 8 work, 4 swapchain, 8 waiting for the tick, nothing outside.
        let split = &summary["breakdown"];
        assert_eq!(split["intervals"], 4);
        assert_eq!(split["work_p50_ms"], 8.0);
        assert_eq!(split["wait_p50_ms"], 8.0);
        assert_eq!(split["loop_p50_ms"], 0.0);
        assert_eq!(split["vsyncs_per_frame"], 2.0);
        assert_eq!(split["missed_ticks"], 0);
        assert_eq!(split["limit"], "work", "a tie goes to the work");
        assert_eq!(rows[0]["missed_ticks"], 0);
        assert!(world.resource::<FramePacingTrace>().log);
        let cleared = pacing_trace(&mut world, &json!({ "clear": true, "log": false })).expect("clear");
        assert_eq!(
            cleared["summary"]["frames"], 10,
            "the answer comes before the clear"
        );
        assert_eq!(world.resource::<FramePacingTrace>().rows().len(), 0);
        assert!(!world.resource::<FramePacingTrace>().log);
        for bad in [
            json!({ "frames": 0 }),
            json!({ "frames": 5000 }),
            json!({ "frames": "x" }),
            json!({ "log": "yes" }),
            json!({ "clear": 1 }),
        ] {
            assert!(pacing_trace(&mut world, &bad).is_err(), "{bad}");
        }
    }
}
