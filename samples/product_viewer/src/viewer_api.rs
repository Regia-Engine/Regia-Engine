// SPDX-License-Identifier: MIT-0
//! The `viewer_app.*` calls (`book/agents/product-viewer.md`): plain functions over the host's state, so
//! they are tested without the agent server. `crate::agent` mounts them on the protocol in a dev build.

use std::time::Duration;

use regia_platform::{SafeArea, TouchPhase};
use regia_web::seams::{PointerClass, ViewRequest};
use serde_json::{Value, json};

use crate::app::{PanelUi, StepRequest, Touch, ViewerCore, ViewerState, end_scripted, scripted_step};

/// A refused call. `code` is lowercase ascii, digits and `_`, documented on the book page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// The error code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl ApiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<regia_web::WebError> for ApiError {
    fn from(err: regia_web::WebError) -> Self {
        Self::new(err.code, err.message)
    }
}

fn text<'a>(params: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new("bad_argument", format!("{name} must be a string")))
}

fn number(value: &Value) -> Option<f32> {
    value.as_f64().map(|n| n as f32)
}

/// `viewer_app.options`: the open product, its option groups, the status line.
pub fn options(state: &ViewerState, core: &mut ViewerCore) -> Value {
    json!({
        "product": state.title,
        "open": state.open_slug,
        "groups": core.player.options().unwrap_or(Value::Null),
        "status": state.status,
        "status_is_error": state.status_is_error,
        "showing_products": state.showing_products,
    })
}

/// `viewer_app.set_pick {group, value}`: applies a sold pick at once, as a tap does; a refusal changes
/// nothing. The next frame shows it.
pub fn set_pick(state: &mut ViewerState, core: &mut ViewerCore, params: &Value) -> Result<Value, ApiError> {
    let group = text(params, "group")?;
    let value = params
        .get("value")
        .ok_or_else(|| ApiError::new("bad_argument", "value is required"))?;
    let (result, _) = core.player.set_pick(group, value, false)?;
    state.note_external_change(core);
    Ok(json!({ "ok": true, "build": result.build }))
}

/// `viewer_app.build`: the current `jewelry.build/1`, with no client data.
pub fn build(core: &mut ViewerCore) -> Result<Value, ApiError> {
    Ok(core.player.build()?)
}

/// `viewer_app.view`: the view shown now, and whether it still moves.
pub fn view(core: &mut ViewerCore) -> Result<Value, ApiError> {
    let shown = core.player.view()?;
    Ok(json!({ "view": shown, "moving": core.player.animating() }))
}

/// `viewer_app.set_view {tilt, spin, pan, zoom, animate}`: sets the target; what is out of range is clamped,
/// never refused, and the answer says so.
pub fn set_view(core: &mut ViewerCore, params: &Value) -> Result<Value, ApiError> {
    let request: ViewRequest = serde_json::from_value(params.clone())
        .map_err(|err| ApiError::new("bad_argument", err.to_string()))?;
    let (report, _) = core.player.set_view(&request)?;
    Ok(report)
}

/// `viewer_app.reset_view`: animates back to the default view.
pub fn reset_view(core: &mut ViewerCore) -> Result<Value, ApiError> {
    core.player.reset_view()?;
    Ok(json!({ "ok": true }))
}

fn phase(name: &str) -> Option<TouchPhase> {
    match name {
        "down" => Some(TouchPhase::Started),
        "move" => Some(TouchPhase::Moved),
        "up" => Some(TouchPhase::Ended),
        "cancel" => Some(TouchPhase::Cancelled),
        _ => None,
    }
}

/// `viewer_app.touch {events: [{id, phase, x, y, at_ms}]}`: feeds finger events, in physical pixels, through
/// the same routing as the OS's (panel or view).
pub fn touch(
    state: &mut ViewerState,
    panel_px: egui::Rect,
    scale: f32,
    params: &Value,
) -> Result<Value, ApiError> {
    let events = params
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::new("bad_argument", "events must be a list"))?;
    let mut parsed = Vec::with_capacity(events.len());
    for event in events {
        let phase = event
            .get("phase")
            .and_then(Value::as_str)
            .and_then(phase)
            .ok_or_else(|| ApiError::new("bad_argument", "phase must be down, move, up or cancel"))?;
        let coordinate = |name: &str| {
            event
                .get(name)
                .and_then(number)
                .filter(|n| n.is_finite())
                .ok_or_else(|| ApiError::new("bad_argument", format!("{name} must be a finite number")))
        };
        parsed.push(Touch {
            id: event.get("id").and_then(Value::as_u64).unwrap_or(0),
            phase,
            class: PointerClass::Touch,
            x: coordinate("x")?,
            y: coordinate("y")?,
            at: Duration::from_secs_f64(
                event.get("at_ms").and_then(Value::as_f64).unwrap_or(0.0).max(0.0) / 1000.0,
            ),
        });
    }
    for event in parsed {
        state.touch(event, panel_px, scale);
    }
    Ok(json!({ "ok": true }))
}

/// `viewer_app.set_deterministic {on}`: scripted capture. While on, the panel and the picture move only in
/// `viewer_app.step`, on the script's clock, so the same calls make the same frames. Turning it on starts that clock
/// at 0.
pub fn set_deterministic(
    panel_ui: &mut PanelUi,
    core: &mut ViewerCore,
    params: &Value,
) -> Result<Value, ApiError> {
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::new("bad_argument", "on must be true or false"))?;
    if on {
        let _ = core.player.set_deterministic(true);
    } else {
        end_scripted(panel_ui, core);
    }
    Ok(json!({ "ok": true, "deterministic": on }))
}

/// `viewer_app.step {dt_ms, force, capture}`: one scripted frame on the script's clock. `force`: draw even when nothing
/// changed. `capture`: `"hash"` answers the frame's hash, `"png"` also its PNG in base64 (`viewer_app.step` needs the
/// agent build, which has the PNG encoder).
pub fn step(
    state: &mut ViewerState,
    panel_ui: &mut PanelUi,
    core: &mut ViewerCore,
    size: (u32, u32),
    scale: f32,
    safe: &SafeArea,
    params: &Value,
) -> Result<Value, ApiError> {
    let dt_ms = params
        .get("dt_ms")
        .and_then(Value::as_f64)
        .ok_or_else(|| ApiError::new("bad_argument", "dt_ms must be a number"))?;
    let force = match params.get("force") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(force)) => *force,
        Some(_) => return Err(ApiError::new("bad_argument", "force must be true or false")),
    };
    let wanted = match params.get("capture") {
        None | Some(Value::Null) => None,
        Some(Value::String(kind)) if kind == "hash" || kind == "png" => Some(kind.as_str()),
        Some(_) => return Err(ApiError::new("bad_argument", "capture must be hash or png")),
    };
    let request = StepRequest {
        dt_ms,
        force,
        capture: wanted.is_some(),
    };
    let done = scripted_step(state, panel_ui, core, size, scale, safe, request)?;
    let (frame, clock_ms) = core.player.script_clock();
    let mut answer = json!({ "drew": done.drew, "frame": frame, "clock_ms": clock_ms });
    if let Some(capture) = done.capture {
        answer["width"] = json!(capture.width);
        answer["height"] = json!(capture.height);
        answer["hash"] = json!(regia_web::capture::frame_hash(&capture.pixels));
        if wanted == Some("png") {
            answer["png_base64"] = json!(png_base64(capture)?);
        }
    }
    Ok(answer)
}

#[cfg(feature = "agent")]
fn png_base64(capture: crate::surface::Capture) -> Result<String, ApiError> {
    let still = regia_viewer::Still {
        width: capture.width,
        height: capture.height,
        rgba: capture.pixels,
    };
    let bytes = still
        .encode_png()
        .map_err(|err| ApiError::new("render_failed", err.to_string()))?;
    Ok(regia_agent::base64_encode(&bytes))
}

#[cfg(not(feature = "agent"))]
fn png_base64(_capture: crate::surface::Capture) -> Result<String, ApiError> {
    Err(ApiError::new(
        "bad_argument",
        "capture png needs the agent build; use capture hash",
    ))
}

/// `viewer_app.stats`: the player's counters, the host's wake-ups and timings, the surface's history.
pub fn stats(state: &ViewerState, core: &ViewerCore) -> Value {
    let mut report = state.stats.report();
    let host = core.host.as_ref().map(|host| host.borrow());
    let backend = host
        .as_ref()
        .map_or_else(String::new, |h| format!("{:?}", h.adapter().get_info().backend));
    if let Some(fields) = report.as_object_mut() {
        fields.insert("player".to_owned(), core.player.stats(&backend));
        if let Some(host) = host {
            fields.insert("adapter".to_owned(), json!(host.adapter().get_info().name));
            fields.insert("surface_format".to_owned(), json!(format!("{:?}", host.format())));
            fields.insert("has_surface".to_owned(), json!(host.has_surface()));
            fields.insert("surfaces_created".to_owned(), json!(host.surfaces_created()));
            fields.insert("surfaces_dropped".to_owned(), json!(host.surfaces_dropped()));
            fields.insert("presents".to_owned(), json!(host.presents()));
        }
        if let Some(gpu_start) = &core.gpu_start {
            fields.insert("gpu_start".to_owned(), gpu_start.clone());
        }
    }
    report
}

/// `viewer_app.stats_clear`: forgets the timing samples, so a measurement starts clean.
pub fn stats_clear(state: &mut ViewerState) -> Value {
    state.stats.clear_timings();
    json!({ "ok": true })
}

/// `viewer_app.packages`: the packages on the device, and where to copy more.
#[must_use]
pub fn packages(state: &ViewerState) -> Value {
    let rows: Vec<Value> = state
        .store
        .list()
        .into_iter()
        .map(|entry| json!({ "slug": entry.slug, "package_id": entry.package_id, "problem": entry.problem }))
        .collect();
    let roots: Vec<String> = state
        .store
        .roots()
        .iter()
        .map(|r| r.display().to_string())
        .collect();
    json!({ "packages": rows, "roots": roots, "busy": state.task_running() })
}

/// `viewer_app.open_package {slug}`: reads and checks a package in the background.
pub fn open_package(state: &mut ViewerState, params: &Value) -> Result<Value, ApiError> {
    let slug = text(params, "slug")?;
    state
        .start_open(slug)
        .map_err(|err| ApiError::new(err.code, err.message))?;
    Ok(json!({ "ok": true, "started": true }))
}

/// `viewer_app.fetch_package {url}`: downloads and keeps a package in the background.
pub fn fetch_package(state: &mut ViewerState, params: &Value) -> Result<Value, ApiError> {
    let url = text(params, "url")?;
    state
        .start_fetch(url)
        .map_err(|err| ApiError::new(err.code, err.message))?;
    Ok(json!({ "ok": true, "started": true }))
}

#[cfg(test)]
mod tests {
    use regia_test_utils::unique_temp_dir;

    use super::*;
    use crate::app::ViewerConfig;

    fn rig() -> (std::path::PathBuf, ViewerState, ViewerCore) {
        let dir = unique_temp_dir("product-viewer-api");
        let config = ViewerConfig {
            package_roots: vec![dir.join("packages")],
            ..ViewerConfig::default()
        };
        (dir, ViewerState::new(&config), ViewerCore::new())
    }

    #[test]
    fn calls_on_nothing_open_say_so_by_code() {
        let (dir, mut state, mut core) = rig();
        let params = json!({"group": "Metal", "value": "x"});
        assert_eq!(
            set_pick(&mut state, &mut core, &params).err().map(|e| e.code),
            Some("no_product")
        );
        assert_eq!(view(&mut core).err().map(|e| e.code), Some("no_product"));
        assert_eq!(build(&mut core).err().map(|e| e.code), Some("no_product"));
        assert_eq!(reset_view(&mut core).err().map(|e| e.code), Some("no_product"));
        assert_eq!(
            set_view(&mut core, &json!({"tilt": [0.0, 0.0]}))
                .err()
                .map(|e| e.code),
            Some("no_product")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_params_are_refused_by_code() {
        let (dir, mut state, mut core) = rig();
        for bad in [
            json!({}),
            json!({"group": 1, "value": "x"}),
            json!({"group": "Metal"}),
        ] {
            assert_eq!(
                set_pick(&mut state, &mut core, &bad).err().map(|e| e.code),
                Some("bad_argument"),
                "{bad}"
            );
        }
        assert_eq!(
            set_view(&mut core, &json!({"tilt": [1.0], "nonsense": true}))
                .err()
                .map(|e| e.code),
            Some("bad_argument")
        );
        let mut panel_ui = PanelUi::default();
        for bad in [json!({}), json!({"on": 1})] {
            assert_eq!(
                set_deterministic(&mut panel_ui, &mut core, &bad)
                    .err()
                    .map(|e| e.code),
                Some("bad_argument"),
                "{bad}"
            );
        }
        let safe = SafeArea::default();
        for bad in [
            json!({}),
            json!({"dt_ms": "1"}),
            json!({"dt_ms": 1, "force": 1}),
            json!({"dt_ms": 1, "capture": "jpeg"}),
            json!({"dt_ms": 1}),
        ] {
            assert_eq!(
                step(&mut state, &mut panel_ui, &mut core, (8, 8), 1.0, &safe, &bad)
                    .err()
                    .map(|e| e.code),
                Some("bad_argument"),
                "{bad}: out of deterministic mode a good call is refused too"
            );
        }
        let panel = egui::Rect::NOTHING;
        assert_eq!(
            touch(&mut state, panel, 1.0, &json!({})).err().map(|e| e.code),
            Some("bad_argument")
        );
        assert_eq!(
            touch(
                &mut state,
                panel,
                1.0,
                &json!({"events": [{"phase": "hover", "x": 0, "y": 0}]})
            )
            .err()
            .map(|e| e.code),
            Some("bad_argument")
        );
        assert_eq!(
            open_package(&mut state, &json!({})).err().map(|e| e.code),
            Some("bad_argument")
        );
        assert_eq!(
            fetch_package(&mut state, &json!({"url": "http://host/a/"}))
                .err()
                .map(|e| e.code),
            Some("package_invalid_url")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stats_and_packages_answer_before_any_gpu() {
        let (dir, state, core) = rig();
        assert_eq!(stats(&state, &core)["player"]["draws"], 0);
        assert!(stats(&state, &core).get("adapter").is_none());
        assert_eq!(packages(&state)["packages"].as_array().map(Vec::len), Some(0));
        let _ = std::fs::remove_dir_all(dir);
    }
}
