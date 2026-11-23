// SPDX-License-Identifier: MPL-2.0
//! Playing and testing (specs/23 § 3.5): pause/step/resume, time scale, raw input and action
//! injection, `wait_until` predicates, `expect` assertions and diagnostics.

use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use regia_app::prelude::*;
use regia_platform::{
    BackPressed, ButtonState, CursorMoved, KeyboardInput, MouseButton, MouseButtonInput, MouseWheel,
    PrimaryWindow, TouchInput, TouchPhase, WheelUnit,
};
use serde_json::{Value, json};

use crate::protocol::Response;
use crate::protocol::RpcError;

/// Pause/step state driven by `play.pause`, `play.resume`, `play.step`.
#[derive(Resource, Default)]
pub struct AgentTimeControl {
    /// Whether the game was paused by an agent before stepping.
    pub(crate) paused: bool,
    /// A running step: frames left, the request id, the reply channel.
    pub(crate) stepping: Option<(u64, Value, Sender<Response>)>,
    /// The frame a running step was dispatched on when that frame ran no game time: the server
    /// dispatches in `PreUpdate`, after `First` took a paused frame's zero delta, so that frame
    /// is not one of the stepped frames (ADR-1411).
    pub(crate) step_dispatch_frame: Option<u64>,
    /// Frames that ran game time (a nonzero delta): `play.get_diagnostics`' `game_frames`, so a
    /// caller in another process sees a step advance exactly one (ADR-7831).
    pub(crate) game_frames: u64,
    /// Raw events `play.inject_input` wrote: `play.get_diagnostics`' `injected_events`.
    pub(crate) injected_events: u64,
    /// Of those, key events: `play.get_diagnostics`' `injected_keys`.
    pub(crate) injected_keys: u64,
}

/// One pending `play.wait_until` call.
struct Waiter {
    /// The condition to poll.
    condition: Value,
    /// When to give up.
    deadline: Instant,
    /// When the wait started.
    started: Instant,
    /// The last evaluation error (reported on timeout).
    last_error: Option<String>,
    /// JSON-RPC id.
    id: Value,
    /// Reply channel.
    reply: Sender<Response>,
}

/// Pending `play.wait_until` calls, polled by [`advance_waiters`].
#[derive(Resource, Default)]
pub struct WaitQueue {
    waiters: Vec<Waiter>,
}

/// A render-out owns the clock until it ends: its frames, manifest and audio are all stated in the
/// request's time, so a pause or a scale here would make them disagree.
fn refuse_during_render_out(world: &World) -> Result<(), RpcError> {
    if crate::render_out::job_running(world) {
        return Err(RpcError::new(
            crate::protocol::INVALID_REQUEST,
            "F0018",
            "a render-out is running",
            "render.out_progress shows it; render.out_cancel stops it",
        ));
    }
    Ok(())
}

/// `play.pause` / `play.resume`.
pub(crate) fn set_paused(world: &mut World, paused: bool) -> Result<Value, RpcError> {
    refuse_during_render_out(world)?;
    world.resource_mut::<AgentTimeControl>().paused = paused;
    world.resource_mut::<Time>().paused = paused;
    Ok(json!({ "paused": paused, "frame": world.resource::<Time>().frame_count() }))
}

/// Most `play.wait_until` calls pending at once; each is re-evaluated every frame (ADR-1034).
const MAX_WAITERS: usize = 64;

/// Largest `frames` one `play.step` accepts.
const MAX_STEP_FRAMES: u64 = 10_000;

/// Parses `play.step`'s `frames` (default 1, `1..=MAX_STEP_FRAMES`).
fn step_frames(params: &Value) -> Result<u64, RpcError> {
    match params.get("frames") {
        None | Some(Value::Null) => Ok(1),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_STEP_FRAMES).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`frames` must be an integer in 1..={MAX_STEP_FRAMES}"))
            }),
    }
}

/// Upper bound for a scheduled action's `at_frame` and `duration_frames`: their sum is compared
/// against the frame counter every frame and must not overflow.
const MAX_SCHEDULE_FRAME: u64 = u64::MAX / 4;

/// Highest `play.set_time_scale`: `Time::advance` scales the frame's `Duration` by it, which
/// panics once the product leaves `Duration`'s range (an infinite or astronomically large scale).
const MAX_TIME_SCALE: f64 = 1000.0;

/// `play.step`: run `frames` frames of game time, then answer.
pub(crate) fn step(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Result<(), RpcError> {
    refuse_during_render_out(world)?;
    let frames = step_frames(params)?;
    let (frame, idle) = {
        let time = world.resource::<Time>();
        (time.frame_count(), time.delta().is_zero())
    };
    let mut control = world.resource_mut::<AgentTimeControl>();
    if control.stepping.is_some() {
        return Err(RpcError::new(
            crate::protocol::INVALID_REQUEST,
            "F0009",
            "a step is already running",
            "wait for it to finish",
        ));
    }
    control.paused = true;
    control.stepping = Some((frames, id, reply));
    // Dispatched mid-frame on a frame that took no game time (paused when `First` ran): the
    // countdown starts with the next frame, so `frames` frames of game time run.
    control.step_dispatch_frame = idle.then_some(frame);
    world.resource_mut::<Time>().paused = false;
    Ok(())
}

/// `Last`: counts down a running `play.step` and pauses again when it ends. The frame the step
/// was dispatched on does not count when it ran no game time (see `step`).
pub fn advance_step(mut control: ResMut<AgentTimeControl>, mut time: ResMut<Time>) {
    if !time.delta().is_zero() {
        control.game_frames += 1;
    }
    let dispatch_frame = control.step_dispatch_frame;
    let Some((remaining, _, _)) = control.stepping.as_mut() else {
        return;
    };
    if dispatch_frame == Some(time.frame_count()) {
        return;
    }
    *remaining -= 1;
    if *remaining > 0 {
        return;
    }
    control.step_dispatch_frame = None;
    if let Some((_, id, reply)) = control.stepping.take() {
        time.paused = control.paused;
        let _ = reply.send(Response::ok(
            id,
            json!({ "frame": time.frame_count(), "paused": time.paused }),
        ));
    }
}

/// `play.set_time_scale`.
pub(crate) fn set_time_scale(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    refuse_during_render_out(world)?;
    let scale = crate::params::opt_f64(params, "scale")?
        .filter(|s| s.is_finite() && (0.0..=MAX_TIME_SCALE).contains(s))
        .ok_or_else(|| RpcError::invalid_params(format!("`scale` (0..={MAX_TIME_SCALE}) is required")))?;
    // Within MAX_TIME_SCALE the f32 is finite (an infinity would panic Duration::mul_f32).
    let narrowed = scale as f32;
    let mut time = world.resource_mut::<Time>();
    let before = f64::from(time.time_scale);
    time.time_scale = narrowed;
    Ok(json!({ "before": before, "after": f64::from(time.time_scale) }))
}

/// Raw input events → engine messages. Shared by `play.inject_input`.
fn state_of(event: &Value) -> Result<ButtonState, RpcError> {
    Ok(match crate::params::opt_str(event, "state")? {
        Some("released" | "up") => ButtonState::Released,
        _ => ButtonState::Pressed,
    })
}

/// A numeric event field, 0 when absent.
fn event_number(event: &Value, key: &str) -> Result<f64, RpcError> {
    Ok(crate::params::opt_f64(event, key)?.unwrap_or(0.0))
}

/// Reads every optional field of a raw input event once, so a wrong-typed field is refused
/// before the batch writes anything (ADR-2622).
fn check_event_fields(event: &Value) -> Result<(), RpcError> {
    for key in ["key", "text", "button", "phase", "state"] {
        crate::params::opt_str(event, key)?;
    }
    crate::params::opt_u64(event, "id")?;
    let scancode = crate::params::opt_u64(event, "scancode")?;
    for key in ["x", "y", "dx", "dy"] {
        crate::params::opt_f64(event, key)?;
    }
    // A physical key is a scancode. Text on that event is refused before the batch writes
    // anything (ADR-8452). A key that omits `scancode` is unchanged.
    if scancode.is_some() && crate::params::opt_str(event, "text")?.is_some_and(|text| !text.is_empty()) {
        return Err(RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "play_gate_text",
            "a physical key event has no text",
            "send scancode and state, and leave text out",
        ));
    }
    if scancode.is_some_and(|code| code > u64::from(u32::MAX)) {
        return Err(RpcError::invalid_params("scancode does not fit in u32"));
    }
    Ok(())
}

fn primary_window(world: &mut World) -> Result<Entity, RpcError> {
    let mut q = world.query_filtered::<Entity, With<PrimaryWindow>>();
    q.iter(world).next().ok_or_else(|| {
        RpcError::not_found(
            "no primary window yet",
            "wait for the app to resume (lifecycle Started)",
        )
    })
}

/// An action value: a number (x), `[x, y(, z)]` or `{x, y(, z)}`.
fn action_value(value: &Value) -> Result<regia_core::math::Vec3, RpcError> {
    use regia_core::math::Vec3;
    let f = |v: &Value| v.as_f64().map(|n| n as f32);
    Ok(match value {
        Value::Number(n) => Vec3::new(n.as_f64().unwrap_or(0.0) as f32, 0.0, 0.0),
        Value::Array(items) => Vec3::new(
            items.first().and_then(f).unwrap_or(0.0),
            items.get(1).and_then(f).unwrap_or(0.0),
            items.get(2).and_then(f).unwrap_or(0.0),
        ),
        Value::Object(map) => Vec3::new(
            map.get("x").and_then(f).unwrap_or(0.0),
            map.get("y").and_then(f).unwrap_or(0.0),
            map.get("z").and_then(f).unwrap_or(0.0),
        ),
        other => {
            return Err(RpcError::invalid_params(format!(
                "action value must be a number, [x, y(, z)] or {{x, y(, z)}}: {other}"
            )));
        }
    })
}

/// `play.inject_input`: raw events and/or scheduled action values.
pub(crate) fn inject_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let events = crate::params::opt_array(params, "events")?
        .cloned()
        .unwrap_or_default();
    let actions = crate::params::opt_array(params, "actions")?
        .cloned()
        .unwrap_or_default();
    if events.is_empty() && actions.is_empty() {
        return Err(RpcError::invalid_params(
            "`events` (raw input events) and/or `actions` ({action, value, at_frame?, duration_frames?}) are required",
        ));
    }
    // Validate the whole batch before writing anything, so a rejected call injects nothing.
    for (i, e) in events.iter().enumerate() {
        check_event_fields(e)?;
        let kind = crate::params::opt_str(e, "type")?;
        if !matches!(
            kind,
            Some("key" | "mouse_button" | "cursor" | "wheel" | "touch" | "back")
        ) {
            return Err(RpcError::invalid_params(format!(
                "event {i}: unknown type {kind:?} (key, mouse_button, cursor, wheel, touch, back)"
            )));
        }
    }
    let frame = world.resource::<Time>().frame_count();
    let mut parsed_actions = Vec::with_capacity(actions.len());
    for (i, action) in actions.iter().enumerate() {
        let name = crate::params::opt_str(action, "action")?
            .ok_or_else(|| RpcError::invalid_params(format!("actions[{i}]: `action` is required")))?;
        let default_value = Value::from(0);
        let value = action_value(action.get("value").unwrap_or(&default_value))?;
        // Clamped so `at_frame + duration` cannot overflow in the injection queue (ADR-1034).
        let at_frame = crate::params::opt_u64(action, "at_frame")?
            .unwrap_or(frame)
            .min(frame.saturating_add(MAX_SCHEDULE_AHEAD_FRAMES))
            .min(MAX_SCHEDULE_FRAME);
        let duration = crate::params::opt_u64(action, "duration_frames")?
            .unwrap_or(1)
            .min(MAX_HOLD_FRAMES);
        parsed_actions.push((name, value, at_frame, duration));
    }
    if !actions.is_empty() && world.get_resource::<regia_input::InputInjection>().is_none() {
        return Err(RpcError::new(
            crate::protocol::UNSUPPORTED,
            "F0013",
            "no InputInjection in this app",
            "action injection needs regia_input::InputPlugin",
        ));
    }
    if !events.is_empty() {
        // Raw events target a window; scheduled actions do not need one.
        let window = primary_window(world)?;
        for (i, e) in events.iter().enumerate() {
            match crate::params::opt_str(e, "type")? {
                Some("key") => {
                    let scancode = crate::params::opt_u64(e, "scancode")?;
                    let text = crate::params::opt_str(e, "text")?;
                    let physical = if let Some(code) = scancode {
                        format!("scancode:{code}")
                    } else {
                        crate::params::opt_str(e, "key")?
                            .unwrap_or("Unidentified")
                            .to_string()
                    };
                    // A scancode is the physical key. Its logical key stays empty. A name with
                    // no text still copies the name, as before.
                    let logical = if scancode.is_some() {
                        String::new()
                    } else {
                        text.map(str::to_string).unwrap_or_else(|| physical.clone())
                    };
                    world.write_message(KeyboardInput {
                        window,
                        physical_key: physical,
                        logical_key: logical,
                        state: state_of(e)?,
                        repeat: false,
                        timestamp: regia_platform::InputTimestamp::ZERO,
                    });
                }
                Some("mouse_button") => {
                    let button = match crate::params::opt_str(e, "button")?.unwrap_or("left") {
                        "right" => MouseButton::Right,
                        "middle" => MouseButton::Middle,
                        "back" => MouseButton::Back,
                        "forward" => MouseButton::Forward,
                        _ => MouseButton::Left,
                    };
                    world.write_message(MouseButtonInput {
                        window,
                        button,
                        state: state_of(e)?,
                        timestamp: regia_platform::InputTimestamp::ZERO,
                    });
                }
                Some("cursor") => {
                    world.write_message(CursorMoved {
                        window,
                        position: (event_number(e, "x")?, event_number(e, "y")?),
                        timestamp: regia_platform::InputTimestamp::ZERO,
                    });
                }
                Some("wheel") => {
                    world.write_message(MouseWheel {
                        window,
                        delta: (event_number(e, "dx")? as f32, event_number(e, "dy")? as f32),
                        unit: WheelUnit::Line,
                        timestamp: regia_platform::InputTimestamp::ZERO,
                    });
                }
                Some("touch") => {
                    let phase = match crate::params::opt_str(e, "phase")?.unwrap_or("started") {
                        "moved" => TouchPhase::Moved,
                        "ended" => TouchPhase::Ended,
                        "cancelled" => TouchPhase::Cancelled,
                        _ => TouchPhase::Started,
                    };
                    let id = crate::params::opt_u64(e, "id")?.unwrap_or(0);
                    world.write_message(TouchInput {
                        window,
                        id,
                        phase,
                        position: (event_number(e, "x")?, event_number(e, "y")?),
                        timestamp: regia_platform::InputTimestamp::ZERO,
                    });
                }
                Some("back") => {
                    world.write_message(BackPressed);
                }
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "event {i}: unknown type {other:?} (key, mouse_button, cursor, wheel, touch, back)"
                    )));
                }
            }
        }
    }
    if let Some(mut control) = world.get_resource_mut::<AgentTimeControl>() {
        let keys = events
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("key"))
            .count() as u64;
        control.injected_events += events.len() as u64;
        control.injected_keys += keys;
    }
    let mut scheduled = 0;
    if let Some(mut injection) = world.get_resource_mut::<regia_input::InputInjection>() {
        for (name, value, at_frame, duration) in parsed_actions {
            injection.schedule(name, value, at_frame, duration);
            scheduled += 1;
        }
    }
    Ok(json!({
        "injected": events.len(),
        "scheduled": scheduled,
        "frame": frame,
    }))
}

/// Furthest ahead `play.inject_input` schedules an action (about 46 hours at 60 Hz).
const MAX_SCHEDULE_AHEAD_FRAMES: u64 = 10_000_000;
/// Longest an injected action is held (about 4.6 hours at 60 Hz).
const MAX_HOLD_FRAMES: u64 = 1_000_000;

/// What an evaluation found: the verdict, a human-readable detail line for reports, and the
/// error when the condition could not be evaluated at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Does the condition hold?
    pub holds: bool,
    /// One-line explanation ("Transform.translation.y = 2.1, expected gte 2.0").
    pub detail: String,
    /// Why the condition could not be evaluated.
    pub error: Option<String>,
}

impl Finding {
    fn holds(detail: impl Into<String>) -> Self {
        Self {
            holds: true,
            detail: detail.into(),
            error: None,
        }
    }
    fn fails(detail: impl Into<String>) -> Self {
        Self {
            holds: false,
            detail: detail.into(),
            error: None,
        }
    }
    fn broken(error: impl Into<String>) -> Self {
        let error = error.into();
        Self {
            holds: false,
            detail: error.clone(),
            error: Some(error),
        }
    }
}

/// Compares a serialised value against the expected one with a predicate op.
fn compare(actual: &Value, op: &str, expected: &Value) -> Result<bool, String> {
    let numbers = (actual.as_f64(), expected.as_f64());
    let ok = match op {
        "eq" => match numbers {
            (Some(a), Some(b)) => {
                // Relative f32 tolerance: a value stored as f32 equals the f64 the caller typed.
                a == b || (a - b).abs() <= f64::from(f32::EPSILON) * a.abs().max(b.abs()).max(1.0)
            }
            _ => actual == expected,
        },
        "ne" => !compare(actual, "eq", expected)?,
        "gt" | "gte" | "lt" | "lte" => {
            let (Some(a), Some(b)) = numbers else {
                return Err(format!("op {op:?} needs numbers, got {actual} vs {expected}"));
            };
            match op {
                "gt" => a > b,
                "gte" => a >= b,
                "lt" => a < b,
                _ => a <= b,
            }
        }
        other => return Err(format!("unknown op {other:?} (eq, ne, gt, gte, lt, lte)")),
    };
    Ok(ok)
}

/// A dotted path into a JSON value ("translation.y"); numeric parts index arrays ("texts.0").
fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        current = match (current, part.parse::<usize>()) {
            (Value::Array(items), Ok(index)) => items.get(index)?,
            _ => current.get(part)?,
        };
    }
    Some(current)
}

/// A JSON snapshot of a well-known resource, for `resource` predicates. Arbitrary reflected
/// resources are listed by `resources.list`; predicate access goes through the typed handlers
/// the engine already exposes.
fn resource_snapshot(world: &mut World, name: &str) -> Result<Value, String> {
    match name {
        "GameSettings" => crate::framework::settings_get(world, &json!({}))
            .map(|v| v["settings"].clone())
            .map_err(|e| e.message),
        "SimCounters" => crate::framework::sim_counters(world)
            .map(|v| {
                let mut object = serde_json::Map::new();
                if let Some(map) = v.as_object() {
                    for (k, val) in map {
                        object.insert(k.clone(), val.clone());
                    }
                }
                Value::Object(object)
            })
            .map_err(|e| e.message),
        "Diagnostics" => Ok(diagnostics_value(world)),
        "StreamStats" => Ok(crate::stream::stats_snapshot(world)),
        "QuickDraw" => crate::quick::snapshot(world),
        "QuickDraw2d" => crate::quick::draw2d_snapshot(world),
        "QuickModel" => crate::quick_app::model_snapshot(world),
        "QuickLoop" => crate::quick_app::loop_snapshot(world),
        "Time" => {
            let time = world.resource::<Time>();
            Ok(json!({
                "frame": time.frame_count(),
                "elapsed_secs": time.elapsed_secs(),
                "time_scale": time.time_scale,
                "paused": time.paused,
            }))
        }
        // The content manager's requests as `content.progress` answers, in id order
        // (spec 40 § 5 recipe step 3 waits on `requests.0.state == "Mounted"`).
        "Content" => {
            let Some(manager) = world.get_resource::<regia_asset::content::ContentManager>() else {
                return Err("no ContentManager in this app".to_owned());
            };
            let requests: Vec<Value> = manager
                .request_snapshots()
                .into_iter()
                .map(|(id, p)| {
                    json!({
                        "request": id,
                        "state": p.state,
                        "done": p.done,
                        "total": p.total,
                        "chunks": p.chunks.iter().map(|(name, state, done, total)| {
                            json!({
                                "name": name,
                                "state": state.label(),
                                "done": done,
                                "total": total,
                            })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect();
            Ok(json!({
                "build_id": manager.manifest().build_id,
                "requests": requests,
            }))
        }
        other => Err(format!(
            "resource {other:?} has no predicate snapshot — supported: GameSettings, StreamStats, \
             SimCounters, Diagnostics, Time, QuickDraw, QuickDraw2d, QuickModel, QuickLoop, Content"
        )),
    }
}

/// The Diagnostics resource as JSON (also `play.get_diagnostics`).
fn diagnostics_value(world: &World) -> Value {
    let mut out = diagnostics_base(world);
    // Game frames and injected input, for a caller in another process (ADR-7831).
    if let (Some(control), Value::Object(map)) = (world.get_resource::<AgentTimeControl>(), &mut out) {
        map.insert("game_frames".into(), json!(control.game_frames));
        map.insert("injected_events".into(), json!(control.injected_events));
        map.insert("injected_keys".into(), json!(control.injected_keys));
    }
    out
}

/// The Diagnostics fields of [`diagnostics_value`].
fn diagnostics_base(world: &World) -> Value {
    let time = world.resource::<Time>();
    match world.get_resource::<regia_app::Diagnostics>() {
        Some(d) => json!({
            "fps": d.fps,
            "frame_time_avg_ms": d.frame_time_avg.as_secs_f64() * 1000.0,
            "fps_1pct_low": d.fps_1pct_low,
            "entity_count": d.entity_count,
            "draw_calls": d.draw_calls,
            "allocated_bytes": d.allocated_bytes,
            "frames": d.frames,
            "paused": time.paused,
        }),
        None => json!({
            "note": "no Diagnostics in this app (add regia_app::DiagnosticsPlugin)",
            "frame": time.frame_count(),
            "elapsed_secs": time.elapsed_secs(),
            "paused": time.paused,
        }),
    }
}

/// Unwraps a typed parameter read inside [`evaluate`]; a wrong-typed field makes the condition
/// `broken` with the `invalid_params` message (ADR-2622).
macro_rules! cond {
    ($read:expr) => {
        match $read {
            Ok(value) => value,
            Err(error) => return Finding::broken(error.message),
        }
    };
}

/// Evaluates one condition (atoms and combinators, see the guide's testing page).
pub(crate) fn evaluate(world: &mut World, condition: &Value) -> Finding {
    let Some(fields) = condition.as_object() else {
        return Finding::broken(format!("condition must be an object: {condition}"));
    };
    // Combinators.
    if let Some(all) = cond!(crate::params::opt_array(fields, "all")) {
        let mut detail = String::new();
        for sub in all {
            let finding = evaluate(world, sub);
            detail.push_str(&finding.detail);
            if !finding.holds {
                return Finding {
                    holds: false,
                    detail,
                    error: finding.error,
                };
            }
        }
        return Finding::holds(detail);
    }
    if let Some(any) = cond!(crate::params::opt_array(fields, "any")) {
        let mut detail = String::new();
        let mut errors = Vec::new();
        for sub in any {
            let finding = evaluate(world, sub);
            detail.push_str(&finding.detail);
            if finding.holds {
                return Finding::holds(format!("any of: {detail}"));
            }
            errors.extend(finding.error);
        }
        return Finding::fails(format!("none of: {detail}")).with_errors(errors);
    }
    if let Some(not) = fields.get("not") {
        let finding = evaluate(world, not);
        if finding.error.is_some() {
            return finding; // a broken condition does not become true by negation
        }
        return Finding {
            holds: !finding.holds,
            detail: format!("not({})", finding.detail),
            error: None,
        };
    }
    // Tween atom: { predicate: "ui.tweening", entity?, is? } (book/agents/ui-animation.md).
    if cond!(crate::params::opt_str(fields, "predicate")) == Some("ui.tweening") {
        return match crate::ui_tween::predicate(world, condition) {
            Ok((true, detail)) => Finding::holds(detail),
            Ok((false, detail)) => Finding::fails(detail),
            Err(error) => Finding::broken(error),
        };
    }
    // Theme fade atom: { predicate: "ui.theme_fading", is? } (book/agents/ui-theme-switch.md).
    if cond!(crate::params::opt_str(fields, "predicate")) == Some("ui.theme_fading") {
        return match crate::ui_theme_mode::predicate(world, condition) {
            Ok((true, detail)) => Finding::holds(detail),
            Ok((false, detail)) => Finding::fails(detail),
            Err(error) => Finding::broken(error),
        };
    }
    // UI atoms: { predicate: "ui.node_exists" | "ui.node_state", ...filters } (book/agents/ui.md).
    if let Some(name) =
        cond!(crate::params::opt_str(fields, "predicate")).filter(|name| name.starts_with("ui."))
    {
        return match crate::ui::predicate(world, name, condition) {
            Ok((true, detail)) => Finding::holds(detail),
            Ok((false, detail)) => Finding::fails(detail),
            Err(error) => Finding::broken(error),
        };
    }
    // Exposure atom: { predicate: "exposure_settled", tolerance? } (book/agents/exposure-zones.md).
    if cond!(crate::params::opt_str(fields, "predicate")) == Some("exposure_settled") {
        return match crate::exposure::settled_predicate(world, condition) {
            Ok((true, detail)) => Finding::holds(detail),
            Ok((false, detail)) => Finding::fails(detail),
            Err(error) => Finding::broken(error),
        };
    }
    // Distance atom: { a, b, op: within|beyond, meters }.
    if let Some(op) =
        cond!(crate::params::opt_str(fields, "op")).filter(|op| *op == "within" || *op == "beyond")
    {
        let (Some(a), Some(b), Some(meters)) = (
            fields.get("a"),
            fields.get("b"),
            cond!(crate::params::opt_f64(fields, "meters")),
        ) else {
            return Finding::broken("distance atom needs a, b and meters");
        };
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let pa = match crate::scene::resolve_entity(world, a).and_then(|e| {
            crate::scene::translation_of(world, &registry, e)
                .ok_or_else(|| "a: entity has no Transform".to_string())
        }) {
            Ok(p) => p,
            Err(e) => return Finding::broken(e),
        };
        let pb = match crate::scene::resolve_entity(world, b).and_then(|e| {
            crate::scene::translation_of(world, &registry, e)
                .ok_or_else(|| "b: entity has no Transform".to_string())
        }) {
            Ok(p) => p,
            Err(e) => return Finding::broken(e),
        };
        let distance = ((pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2)).sqrt();
        let holds = if op == "within" {
            distance <= meters
        } else {
            distance >= meters
        };
        return if holds {
            Finding::holds(format!("distance = {distance:.3} m, {op} {meters} m"))
        } else {
            Finding::fails(format!("distance = {distance:.3} m, {op} {meters} m"))
        };
    }
    // Resource atom: { resource, path?, op, value }.
    if let Some(resource) = cond!(crate::params::opt_str(fields, "resource")) {
        return evaluate_snapshot(
            resource_snapshot(world, resource),
            resource,
            cond!(crate::params::opt_str(fields, "path")).unwrap_or(""),
            cond!(crate::params::opt_str(fields, "op")).unwrap_or("eq"),
            fields.get("value"),
        );
    }
    // State atom: { state, op?, value } — the state's current value, null while it does not exist.
    if let Some(state) = cond!(crate::params::opt_str(fields, "state")) {
        return evaluate_snapshot(
            crate::states::state_snapshot(world, state),
            state,
            "",
            cond!(crate::params::opt_str(fields, "op")).unwrap_or("eq"),
            fields.get("value"),
        );
    }
    // Logs atom: { logs_contain: text }.
    if let Some(text) = cond!(crate::params::opt_str(fields, "logs_contain")) {
        use regia_core::log::ring::{LogQuery, recent};
        let query = LogQuery {
            since: None,
            min_level: None,
            target: None,
            contains: Some(text.to_string()),
            limit: Some(usize::MAX),
        };
        let (records, _) = recent(&query);
        let count = records.len();
        return if count > 0 {
            Finding::holds(format!("logs contain {text:?} ({count} records)"))
        } else {
            Finding::fails(format!("logs do not contain {text:?}"))
        };
    }
    // Events atom: { events_contain: text } or { events_contain: { kind?, contains? } }.
    if let Some(events) = fields.get("events_contain") {
        let (kind, contains) = match events {
            Value::String(text) => (None, Some(text.clone())),
            Value::Object(map) => (
                cond!(crate::params::opt_str(map, "kind")).map(String::from),
                cond!(crate::params::opt_str(map, "contains")).map(String::from),
            ),
            _ => return Finding::broken(format!("events_contain must be text or an object: {events}")),
        };
        let log = world
            .get_resource::<crate::events::AgentEventLog>()
            .map(|l| l.records_snapshot(usize::MAX))
            .unwrap_or_default();
        let matched = log
            .iter()
            .filter(|r| kind.as_deref().is_none_or(|k| r.kind == k))
            .filter(|r| {
                contains
                    .as_deref()
                    .is_none_or(|c| serde_json::to_string(&r.data).unwrap_or_default().contains(c))
            })
            .count();
        return if matched > 0 {
            Finding::holds(format!("events match ({matched} records)"))
        } else {
            Finding::fails("no recorded event matches")
        };
    }
    // Field atom: { entity, component, path?, op, value }.
    let (Some(component), Some(op), expected) = (
        cond!(crate::params::opt_str(fields, "component")),
        cond!(crate::params::opt_str(fields, "op")),
        fields.get("value"),
    ) else {
        return Finding::broken(format!(
            "not a condition: expected entity/component/op, resource, state, logs_contain or \
             events_contain — {condition}"
        ));
    };
    let Some(entity_ref) = fields.get("entity") else {
        return Finding::broken(format!("field atom needs `entity`: {condition}"));
    };
    let entity = match crate::scene::resolve_entity(world, entity_ref) {
        Ok(entity) => entity,
        Err(e) => return Finding::broken(e),
    };
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let resolved = match crate::scene::resolve_component(&registry, component) {
        Ok(resolved) => resolved,
        Err(e) => return Finding::broken(e.message),
    };
    let Ok(entity_cell) = world.get_entity(entity) else {
        return Finding::broken(format!("entity {} does not exist", entity.to_bits()));
    };
    let Some(value) = resolved.data.reflect(entity_cell) else {
        return Finding::broken(format!("entity has no {component}"));
    };
    // The path navigates reflection field names ("translation.y"), so it works for struct
    // fields however the leaf serialises.
    let path = cond!(crate::params::opt_str(fields, "path")).unwrap_or("");
    let leaf: &dyn regia_ecs::reflect::PartialReflect = if path.is_empty() {
        value.as_partial_reflect()
    } else {
        match value.reflect_path(path) {
            Ok(leaf) => leaf,
            Err(_) => return Finding::broken(format!("no field `{path}` in {component}")),
        }
    };
    let actual = match crate::scene::serialize_reflect(leaf, &registry) {
        Ok(actual) => actual,
        Err(e) => return Finding::broken(format!("cannot serialize {component}.{path}: {e}")),
    };
    let Some(expected) = expected else {
        return Finding::broken("field atom needs `value` to compare against");
    };
    let holds = match compare(&actual, op, expected) {
        Ok(holds) => holds,
        Err(e) => return Finding::broken(e),
    };
    let target = if path.is_empty() {
        component.to_string()
    } else {
        format!("{component}.{path}")
    };
    if holds {
        Finding::holds(format!("{target} = {actual}, {op} {expected}"))
    } else {
        Finding::fails(format!("{target} = {actual}, expected {op} {expected}"))
    }
}

impl Finding {
    /// Attaches evaluation errors to a failing finding (for the `any` combinator).
    fn with_errors(mut self, errors: Vec<String>) -> Self {
        if let Some(error) = errors.into_iter().next() {
            self.error = Some(error);
        }
        self
    }
}

/// Compares a snapshot field against the expectation (shared by the resource atom).
fn evaluate_snapshot(
    snapshot: Result<Value, String>,
    resource: &str,
    path: &str,
    op: &str,
    expected: Option<&Value>,
) -> Finding {
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(e) => return Finding::broken(e),
    };
    let actual = if path.is_empty() {
        snapshot.clone()
    } else {
        match json_path(&snapshot, path) {
            Some(v) => v.clone(),
            None => return Finding::broken(format!("no field `{path}` in {resource}")),
        }
    };
    let Some(expected) = expected else {
        return Finding::broken(format!("resource atom needs `value`: {actual}"));
    };
    let holds = match compare(&actual, op, expected) {
        Ok(holds) => holds,
        Err(e) => return Finding::broken(e),
    };
    let target = if path.is_empty() {
        resource.to_string()
    } else {
        format!("{resource}.{path}")
    };
    if holds {
        Finding::holds(format!("{target} = {actual}, {op} {expected}"))
    } else {
        Finding::fails(format!("{target} = {actual}, expected {op} {expected}"))
    }
}

/// `play.wait_until`: defer the reply until the condition holds or the deadline passes.
pub(crate) fn wait_until(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Result<(), RpcError> {
    let Some(condition) = params.get("condition").filter(|c| c.is_object()) else {
        return Err(RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0014",
            "`condition` (a predicate object, see the guide's testing page) is required",
            "atoms: { entity, component, path, op, value }, { a, b, op: within|beyond, meters }, \
             { resource, path, op, value }, { logs_contain }, { events_contain }; combinators: \
             { all }, { any }, { not }",
        ));
    };
    let timeout = Duration::from_millis(
        crate::params::opt_u64(params, "timeout_ms")?
            .unwrap_or(10_000)
            .min(120_000),
    );
    let pending = world
        .get_resource::<WaitQueue>()
        .map_or(0, |queue| queue.waiters.len());
    if pending >= MAX_WAITERS {
        return Err(crate::seeing::too_many_pending("play.wait_until", MAX_WAITERS));
    }
    world
        .get_resource_or_insert_with(WaitQueue::default)
        .waiters
        .push(Waiter {
            condition: condition.clone(),
            deadline: Instant::now() + timeout,
            started: Instant::now(),
            last_error: None,
            id,
            reply,
        });
    Ok(())
}

/// `Last`: polls every pending `wait_until` condition; answers when it holds or times out.
pub fn advance_waiters(world: &mut World) {
    let mut queue = world.remove_resource::<WaitQueue>().unwrap_or_default();
    let frame = world.resource::<Time>().frame_count();
    let mut remove = Vec::new();
    for (index, waiter) in queue.waiters.iter_mut().enumerate() {
        let finding = evaluate(world, &waiter.condition);
        waiter.last_error = finding.error.clone();
        if finding.holds {
            let waited = waiter.started.elapsed();
            let _ = waiter.reply.send(Response::ok(
                waiter.id.clone(),
                json!({
                    "satisfied": true,
                    "frame": frame,
                    "waited_ms": waited.as_millis() as u64,
                    "detail": finding.detail,
                }),
            ));
            remove.push(index);
        } else if Instant::now() >= waiter.deadline {
            let waited = waiter.started.elapsed();
            let _ = waiter.reply.send(Response::ok(
                waiter.id.clone(),
                json!({
                    "satisfied": false,
                    "frame": frame,
                    "waited_ms": waited.as_millis() as u64,
                    "last_error": waiter.last_error,
                    "detail": finding.detail,
                }),
            ));
            remove.push(index);
        }
    }
    for index in remove.into_iter().rev() {
        queue.waiters.remove(index);
    }
    world.insert_resource(queue);
}

/// `play.expect`: evaluate assertions now and report pass/fail per assertion.
pub(crate) fn expect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let assertions = crate::params::opt_array(params, "assertions")?
        .filter(|a| !a.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`assertions` (a non-empty array) is required"))?;
    let mut results = Vec::new();
    let mut passed = 0;
    let mut failed = 0;
    for (index, assertion) in assertions.iter().enumerate() {
        let name = crate::params::opt_str(assertion, "name")?
            .map(String::from)
            .unwrap_or_else(|| format!("assertion {index}"));
        let Some(condition) = assertion.get("condition") else {
            return Err(RpcError::invalid_params(format!(
                "assertions[{index}] ({name}) needs a `condition`"
            )));
        };
        let finding = evaluate(world, condition);
        let entry = json!({
            "name": name,
            "pass": finding.holds,
            "detail": finding.detail,
            "error": finding.error,
        });
        if finding.holds {
            passed += 1;
        } else {
            failed += 1;
        }
        results.push(entry);
    }
    Ok(json!({
        "results": results,
        "passed": passed,
        "failed": failed,
        "pass": failed == 0,
    }))
}

/// `play.get_diagnostics`.
pub(crate) fn get_diagnostics(world: &mut World) -> Value {
    diagnostics_value(world)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::{Quat, Vec3};
    use regia_scene::Transform;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_framework::settings::SettingsPlugin);
        app.insert_resource(regia_framework::settings::SettingsPath(None));
        app.register_type::<Transform>();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        // The plugin's own wiring, minus the server: step/wait_until are driven by these.
        app.init_resource::<AgentTimeControl>()
            .add_systems(Last, (advance_step, advance_waiters));
        app
    }

    fn spawn_at(world: &mut World, name: &str, translation: [f32; 3]) -> Entity {
        world
            .spawn((
                Name::new(name.to_string()),
                Transform {
                    translation: translation.into(),
                    rotation: Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
            ))
            .id()
    }

    #[test]
    fn the_theme_fading_atom_routes_to_the_resolver() {
        let mut world = World::new();
        let atom = serde_json::json!({ "predicate": "ui.theme_fading", "is": false });
        let broken = evaluate(&mut world, &atom);
        assert!(
            !broken.holds && broken.error.is_some(),
            "no UI: {}",
            broken.detail
        );
        world.init_resource::<regia_ui::ThemeResolver>();
        assert!(evaluate(&mut world, &atom).holds, "nothing fades before a swap");
        let running = serde_json::json!({ "predicate": "ui.theme_fading" });
        assert!(!evaluate(&mut world, &running).holds);
        let negated = serde_json::json!({ "not": { "predicate": "ui.theme_fading" } });
        assert!(evaluate(&mut world, &negated).holds);
    }

    #[test]
    fn json_paths_index_arrays_and_quick_draw_is_a_predicate_resource() {
        let v = serde_json::json!({ "texts": ["SCORE 1", "PAUSED"], "by_kind": { "text": 2 } });
        assert_eq!(json_path(&v, "texts.1"), Some(&serde_json::json!("PAUSED")));
        assert_eq!(json_path(&v, "texts.2"), None);
        assert_eq!(json_path(&v, "by_kind.text"), Some(&serde_json::json!(2)));
        let mut world = World::new();
        assert!(resource_snapshot(&mut world, "QuickDraw").is_err());
        world.insert_resource(regia_quick::QuickDrawStats {
            texts: vec!["GAME OVER".into()],
            ..Default::default()
        });
        let snap = resource_snapshot(&mut world, "QuickDraw").expect("snapshot");
        assert_eq!(json_path(&snap, "texts.0"), Some(&serde_json::json!("GAME OVER")));
    }

    #[test]
    fn huge_time_scale_and_step_counts_are_refused() {
        let mut app = app();
        app.update();
        assert!(set_time_scale(app.world_mut(), &json!({ "scale": 1e39 })).is_err());
        assert_eq!(app.world().resource::<Time>().time_scale, 1.0, "unchanged");

        let (tx, _rx) = crossbeam_channel::bounded(1);
        let err = step(
            app.world_mut(),
            &json!({ "frames": 1_000_000_000_000_000_000u64 }),
            json!(1),
            tx,
        );
        assert!(err.is_err());
        let control = app.world().resource::<AgentTimeControl>();
        assert!(control.stepping.is_none() && !control.paused, "nothing was armed");
    }

    #[test]
    fn events_contain_matches_a_record_older_than_the_newest_200() {
        let mut app = app();
        let mut log = crate::events::AgentEventLog::default();
        log.push(0, "toast", serde_json::json!({ "text": "NEEDLE" }));
        for i in 0..250 {
            log.push(0, "toast", serde_json::json!({ "text": format!("filler {i}") }));
        }
        app.insert_resource(log);
        let finding = evaluate(
            app.world_mut(),
            &serde_json::json!({ "events_contain": "NEEDLE" }),
        );
        assert!(finding.holds, "{finding:?}");
    }

    #[test]
    fn not_of_a_broken_condition_stays_broken() {
        let mut app = app();
        let finding = evaluate(
            app.world_mut(),
            &serde_json::json!({
                "not": { "entity": 1u64 << 40, "component": "Transform", "op": "eq", "value": 0 }
            }),
        );
        assert!(!finding.holds, "{finding:?}");
        assert!(finding.error.is_some(), "{finding:?}");
    }

    #[test]
    fn pause_step_and_time_scale() {
        let mut app = app();
        app.update();
        let paused = set_paused(app.world_mut(), true).expect("pauses");
        assert_eq!(paused["paused"], true);
        assert!(app.world().resource::<Time>().paused);

        let (tx, rx) = crossbeam_channel::bounded(1);
        step(app.world_mut(), &json!({ "frames": 3 }), json!(1), tx).expect("defers");
        for _ in 0..3 {
            app.update();
        }
        let response = rx.recv_timeout(Duration::from_secs(1)).expect("answered");
        assert!(
            response.error.is_none(),
            "{:?}",
            response.error.map(|e| e.message)
        );
        assert!(
            app.world().resource::<Time>().paused,
            "paused again after the step"
        );

        let scale = set_time_scale(app.world_mut(), &json!({ "scale": 0.5 })).expect("scales");
        assert_eq!(scale["before"], 1.0);
        assert_eq!(scale["after"], 0.5);
        let tenth = set_time_scale(app.world_mut(), &json!({ "scale": 0.1 })).expect("scales");
        assert_eq!(tenth["after"], f64::from(0.1_f32), "reports the stored f32");
        assert!(set_time_scale(app.world_mut(), &json!({ "scale": -1 })).is_err());
    }

    /// A `play.step` the next `PreUpdate` dispatches (as the server does).
    #[derive(Resource)]
    struct PendingStep(Option<(u64, Sender<Response>)>);

    fn dispatch_pending_step(world: &mut World) {
        let Some((frames, reply)) = world
            .get_resource_mut::<PendingStep>()
            .and_then(|mut p| p.0.take())
        else {
            return;
        };
        step(world, &json!({ "frames": frames }), json!(1), reply).expect("defers");
    }

    #[test]
    fn a_step_dispatched_on_a_paused_frame_runs_that_many_frames_of_game_time() {
        // The server dispatches in `PreUpdate`, after `First` took the paused frame's zero
        // delta: that frame used to count as the first stepped frame, so `frames: 1` ran no game
        // time at all and `frames: n` ran n - 1 frames.
        let mut app = app();
        app.add_systems(PreUpdate, dispatch_pending_step);
        app.update();
        set_paused(app.world_mut(), true).expect("pauses");
        app.update();
        for frames in [1u64, 3] {
            let before = app.world().resource::<Time>().elapsed();
            let (tx, rx) = crossbeam_channel::bounded(1);
            app.insert_resource(PendingStep(Some((frames, tx))));
            let mut updates = 0u64;
            while rx.is_empty() && updates < 10 {
                app.update();
                updates += 1;
            }
            let response = rx.try_recv().expect("answered");
            assert!(
                response.error.is_none(),
                "{:?}",
                response.error.map(|e| e.message)
            );
            let ran = app.world().resource::<Time>().elapsed() - before;
            assert_eq!(
                ran,
                Duration::from_millis(16) * frames as u32,
                "{frames} frame(s) of game time"
            );
            assert_eq!(updates, frames + 1, "the dispatch frame plus the stepped frames");
            assert!(app.world().resource::<Time>().paused, "paused again");
            app.update();
            assert_eq!(
                app.world().resource::<Time>().elapsed() - before,
                ran,
                "no game time after the step"
            );
        }
    }

    #[test]
    fn a_step_sent_while_running_counts_its_dispatch_frame() {
        // Unpaused, the dispatch frame took a real delta in `First`: it is the first stepped
        // frame, as before.
        let mut app = app();
        app.add_systems(PreUpdate, dispatch_pending_step);
        app.update();
        let before = app.world().resource::<Time>().elapsed();
        let (tx, rx) = crossbeam_channel::bounded(1);
        app.insert_resource(PendingStep(Some((2, tx))));
        let mut updates = 0u64;
        while rx.is_empty() && updates < 10 {
            app.update();
            updates += 1;
        }
        assert!(rx.try_recv().is_ok(), "answered");
        assert_eq!(updates, 2);
        assert_eq!(
            app.world().resource::<Time>().elapsed() - before,
            Duration::from_millis(32)
        );
    }

    #[test]
    fn a_second_step_while_stepping_is_rejected() {
        let mut app = app();
        app.update();
        let (tx, _rx) = crossbeam_channel::bounded(1);
        step(app.world_mut(), &json!({ "frames": 5 }), json!(1), tx).expect("defers");
        let (tx2, _rx2) = crossbeam_channel::bounded(1);
        let err = step(app.world_mut(), &json!({ "frames": 1 }), json!(2), tx2).expect_err("rejects");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0009"));
    }

    #[test]
    fn field_distance_resource_logs_and_event_conditions() {
        let mut app = app();
        let a = spawn_at(app.world_mut(), "Player", [0.0, 0.0, 0.0]);
        let b = spawn_at(app.world_mut(), "Door", [3.0, 0.0, 4.0]); // 5 m away
        let world = app.world_mut();

        // Field atom by id and by name; comparison ops both ways.
        let finding = evaluate(
            world,
            &json!({
                "entity": a.to_bits(), "component": "Transform",
                "path": "translation.x", "op": "eq", "value": 0.0
            }),
        );
        assert!(finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "entity": { "name": "Door" }, "component": "Transform",
                "path": "translation.x", "op": "gte", "value": 1.0
            }),
        );
        assert!(finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "entity": a.to_bits(), "component": "Transform",
                "path": "translation.x", "op": "gt", "value": 0.5
            }),
        );
        assert!(!finding.holds && finding.error.is_none(), "{finding:?}");

        // Distance atom.
        let finding = evaluate(
            world,
            &json!({
                "a": { "name": "Player" }, "b": { "name": "Door" }, "op": "within", "meters": 6.0
            }),
        );
        assert!(finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "a": a.to_bits(), "b": b.to_bits(), "op": "within", "meters": 4.0
            }),
        );
        assert!(!finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "a": a.to_bits(), "b": b.to_bits(), "op": "beyond", "meters": 4.0
            }),
        );
        assert!(finding.holds, "{finding:?}");

        // Resource atom against GameSettings.
        let finding = evaluate(
            world,
            &json!({
                "resource": "GameSettings", "path": "input.sensitivity", "op": "eq", "value": 1.0
            }),
        );
        assert!(finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "resource": "Time", "path": "frame", "op": "gte", "value": 0
            }),
        );
        assert!(finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "resource": "Nope", "op": "eq", "value": 0
            }),
        );
        assert!(
            finding.error.is_some(),
            "unknown resources are broken, not false: {finding:?}"
        );

        // Combinators.
        let finding = evaluate(
            world,
            &json!({
                "all": [
                    { "entity": a.to_bits(), "component": "Transform", "path": "translation.x", "op": "eq", "value": 0.0 },
                    { "not": { "entity": a.to_bits(), "component": "Transform", "path": "translation.x", "op": "eq", "value": 1.0 } },
                ]
            }),
        );
        assert!(finding.holds, "{finding:?}");
        let finding = evaluate(
            world,
            &json!({
                "any": [
                    { "entity": a.to_bits(), "component": "Transform", "path": "translation.x", "op": "gt", "value": 99.0 },
                    { "entity": { "name": "Door" }, "component": "Transform", "path": "translation.z", "op": "eq", "value": 4.0 },
                ]
            }),
        );
        assert!(finding.holds, "{finding:?}");

        // Bad conditions are reported, not panicked on.
        assert!(
            evaluate(
                world,
                &json!({ "entity": 1u64 << 40, "component": "Transform", "op": "eq", "value": 0 })
            )
            .error
            .is_some()
        );
        assert!(
            evaluate(
                world,
                &json!({ "entity": a.to_bits(), "component": "Nope", "op": "eq", "value": 0 })
            )
            .error
            .is_some()
        );
        assert!(evaluate(world, &json!(42)).error.is_some());
    }

    #[test]
    fn pending_waiters_are_capped() {
        let mut app = app();
        let condition = json!({
            "condition": { "resource": "Time", "path": "frame", "op": "gte", "value": u64::MAX },
            "timeout_ms": 120_000,
        });
        let mut replies = Vec::new();
        for i in 0..MAX_WAITERS {
            let (tx, rx) = crossbeam_channel::bounded(1);
            wait_until(app.world_mut(), &condition, json!(i), tx).expect("queued");
            replies.push(rx);
        }
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let error = wait_until(app.world_mut(), &condition, json!("one more"), tx).expect_err("capped");
        assert_eq!(error.code, crate::protocol::INVALID_REQUEST);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("too_many_pending")
        );
        assert_eq!(app.world().resource::<WaitQueue>().waiters.len(), MAX_WAITERS);
    }

    #[test]
    fn wait_until_answers_when_the_condition_becomes_true() {
        let mut app = app();
        app.update();
        let a = spawn_at(app.world_mut(), "Mover", [0.0, 0.0, 0.0]);
        let (tx, rx) = crossbeam_channel::bounded(1);
        wait_until(
            app.world_mut(),
            &json!({
                "condition": {
                    "entity": { "name": "Mover" }, "component": "Transform",
                    "path": "translation.y", "op": "gte", "value": 2.0
                },
                "timeout_ms": 5000,
            }),
            json!(1),
            tx,
        )
        .expect("defers");

        // Nothing yet.
        app.update();
        assert!(rx.try_recv().is_err());
        // Move the entity past the threshold (scoped borrow releases before the update).
        {
            let mut transform = app.world_mut().get_mut::<Transform>(a).expect("transform");
            transform.translation.y = 2.5;
        }
        app.update();
        let response = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("answered once true");
        let result = response.result.expect("satisfied");
        assert_eq!(result["satisfied"], true);
        assert!(result["waited_ms"].is_number(), "wait time reported: {result}");
    }

    #[test]
    fn wait_until_times_out_with_satisfied_false() {
        let mut app = app();
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        wait_until(
            app.world_mut(),
            &json!({
                "condition": { "entity": { "component": "NopeThere" }, "component": "Transform", "op": "eq", "value": 0 },
                "timeout_ms": 30,
            }),
            json!(1),
            tx,
        )
        .expect("defers");
        std::thread::sleep(Duration::from_millis(60));
        app.update();
        let response = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("answered on timeout");
        let result = response.result.expect("timeout is a result, not an error");
        assert_eq!(result["satisfied"], false);
        assert!(
            result["last_error"].is_string(),
            "the unresolvable entity is reported: {result}"
        );
    }

    #[test]
    fn expect_reports_each_assertion() {
        let mut app = app();
        app.update();
        let a = spawn_at(app.world_mut(), "Player", [1.0, 0.0, 0.0]);
        let bits = a.to_bits();
        let report = expect(
            app.world_mut(),
            &json!({ "assertions": [
                { "name": "player at x=1", "condition": { "entity": bits, "component": "Transform", "path": "translation.x", "op": "eq", "value": 1.0 } },
                { "name": "player moved", "condition": { "entity": bits, "component": "Transform", "path": "translation.x", "op": "gt", "value": 5.0 } },
            ]}),
        )
        .expect("evaluates");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["failed"], 1);
        assert_eq!(report["pass"], false);
        let results = report["results"].as_array().expect("results");
        assert_eq!(results[0]["pass"], true);
        assert_eq!(results[1]["pass"], false);
        assert!(
            results[1]["detail"]
                .as_str()
                .unwrap_or("")
                .contains("expected gt 5.0")
        );

        let broken = expect(
            app.world_mut(),
            &json!({ "assertions": [{ "condition": { "entity": 1u64 << 40, "component": "Transform", "op": "eq", "value": 0 } }] }),
        )
        .expect("broken conditions fail, not error");
        assert_eq!(broken["failed"], 1);
        assert!(broken["results"][0]["error"].is_string());
    }

    #[test]
    fn action_values_parse_all_three_shapes() {
        assert_eq!(action_value(&json!(1.0)).expect("number").x, 1.0);
        assert_eq!(action_value(&json!([1.0, -1.0])).expect("array").y, -1.0);
        assert_eq!(
            action_value(&json!({ "x": 1.0, "y": 2.0, "z": 3.0 }))
                .expect("object")
                .z,
            3.0
        );
        assert!(action_value(&json!("fast")).is_err());
    }

    #[test]
    fn inject_actions_schedule_into_input_injection() {
        let mut app = app();
        app.insert_resource(regia_input::InputInjection::default());
        app.update();
        let frame = app.world().resource::<Time>().frame_count();
        let result = inject_input(
            app.world_mut(),
            &json!({ "actions": [
                { "action": "Move", "value": [0.0, 1.0], "duration_frames": 3 },
                { "action": "Jump", "value": 1.0 },
            ]}),
        )
        .expect("schedules");
        assert_eq!(result["scheduled"], 2);
        let injection = app.world().resource::<regia_input::InputInjection>();
        assert_eq!(injection.value_for("Move", frame).map(|v| v.y), Some(1.0));
        assert_eq!(injection.value_for("Move", frame + 2).map(|v| v.y), Some(1.0));
        assert_eq!(injection.value_for("Move", frame + 3), None, "duration honored");
    }

    #[test]
    fn rejected_inject_batches_commit_nothing() {
        let mut app = app();
        app.insert_resource(regia_input::InputInjection::default());
        app.update();
        // A good action followed by a bad one: neither is scheduled.
        let bad_action = inject_input(
            app.world_mut(),
            &json!({ "actions": [
                { "action": "Move", "value": 1.0 },
                { "value": 1.0 },
            ]}),
        );
        assert!(bad_action.is_err());
        assert_eq!(
            app.world()
                .resource::<regia_input::InputInjection>()
                .scheduled_count(),
            0
        );
        // A good event followed by an unknown type is refused as invalid params, before any
        // window lookup or write.
        let bad_event = inject_input(
            app.world_mut(),
            &json!({
                "events": [{ "type": "key", "key": "KeyA" }, { "type": "warp" }],
                "actions": [{ "action": "Move", "value": 1.0 }],
            }),
        )
        .expect_err("rejects");
        assert_eq!(bad_event.code, crate::protocol::INVALID_PARAMS);
        assert_eq!(
            app.world()
                .resource::<regia_input::InputInjection>()
                .scheduled_count(),
            0
        );
    }

    #[test]
    fn not_of_a_broken_condition_stays_broken_after_an_update() {
        let mut app = app();
        app.update();
        let finding = evaluate(
            app.world_mut(),
            &json!({ "not": { "entity": 1u64 << 40, "component": "Transform", "op": "eq", "value": 0 } }),
        );
        assert!(!finding.holds, "{finding:?}");
        assert!(finding.error.is_some(), "{finding:?}");
    }

    #[test]
    fn comparisons_handle_numbers_and_strings() {
        assert!(compare(&json!(2.0), "gte", &json!(1.0)).expect("works"));
        assert!(compare(&json!(2), "eq", &json!(2.0)).expect("integer vs float"));
        assert!(
            compare(&json!(f64::from(0.1_f32)), "eq", &json!(0.1)).expect("f32 vs f64"),
            "an f32-stored value equals the f64 the caller typed"
        );
        assert!(!compare(&json!(0.1), "eq", &json!(0.2)).expect("distinct"));
        assert!(!compare(&json!("a"), "eq", &json!("b")).expect("strings"));
        assert!(compare(&json!("a"), "eq", &json!("a")).expect("strings"));
        assert!(compare(&json!(2.0), "ne", &json!(3.0)).expect("ne"));
        assert!(compare(&json!("a"), "gt", &json!(1)).is_err(), "gt needs numbers");
        assert!(compare(&json!(1), "warp", &json!(1)).is_err(), "unknown op");
    }

    /// ADR-7831: `play.get_diagnostics` counts frames that ran game time and the raw events
    /// `play.inject_input` wrote, so a caller in another process (the editor's Game tab) sees a
    /// step advance exactly one game frame and a key reach this process.
    #[test]
    fn diagnostics_count_game_frames_and_injected_keys() {
        let mut app = app();
        app.add_systems(PreUpdate, dispatch_pending_step);
        app.world_mut()
            .spawn((regia_platform::Window::default(), PrimaryWindow));
        app.update();
        app.update();
        let running = get_diagnostics(app.world_mut())["game_frames"]
            .as_u64()
            .expect("game_frames");
        assert!(running >= 1, "frames with game time counted: {running}");
        set_paused(app.world_mut(), true).expect("pauses");
        app.update();
        let paused = get_diagnostics(app.world_mut())["game_frames"]
            .as_u64()
            .expect("game_frames");
        app.update();
        app.update();
        assert_eq!(
            get_diagnostics(app.world_mut())["game_frames"],
            paused,
            "no game frames while paused"
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        app.insert_resource(PendingStep(Some((1, tx))));
        for _ in 0..4 {
            app.update();
        }
        assert!(rx.try_recv().is_ok(), "the step answered");
        assert_eq!(
            get_diagnostics(app.world_mut())["game_frames"],
            paused + 1,
            "one step is exactly one game frame"
        );
        let out = inject_input(
            app.world_mut(),
            &json!({ "events": [
                { "type": "key", "key": "KeyW", "state": "pressed" },
                { "type": "key", "key": "KeyW", "state": "released" },
                { "type": "cursor", "x": 4.0, "y": 2.0 },
            ]}),
        )
        .expect("injects");
        assert_eq!(out["injected"], 3);
        let d = get_diagnostics(app.world_mut());
        assert_eq!(
            (d["injected_events"].as_u64(), d["injected_keys"].as_u64()),
            (Some(3), Some(2))
        );
    }

    /// ADR-8452: a scancode is a physical key (`scancode:N`, empty logical key). Text on that
    /// event is `play_gate_text` and the batch writes nothing. A named key that omits text
    /// still copies the name into the logical key.
    #[test]
    fn a_scancode_is_a_physical_key_and_text_is_refused() {
        let mut app = app();
        app.add_message::<KeyboardInput>();
        app.world_mut()
            .spawn((regia_platform::Window::default(), PrimaryWindow));
        app.update();
        let refused = inject_input(
            app.world_mut(),
            &json!({ "events": [{
                "type": "key",
                "scancode": 28,
                "text": "confirm",
                "state": "pressed",
            }]}),
        )
        .expect_err("text with a scancode");
        assert_eq!(refused.code, crate::protocol::INVALID_PARAMS);
        assert_eq!(
            refused.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("play_gate_text")
        );
        let written = app
            .world()
            .get_resource::<regia_ecs::prelude::Messages<KeyboardInput>>()
            .map(|messages| messages.get_cursor().read(messages).count())
            .unwrap_or(0);
        assert_eq!(written, 0, "a refused batch writes no key");

        inject_input(
            app.world_mut(),
            &json!({ "events": [
                { "type": "key", "scancode": 28, "state": "pressed" },
                { "type": "key", "key": "KeyW", "state": "pressed" },
            ]}),
        )
        .expect("scancode and a named key");
        let messages = app
            .world()
            .resource::<regia_ecs::prelude::Messages<KeyboardInput>>();
        let keys: Vec<KeyboardInput> = messages.get_cursor().read(messages).cloned().collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].physical_key, "scancode:28");
        assert_eq!(keys[0].logical_key, "");
        assert_eq!(keys[1].physical_key, "KeyW");
        assert_eq!(keys[1].logical_key, "KeyW");
    }
}
