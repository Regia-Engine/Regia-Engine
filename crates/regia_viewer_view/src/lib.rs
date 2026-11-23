// SPDX-License-Identifier: MPL-2.0
//! Product viewer drag view (track JW, item JW-L5, ADR-2540).
//!
//! A small, platform-free camera controller shared by the web player and the Android viewer:
//! drag to turn, pinch or scroll to zoom, two fingers to pan, double tap to reset. The view is
//! damped (frame-rate independent smoothing plus a decaying fling) and can never leave the
//! recipe's `view_range`: the tilt cone around the front axis, the pan bounds, the zoom limit from
//! `max_px_per_mm`, and the `spin` mode (off by default).
//!
//! The crate has no clock, no threads, no file system and no GPU: the host feeds pointer events
//! with its own timestamps, calls [`DragView::update`] once per frame with the frame time, draws
//! only when it returns true (a lazy loop idles at zero draws), and reads [`DragView::pose`].
//!
//! Page: `book/agents/viewer-view.md`. Agent method: `viewer.view_simulate`.

pub mod controller;
pub mod gesture;
pub mod range;

pub use controller::{ViewController, ViewInput, ViewPose, ViewState};
pub use gesture::{GestureRecognizer, PointerKind};
pub use range::{CameraDefaults, SpinMode, ViewRange, ViewRangeError};

use serde_json::{Value, json};

/// The recognizer and the controller together: what a player holds per canvas.
#[derive(Clone, Debug)]
pub struct DragView {
    /// The camera state and its limits.
    pub controller: ViewController,
    /// The pointer recognizer.
    pub gestures: GestureRecognizer,
}

impl DragView {
    /// A drag view for one product.
    pub fn new(range: ViewRange, camera: CameraDefaults) -> Self {
        DragView {
            controller: ViewController::new(range, camera),
            gestures: GestureRecognizer::new(),
        }
    }

    /// A drag view from a whole recipe document (its `view_range` and `camera_default`).
    pub fn from_recipe(recipe: &Value) -> Result<Self, ViewRangeError> {
        let (range, camera) = range::from_recipe(recipe)?;
        Ok(Self::new(range, camera))
    }

    fn apply(&mut self, inputs: Vec<ViewInput>) -> bool {
        let any = !inputs.is_empty();
        for i in inputs {
            self.controller.input(i);
        }
        any
    }

    /// A pointer went down. Returns whether the view's target changed.
    pub fn pointer_down(&mut self, id: u64, kind: PointerKind, x: f32, y: f32, t_ms: f64) -> bool {
        let i = self.gestures.pointer_down(id, kind, x, y, t_ms);
        self.apply(i)
    }

    /// A pointer moved.
    pub fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64) -> bool {
        let i = self.gestures.pointer_move(id, x, y, t_ms);
        self.apply(i)
    }

    /// A pointer lifted.
    pub fn pointer_up(&mut self, id: u64, t_ms: f64) -> bool {
        let i = self.gestures.pointer_up(id, t_ms);
        self.apply(i)
    }

    /// A pointer was cancelled.
    pub fn pointer_cancel(&mut self, id: u64) -> bool {
        let i = self.gestures.pointer_cancel(id);
        self.apply(i)
    }

    /// A wheel scroll in lines.
    pub fn wheel(&mut self, lines: f32) -> bool {
        let i = self.gestures.wheel(lines);
        self.apply(i)
    }

    /// Advance one frame of `dt_s` seconds. Returns whether to draw this frame.
    pub fn update(&mut self, dt_s: f32) -> bool {
        self.controller.update(dt_s);
        self.controller.take_dirty()
    }

    /// Whether the view still animates (keep requesting frames while true).
    pub fn is_animating(&self) -> bool {
        self.controller.is_animating()
    }

    /// The pose to draw.
    pub fn pose(&self) -> ViewPose {
        self.controller.pose()
    }
}

/// The largest number of events `simulate` accepts.
pub const MAX_SIM_EVENTS: usize = 65_536;

/// The most frames one `simulate` script may run, over all its `frame`, `frames` and `settle`
/// events (about 4.6 hours at 60 Hz). Without it, 65536 `frames` events of 100000 each would hold
/// the agent server for minutes.
pub const MAX_SIM_FRAMES: u64 = 1_000_000;

fn sim_err(code: &'static str, message: impl Into<String>) -> ViewRangeError {
    ViewRangeError {
        code,
        message: message.into(),
    }
}

fn state_json(s: &ViewState) -> Value {
    json!({"tilt_deg": s.tilt_deg, "spin_deg": s.spin_deg, "pan_mm": s.pan_mm, "zoom": s.zoom})
}

/// Run a scripted input sequence through a fresh drag view and report where it ends and the widest
/// view it ever showed. This is the agent method `viewer.view_simulate` (book/agents/viewer-view.md).
///
/// `script`: `{ recipe? | view_range?, camera_default?, viewport?: [w, h], events: [...] }` where
/// each event is one of `{down: {id, x, y, t, pointer?}}` (`pointer` = `touch`, `mouse`,
/// `mouse_secondary`), `{move: {id, x, y, t}}`, `{up: {id, t}}`, `{cancel: {id}}`,
/// `{wheel: lines}`, `{frame: dt_ms}`, `{frames: [count, dt_ms]}`, `{set_view: {tilt_deg?, spin_deg?,
/// pan_mm?, zoom?, immediate?}}`, `{reset: true}`, `{settle: true}` (frames at 60 Hz until idle,
/// at most 10 s). A script runs at most [`MAX_SIM_EVENTS`] events and [`MAX_SIM_FRAMES`] frames.
pub fn simulate(script: &Value) -> Result<Value, ViewRangeError> {
    let (range, camera) = if let Some(recipe) = script.get("recipe") {
        range::from_recipe(recipe)?
    } else {
        range::from_recipe(script)?
    };
    let mut view = DragView::new(range, camera);
    if let Some(vp) = script.get("viewport") {
        let a = vp
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| sim_err("view_bad_event", "viewport is not [width, height]"))?;
        let side = |v: &Value, name: &str| -> Result<f32, ViewRangeError> {
            let Some(x) = v.as_f64() else {
                return Err(sim_err(
                    "view_bad_event",
                    format!("viewport {name} is not a number"),
                ));
            };
            let x = x as f32;
            if x.is_finite() && x > 0.0 {
                Ok(x)
            } else {
                Err(sim_err(
                    "view_bad_event",
                    format!("viewport {name} must be positive"),
                ))
            }
        };
        view.controller
            .set_viewport(side(&a[0], "width")?, side(&a[1], "height")?);
    }
    let events = match script.get("events") {
        None => Vec::new(),
        Some(e) => e
            .as_array()
            .cloned()
            .ok_or_else(|| sim_err("view_bad_event", "events is not an array"))?,
    };
    if events.len() > MAX_SIM_EVENTS {
        return Err(sim_err(
            "view_too_many_events",
            format!("more than {MAX_SIM_EVENTS} events"),
        ));
    }
    let front = view.controller.range().front_axis;
    let mut widest: f32 = 0.0;
    let mut frames: u64 = 0;
    let mut draws: u64 = 0;
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let step = |view: &mut DragView, dt_s: f32, widest: &mut f32, frames: &mut u64, draws: &mut u64| {
        *frames += 1;
        if view.update(dt_s) {
            *draws += 1;
        }
        let a = view
            .pose()
            .view_dir()
            .dot(front)
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees();
        *widest = widest.max(a);
    };
    // Refuses a script whose frames would pass `MAX_SIM_FRAMES` before any of them runs.
    let budget = |frames: u64, more: u64| {
        if frames.saturating_add(more) > MAX_SIM_FRAMES {
            Err(sim_err(
                "view_too_many_events",
                format!("more than {MAX_SIM_FRAMES} frames in one script"),
            ))
        } else {
            Ok(())
        }
    };
    for (n, e) in events.iter().enumerate() {
        let bad = |what: &str| sim_err("view_bad_event", format!("event {n}: {what}"));
        let obj = e
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or_else(|| bad("not a one-key object"))?;
        let Some((kind, body)) = obj.iter().next() else {
            return Err(bad("empty"));
        };
        let id = || f(body, "id").map(|x| x as u64).ok_or_else(|| bad("missing id"));
        let xy = || match (f(body, "x"), f(body, "y")) {
            (Some(x), Some(y)) => Ok((x as f32, y as f32)),
            _ => Err(bad("missing x or y")),
        };
        let t = || f(body, "t").unwrap_or(0.0);
        match kind.as_str() {
            "down" => {
                let pointer = match body.get("pointer").and_then(Value::as_str).unwrap_or("touch") {
                    "touch" => PointerKind::Touch,
                    "mouse" => PointerKind::MousePrimary,
                    "mouse_secondary" => PointerKind::MouseSecondary,
                    _ => return Err(bad("pointer is not touch, mouse or mouse_secondary")),
                };
                let (x, y) = xy()?;
                view.pointer_down(id()?, pointer, x, y, t());
            }
            "move" => {
                let (x, y) = xy()?;
                view.pointer_move(id()?, x, y, t());
            }
            "up" => {
                view.pointer_up(id()?, t());
            }
            "cancel" => {
                view.pointer_cancel(id()?);
            }
            "wheel" => {
                view.wheel(body.as_f64().ok_or_else(|| bad("wheel is not a number"))? as f32);
            }
            "frame" => {
                let ms = body.as_f64().ok_or_else(|| bad("frame is not a number of ms"))?;
                budget(frames, 1)?;
                step(
                    &mut view,
                    (ms / 1000.0) as f32,
                    &mut widest,
                    &mut frames,
                    &mut draws,
                );
            }
            "frames" => {
                let a = body
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or_else(|| bad("frames is not [count, dt_ms]"))?;
                let count = a[0].as_u64().ok_or_else(|| bad("frames count"))?.min(100_000);
                let ms = a[1].as_f64().ok_or_else(|| bad("frames dt_ms"))?;
                budget(frames, count)?;
                for _ in 0..count {
                    step(
                        &mut view,
                        (ms / 1000.0) as f32,
                        &mut widest,
                        &mut frames,
                        &mut draws,
                    );
                }
            }
            "set_view" => {
                let mut s = view.controller.target();
                let finite = |v: &Value, what: &str| -> Result<f32, ViewRangeError> {
                    let Some(x) = v.as_f64() else {
                        return Err(bad(what));
                    };
                    let x = x as f32;
                    if x.is_finite() { Ok(x) } else { Err(bad(what)) }
                };
                if let Some(tilt) = body.get("tilt_deg") {
                    let a = tilt
                        .as_array()
                        .filter(|a| a.len() == 2)
                        .ok_or_else(|| bad("tilt_deg is not [x, y]"))?;
                    s.tilt_deg = [
                        finite(&a[0], "tilt_deg is not [x, y]")?,
                        finite(&a[1], "tilt_deg is not [x, y]")?,
                    ];
                }
                if let Some(spin) = body.get("spin_deg") {
                    s.spin_deg = finite(spin, "spin_deg is not a finite number")?;
                }
                if let Some(pan) = body.get("pan_mm") {
                    let a = pan
                        .as_array()
                        .filter(|a| a.len() == 2)
                        .ok_or_else(|| bad("pan_mm is not [x, y]"))?;
                    s.pan_mm = [
                        finite(&a[0], "pan_mm is not [x, y]")?,
                        finite(&a[1], "pan_mm is not [x, y]")?,
                    ];
                }
                if let Some(zoom) = body.get("zoom") {
                    s.zoom = finite(zoom, "zoom is not a finite number")?;
                }
                let immediate = match body.get("immediate") {
                    None => false,
                    Some(Value::Bool(flag)) => *flag,
                    Some(_) => return Err(bad("immediate is not a bool")),
                };
                view.controller.set_view(s, immediate);
            }
            "reset" => view.controller.reset(),
            "settle" => {
                let mut n = 0;
                while view.is_animating() && n < 600 {
                    budget(frames, 1)?;
                    step(&mut view, 1.0 / 60.0, &mut widest, &mut frames, &mut draws);
                    n += 1;
                }
            }
            other => return Err(bad(&format!("unknown event {other}"))),
        }
        // Immediate `set_view` shows a pose without a frame event. Sample after every
        // event so that pose counts toward the widest view the script showed.
        let shown = view
            .pose()
            .view_dir()
            .dot(front)
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees();
        if shown.is_finite() {
            widest = widest.max(shown);
        }
    }
    let pose = view.pose();
    let r = view.controller.range();
    Ok(json!({
        "target": state_json(&view.controller.target()),
        "shown": state_json(&view.controller.shown()),
        "pose": {
            "eye_mm": pose.eye_mm.to_array(),
            "look_at_mm": pose.look_at_mm.to_array(),
            "up": pose.up.to_array(),
            "fov_y_deg": pose.fov_y_rad.to_degrees(),
            "fov_short_deg": pose.fov_short_deg,
        },
        "view_angle_deg": pose.view_dir().dot(front).clamp(-1.0, 1.0).acos().to_degrees(),
        "widest_angle_deg": widest,
        "max_view_angle_deg": r.max_view_angle_deg(),
        "zoom_max": view.controller.zoom_max(),
        "spin": r.spin.as_str(),
        "frames": frames,
        "draws": draws,
        "animating": view.is_animating(),
        "build_camera": view.controller.build_camera(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulate_drag_and_settle() {
        let out = simulate(&json!({
            "viewport": [800, 600],
            "events": [
                {"down": {"id": 1, "x": 100, "y": 100, "t": 0}},
                {"move": {"id": 1, "x": 700, "y": 100, "t": 16}},
                {"up": {"id": 1, "t": 500}},
                {"settle": true},
                {"frames": [10, 16.7]}
            ]
        }))
        .expect("ok");
        assert!(
            (out["view_angle_deg"].as_f64().expect("ok") - 30.0).abs() < 1e-2,
            "{out}"
        );
        assert_eq!(out["animating"], false);
        // Idle frames draw nothing.
        let frames = out["frames"].as_u64().expect("ok");
        let draws = out["draws"].as_u64().expect("ok");
        assert!(draws + 10 <= frames, "{out}");
    }

    #[test]
    fn simulate_refuses_bad_events() {
        let e = simulate(&json!({"events": [{"jump": 1}]})).expect_err("refused");
        assert_eq!(e.code, "view_bad_event");
        let e = simulate(&json!({"view_range": {"tilt_max_deg": 120}})).expect_err("refused");
        assert_eq!(e.code, "view_range_bad_tilt");
    }

    /// corr-agent (ADR-5597): the steps of all `frames` events together are capped, not only
    /// each event's count. Eleven events of 100 000 frames ran (1.1 M steps) before the cap.
    #[test]
    fn simulate_caps_the_frames_of_the_whole_script() {
        let events: Vec<Value> = (0..11).map(|_| json!({"frames": [100_000, 16.7]})).collect();
        let e = simulate(&json!({ "events": events })).expect_err("over the script cap");
        assert_eq!(e.code, "view_too_many_events");
        // Exactly the cap still runs (ten events), and a single frame past it is refused.
        let mut events: Vec<Value> = (0..10).map(|_| json!({"frames": [100_000, 16.7]})).collect();
        let out = simulate(&json!({ "events": events.clone() })).expect("at the cap");
        assert_eq!(out["frames"].as_u64(), Some(MAX_SIM_FRAMES));
        events.push(json!({"frame": 16.7}));
        let e = simulate(&json!({ "events": events })).expect_err("one past the cap");
        assert_eq!(e.code, "view_too_many_events");
    }

    /// 65536 `frames` events of 100000 frames each passed the event cap and ran 6.5e9 frames,
    /// holding the agent server for minutes. The script's frames now have a budget.
    #[test]
    fn a_script_past_the_frame_budget_is_refused() {
        let per_event = 100_000u64;
        let events: Vec<Value> = (0..MAX_SIM_FRAMES / per_event + 1)
            .map(|_| json!({"frames": [per_event, 16.7]}))
            .collect();
        let e = simulate(&json!({"events": events})).expect_err("refused");
        assert_eq!(e.code, "view_too_many_events", "{e}");
        assert!(e.message.contains("frames"), "{e}");
        // The budget counts single frames too: one more after exactly the budget is refused.
        let mut events: Vec<Value> = (0..MAX_SIM_FRAMES / per_event)
            .map(|_| json!({"frames": [per_event, 16.7]}))
            .collect();
        events.push(json!({"frame": 16.7}));
        let e = simulate(&json!({"events": events})).expect_err("one frame past the budget");
        assert_eq!(e.code, "view_too_many_events", "{e}");
    }

    #[test]
    fn widest_counts_a_view_that_was_shown_without_a_frame() {
        let out = simulate(&json!({
            "events": [
                {"set_view": {"tilt_deg": [30.0, 0.0], "immediate": true}},
                {"set_view": {"tilt_deg": [0.0, 0.0], "immediate": true}}
            ]
        }))
        .expect("ok");
        let widest = out["widest_angle_deg"].as_f64().expect("widest");
        assert!(
            widest > 29.0,
            "the 30 degree view was shown between the two sets, widest {widest}: {out}"
        );
    }

    #[test]
    fn a_malformed_set_view_or_viewport_is_refused() {
        let e = simulate(&json!({"events": [{"set_view": {"tilt_deg": [10.0]}}]})).expect_err("tilt");
        assert_eq!(e.code, "view_bad_event", "{e}");
        let e = simulate(&json!({"events": [{"set_view": {"tilt_deg": ["x", 1.0]}}]})).expect_err("type");
        assert_eq!(e.code, "view_bad_event", "{e}");
        let e = simulate(&json!({"viewport": [0.0, 600.0]})).expect_err("viewport");
        assert_eq!(e.code, "view_bad_event", "{e}");
    }
}
