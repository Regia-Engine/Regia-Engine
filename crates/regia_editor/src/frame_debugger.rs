// SPDX-License-Identifier: MPL-2.0
//! Frozen-frame inspection through public renderer APIs (ADR-7824, ADR-7860).
//!
//! The inspector keeps one capture in memory. Pass order, versioned inputs/outputs,
//! declared pipeline keys and aggregate counters come from the captured renderer.
//! Images are read-only copies: the extension checkpoints, plus each built-in pass's
//! shared-target outputs (`color`, `depth`, `output`) copied at its pass boundary for the
//! captured frame only. Pass-internal resources and copies over the capture's byte budget are
//! listed as not copied. No saved document or undo stack is changed.
//!
//! Draws (ADR-8224): the captured frame's draw records (`regia_render::DrawRecord`) are listed
//! under their pass. Selecting one (`editor.frame_draw`, the panel's draw list, Previous draw /
//! Next draw) shows its record and selects its entity in the Outliner. No pass replays its draws
//! one by one, so a draw has no partial image (`draw_replay_unsupported`); its pass's copied
//! output is the image after all of the pass's draws.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::PassDrawCounts;
use regia_render::frame_capture::{CapturedFrame, PassCaptureRequest};
use serde_json::{Value, json};

use crate::canvas_keys::{ListKeys, RowKey};
use crate::viewport_agent::Handler;

/// Where a capture is taken from (ADR-8224).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Source {
    /// The main viewport's active pane.
    #[default]
    Viewport,
    /// The separate play window of an in-process play session.
    PlayWindow,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Viewport => "viewport",
            Self::PlayWindow => "play_window",
        }
    }
}

/// A play-window capture request in the play world (ADR-8224): [`sync_play_window`] puts it
/// there, and the play window's next render arms it.
#[derive(Resource, Clone)]
pub(crate) struct PlayCaptureRequest(PassCaptureRequest);

/// A play-window capture's outcome, left in the play world for [`sync_play_window`].
#[derive(Resource)]
struct PlayCaptureResult(Result<CapturedFrame, String>);

/// The frame debugger's transient view state. Absent means closed and idle.
#[derive(Resource, Default)]
pub struct FrameDebugger {
    /// Whether the floating inspector is visible.
    pub open: bool,
    pending: Option<usize>,
    capture: Option<Arc<CapturedFrame>>,
    selected: Option<usize>,
    keys: ListKeys,
    textures: BTreeMap<usize, egui::TextureHandle>,
    last_action_frame: Option<u64>,
    last_toggle_frame: Option<u64>,
    error: Option<String>,
    completed: u64,
    /// The pass-boundary copies the pending or last capture asked for (ADR-7860).
    request: PassCaptureRequest,
    /// The selected draw record of the capture (ADR-8224).
    draw: Option<usize>,
    draw_keys: ListKeys,
    last_draw_frame: Option<u64>,
    /// Where the pending or last capture is taken from.
    source: Source,
}

fn error(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({"code": code, "hint":"see book/agents/frame-debugger.md"})),
    }
}

/// Toggles the inspector without changing a capture or the document.
pub fn toggle(world: &mut World) -> bool {
    let mut state = world.get_resource_or_insert_with(FrameDebugger::default);
    state.open = !state.open;
    state.open
}

pub(crate) fn toggle_ui(world: &mut World, frame: u64) {
    let mut state = world.get_resource_or_insert_with(FrameDebugger::default);
    if state.last_toggle_frame != Some(frame) {
        state.last_toggle_frame = Some(frame);
        state.open = !state.open;
    }
}

/// Requests capture of the main viewport's active pane on its next render, with copies at
/// every built-in pass boundary under the default byte budget.
/// Repeated requests while pending are idempotent. A new request releases a frozen frame.
pub fn start(world: &mut World) {
    start_with(world, PassCaptureRequest::all());
}

/// [`start`] with the pass-boundary copies `request` names (ADR-7860). Only the captured frame
/// copies; live frames do no capture work.
pub fn start_with(world: &mut World, request: PassCaptureRequest) {
    start_from(world, request, Source::Viewport);
}

/// [`start_with`] for the separate play window of an in-process play session (ADR-8224): the
/// play window's next render is captured and frozen until Resume. Draws of a play-window capture
/// name play-world objects, so no entity of the edited level is resolved or selected for them.
///
/// # Errors
/// `capture_no_play_window` when no play window is open.
pub fn start_play_window(world: &mut World, request: PassCaptureRequest) -> Result<(), RpcError> {
    if !world
        .get_resource::<crate::play_window::PlayWindowStatus>()
        .is_some_and(|status| status.open)
    {
        return Err(error(
            "capture_no_play_window",
            "No play window is open; play in a separate window (in-process) first",
        ));
    }
    start_from(world, request, Source::PlayWindow);
    Ok(())
}

fn start_from(world: &mut World, request: PassCaptureRequest, source: Source) {
    if world
        .get_resource::<FrameDebugger>()
        .is_some_and(|s| s.pending.is_some())
    {
        return;
    }
    let pane = world
        .get_resource::<crate::views::ViewportLayout>()
        .map_or(0, |l| l.active);
    crate::render::release_frame_capture(world);
    let mut state = world.get_resource_or_insert_with(FrameDebugger::default);
    state.pending = Some(pane);
    state.capture = None;
    state.selected = None;
    state.keys = ListKeys::default();
    state.textures.clear();
    state.error = None;
    state.open = true;
    state.request = request;
    state.draw = None;
    state.draw_keys = ListKeys::default();
    state.source = source;
}

/// The pass-boundary request of the pending capture (`render.rs` arms it with the frame): a
/// play world's [`PlayCaptureRequest`], else the editor's.
pub(crate) fn pass_request(world: &World) -> PassCaptureRequest {
    if let Some(play) = world.get_resource::<PlayCaptureRequest>() {
        return play.0.clone();
    }
    world
        .get_resource::<FrameDebugger>()
        .map(|state| state.request.clone())
        .unwrap_or_default()
}

/// `world` is a play world whose play window is asked to capture its next frame (ADR-8224).
pub(crate) fn play_capture_requested(world: &World) -> bool {
    world.contains_resource::<PlayCaptureRequest>()
}

/// A finished capture: a play world keeps it for [`sync_play_window`], the editor completes it.
pub(crate) fn capture_complete(world: &mut World, capture: CapturedFrame) {
    if world.remove_resource::<PlayCaptureRequest>().is_some() {
        world.insert_resource(PlayCaptureResult(Ok(capture)));
    } else {
        complete(world, capture);
    }
}

/// A failed capture, kept like [`capture_complete`].
pub(crate) fn capture_failed(world: &mut World, message: String) {
    if world.remove_resource::<PlayCaptureRequest>().is_some() {
        world.insert_resource(PlayCaptureResult(Err(message)));
    } else {
        failed(world, message);
    }
}

/// Carries a play-window capture between the editor world `edit` and the in-process play world
/// `play` its play window renders (ADR-8224); the play window calls it once per rendered frame,
/// before the render. A finished capture comes back to the editor; a pending play-window
/// request goes to the play world (releasing the play window's previous frozen picture); with no
/// play-window capture pending or frozen, the play world holds no request and no frozen picture.
pub fn sync_play_window(edit: &mut World, play: &mut World) {
    let source = edit.get_resource::<FrameDebugger>().map(|state| state.source);
    let pending = edit
        .get_resource::<FrameDebugger>()
        .is_some_and(|state| state.pending.is_some());
    if let Some(PlayCaptureResult(result)) = play.remove_resource::<PlayCaptureResult>() {
        match result {
            Ok(capture) if source == Some(Source::PlayWindow) && pending => complete(edit, capture),
            Err(message) if source == Some(Source::PlayWindow) && pending => failed(edit, message),
            // Resumed or replaced while the frame was in flight.
            _ => crate::render::release_frame_capture(play),
        }
    }
    let Some(state) = edit.get_resource::<FrameDebugger>() else {
        play.remove_resource::<PlayCaptureRequest>();
        crate::render::release_frame_capture(play);
        return;
    };
    let ours = state.source == Source::PlayWindow;
    if ours && state.pending.is_some() {
        if !play.contains_resource::<PlayCaptureRequest>() {
            let request = state.request.clone();
            crate::render::release_frame_capture(play);
            play.insert_resource(PlayCaptureRequest(request));
        }
    } else {
        play.remove_resource::<PlayCaptureRequest>();
        if !(ours && state.capture.is_some()) {
            crate::render::release_frame_capture(play);
        }
    }
}

/// Releases the kept frame and resumes live viewport rendering. Also cancels a pending request.
pub fn resume(world: &mut World) {
    crate::render::release_frame_capture(world);
    if let Some(mut state) = world.get_resource_mut::<FrameDebugger>() {
        state.pending = None;
        state.capture = None;
        state.selected = None;
        state.textures.clear();
        state.error = None;
        state.draw = None;
    }
}

pub(crate) fn pending(world: &World, pane: usize) -> bool {
    world
        .get_resource::<FrameDebugger>()
        .is_some_and(|s| s.source == Source::Viewport && s.pending == Some(pane))
}

pub(crate) fn complete(world: &mut World, capture: CapturedFrame) {
    let mut state = world.get_resource_or_insert_with(FrameDebugger::default);
    state.pending = None;
    state.selected = (!capture.graph.graph.order.is_empty()).then_some(0);
    state.draw = None;
    state.draw_keys = ListKeys::default();
    state.capture = Some(Arc::new(capture));
    state.completed += 1;
    state.error = None;
}

pub(crate) fn failed(world: &mut World, message: String) {
    let mut state = world.get_resource_or_insert_with(FrameDebugger::default);
    state.pending = None;
    state.error = Some(message);
}

fn counts(capture: &CapturedFrame, name: &str) -> Option<PassDrawCounts> {
    match name {
        "shadows" => Some(capture.counts.shadow),
        "depth_prepass" => Some(capture.counts.depth_prepass),
        "forward" => Some(capture.counts.opaque.plus(capture.counts.translucent)),
        "visbuffer_resolve" => Some(capture.counts.visbuffer_resolve),
        _ => None,
    }
}

fn target_indices(capture: &CapturedFrame, name: &str) -> Vec<usize> {
    capture
        .targets
        .iter()
        .enumerate()
        .filter_map(|(index, target)| (target.after_pass.as_deref() == Some(name)).then_some(index))
        .collect()
}

/// The capture's limitations, as stable codes.
fn limitations(capture: Option<&CapturedFrame>) -> Value {
    match capture {
        Some(c) if c.pass_copies && !c.draws.is_empty() => json!([
            "pass_internal_resources_not_copied",
            "draw_records_listed_passes_only",
            "draw_replay_unsupported"
        ]),
        Some(c) if c.pass_copies => {
            json!(["pass_internal_resources_not_copied", "draw_details_not_recorded"])
        }
        _ => json!(["checkpoint_targets_only", "draw_details_not_recorded"]),
    }
}

/// Whether the graph pass `name` records its draws one by one (`regia_render::DRAW_RECORD_PASSES`).
fn records_draws(name: &str) -> bool {
    regia_render::DRAW_RECORD_PASSES
        .iter()
        .any(|(pass, _)| *pass == name)
}

/// Indices into the capture's draws of the draws of graph pass `name`, in recording order.
fn pass_draws(capture: &CapturedFrame, name: &str) -> Vec<usize> {
    capture
        .draws
        .iter()
        .enumerate()
        .filter_map(|(index, draw)| (draw.pass == name).then_some(index))
        .collect()
}

/// The live entity of a draw's object key in `world`: mesh entities extract with their
/// `Entity::to_bits` as the object key (ADR-8224). Keys of other objects (proxies, terrain, a
/// play world's entities) name no entity of the edited world.
fn object_entity(world: &World, object: u64) -> Option<Entity> {
    Entity::try_from_bits(object).filter(|entity| world.get_entity(*entity).is_ok())
}

/// One draw record as protocol JSON, with the entities its objects name in `world`.
fn draw_json(world: &World, capture: &CapturedFrame, index: usize) -> Value {
    let draw = &capture.draws[index];
    // A play-window capture's objects are the play world's: none names an edited entity.
    let edited = world
        .get_resource::<FrameDebugger>()
        .is_none_or(|state| state.source == Source::Viewport);
    let entities: Vec<u64> = draw
        .objects
        .iter()
        .filter(|_| edited)
        .filter_map(|object| object_entity(world, *object))
        .map(Entity::to_bits)
        .collect();
    let in_pass = capture.draws[..index]
        .iter()
        .filter(|d| d.pass == draw.pass)
        .count();
    json!({"index":index, "pass":draw.pass, "counted":draw.counted,
        "pass_index":capture.graph.graph.order.iter().position(|n| n == draw.pass),
        "draw_in_pass":in_pass, "draws_in_pass":capture.draws.iter().filter(|d| d.pass == draw.pass).count(),
        "pipeline":draw.pipeline, "mesh":draw.mesh, "material":draw.material, "objects":draw.objects,
        "entities":entities, "entity":entities.first(),
        "entity_name":entities.first().and_then(|bits| Entity::try_from_bits(*bits))
            .map(|entity| crate::outliner::display_name(world, entity)),
        "vertex_count":draw.vertex_count, "index_count":draw.index_count,
        "instance_count":draw.instance_count, "indirect":draw.indirect, "target":draw.target,
        "partial_image":null, "partial_image_unavailable":"draw_replay_unsupported"})
}

fn skipped_json(capture: &CapturedFrame, name: &str) -> Value {
    capture
        .skipped
        .iter()
        .filter(|skip| skip.pass == name)
        .map(|skip| json!({"pass":skip.pass, "resource":skip.resource, "reason":skip.reason}))
        .collect()
}

fn pass_json(capture: &CapturedFrame, index: usize) -> Value {
    let name = &capture.graph.graph.order[index];
    let mut row = pass_row(capture, index);
    let recorded = records_draws(name) && capture.pass_copies;
    row["draws"] = json!(pass_draws(capture, name));
    row["draw_details"] = json!(if recorded { "recorded" } else { "not_recorded" });
    row["partial_images"] = json!(if recorded {
        "draw_replay_unsupported"
    } else {
        "no_draw_records"
    });
    row
}

fn pass_row(capture: &CapturedFrame, index: usize) -> Value {
    let name = &capture.graph.graph.order[index];
    let node = capture.graph.graph.nodes.iter().find(|n| &n.name == name);
    let recorded = capture.graph.dispatch.recorded.contains(&name.as_str())
        || capture.graph.extensions.iter().any(|p| {
            &p.name == name && p.last_frame.is_some() && p.last_frame == capture.graph.dispatch.last_frame
        });
    let timing: Vec<_> = capture
        .graph
        .gpu
        .node_passes()
        .into_iter()
        .filter(|p| p.node == name.as_str())
        .collect();
    let count = counts(capture, name);
    json!({ "index":index, "name":name, "recorded":recorded,
        "inputs": node.map(|n| &n.reads), "outputs":node.map(|n| &n.writes),
        "declared_pipelines":node.map(|n| &n.pipelines),
        "gpu_ms": (recorded && capture.graph.gpu_timing && !timing.is_empty()).then(|| timing.iter().map(|p| p.ms).sum::<f32>()),
        "draw_count":count.map(|c| c.draws), "pipeline_switches":count.map(|c| c.pipeline_switches),
        "bind_group_switches":count.map(|c| c.bind_group_switches),
        "target_indices": target_indices(capture, name),
        "skipped": skipped_json(capture, name),
    })
}

/// Returns the state and captured graph rows without copying image pixels.
#[must_use]
pub fn state(world: &World) -> Value {
    let Some(view) = world.get_resource::<FrameDebugger>() else {
        return json!({"state":"idle", "open":false, "pane":null, "selected":null,
            "completed":0, "error":null, "passes":[], "frame":null, "recorded":null,
            "copies":0, "staging_bytes":0, "pass_copies":false, "skipped":[],
            "request":{"passes":[], "budget_bytes":regia_render::frame_capture::DEFAULT_PASS_CAPTURE_BYTES},
            "draws":[], "draw_selected":null, "source":"viewport",
            "limitations":limitations(None)});
    };
    let capture = view.capture.as_deref();
    let phase = if view.pending.is_some() {
        "pending"
    } else if capture.is_some() {
        "frozen"
    } else if view.error.is_some() {
        "failed"
    } else {
        "idle"
    };
    let passes: Vec<_> = capture
        .map(|c| (0..c.graph.graph.order.len()).map(|i| pass_json(c, i)).collect())
        .unwrap_or_default();
    json!({"state":phase, "open":view.open, "pane":view.pending, "selected":view.selected,
        "completed":view.completed, "error":view.error, "passes":passes,
        "frame": capture.and_then(|c| c.graph.dispatch.last_frame),
        "recorded":capture.map(|c| &c.graph.dispatch.recorded),
        "copies":capture.map_or(0, |c| c.copies), "staging_bytes":capture.map_or(0, |c| c.staging_bytes),
        "pass_copies":capture.is_some_and(|c| c.pass_copies),
        "skipped":capture.map(|c| c.skipped.iter()
            .map(|skip| json!({"pass":skip.pass, "resource":skip.resource, "reason":skip.reason}))
            .collect::<Vec<_>>()).unwrap_or_default(),
        "request":{"passes":view.request.passes, "budget_bytes":view.request.budget_bytes},
        "draws":capture.map(|c| (0..c.draws.len()).map(|i| draw_json(world, c, i)).collect::<Vec<_>>())
            .unwrap_or_default(),
        "draw_selected":view.draw, "source":view.source.name(),
        "limitations":limitations(capture)})
}

/// The optional `passes` / `budget_bytes` of a start request.
fn request_params(params: &Value) -> Result<PassCaptureRequest, RpcError> {
    let mut request = PassCaptureRequest::all();
    match params.get("passes") {
        None | Some(Value::Null) => {}
        Some(Value::Array(names)) => {
            for name in names {
                let name = name
                    .as_str()
                    .ok_or_else(|| error("capture_unknown_pass", "passes must be built-in pass names"))?;
                request.passes.push(name.to_string());
            }
        }
        Some(_) => {
            return Err(error(
                "capture_unknown_pass",
                "passes must be an array of built-in pass names",
            ));
        }
    }
    request
        .resolve()
        .map_err(|e| error(e.code(), format!("{e}; see render.graph steps for the names")))?;
    match params.get("budget_bytes") {
        None | Some(Value::Null) => {}
        Some(value) => {
            request.budget_bytes = value.as_u64().ok_or_else(|| {
                error(
                    "frame_capture_budget",
                    "budget_bytes must be a nonnegative integer",
                )
            })?;
        }
    }
    Ok(request)
}

fn capture_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match params.get("action").and_then(Value::as_str) {
        Some("start") => match params.get("source").map(|s| s.as_str()) {
            None | Some(Some("viewport")) => start_with(world, request_params(params)?),
            Some(Some("play_window")) => start_play_window(world, request_params(params)?)?,
            Some(_) => {
                return Err(error(
                    "frame_capture_action",
                    "source must be viewport or play_window",
                ));
            }
        },
        Some("resume") => resume(world),
        Some("state") => {}
        _ => {
            return Err(error(
                "frame_capture_action",
                "action must be start, state or resume",
            ));
        }
    }
    Ok(state(world))
}

fn select_pass(world: &mut World, index: usize) -> Result<Arc<CapturedFrame>, RpcError> {
    let mut state = world
        .get_resource_mut::<FrameDebugger>()
        .ok_or_else(|| error("frame_capture_missing", "Capture a frame first"))?;
    let capture = state
        .capture
        .clone()
        .ok_or_else(|| error("frame_capture_missing", "No completed capture"))?;
    if index >= capture.graph.graph.order.len() {
        return Err(error(
            "frame_pass_index",
            "Pass index is outside the captured graph",
        ));
    }
    state.selected = Some(index);
    Ok(capture)
}

fn pass_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let index = params
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| error("frame_pass_index", "index must be a nonnegative integer"))?;
    let capture = select_pass(world, index)?;
    let mut row = pass_json(&capture, index);
    let mut images = Vec::new();
    for i in target_indices(&capture, &capture.graph.graph.order[index]) {
        let target = &capture.targets[i];
        let png = target
            .pixels
            .as_ref()
            .map(|pixels| {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [target.size[0] as usize, target.size[1] as usize],
                    pixels,
                );
                crate::render::viewport_png_base64(&image).map_err(|m| error("frame_image_encode", m))
            })
            .transpose()?;
        images.push(
            json!({"checkpoint":target.checkpoint, "resource":target.resource, "version":target.version, "after_pass":target.after_pass, "frame":target.frame,
            "width":target.size[0], "height":target.size[1], "format":format!("{:?}", target.format),
            "encoding":target.encoding, "png_base64":png, "unavailable":target.unavailable}),
        );
    }
    let skipped = row["skipped"].as_array().is_some_and(|s| !s.is_empty());
    row["targets"] = json!(images);
    row["targets_unavailable"] = if !images.is_empty() {
        Value::Null
    } else if skipped {
        json!("pass_outputs_skipped")
    } else {
        json!("no_checkpoint_for_pass_output")
    };
    Ok(row)
}

/// Selects draw `index` of the capture (ADR-8224): its pass row becomes the selected pass, and
/// with `select_entity` the entity its first object names becomes the Outliner selection.
fn select_draw(world: &mut World, index: usize, select_entity: bool) -> Result<Value, RpcError> {
    let capture = world
        .get_resource::<FrameDebugger>()
        .ok_or_else(|| error("frame_capture_missing", "Capture a frame first"))?
        .capture
        .clone()
        .ok_or_else(|| error("frame_capture_missing", "No completed capture"))?;
    if index >= capture.draws.len() {
        return Err(error(
            "frame_draw_index",
            format!(
                "Draw index is outside the capture's {} recorded draws",
                capture.draws.len()
            ),
        ));
    }
    let mut row = draw_json(world, &capture, index);
    let pass = row["pass_index"].as_u64().and_then(|i| usize::try_from(i).ok());
    {
        let mut state = world.resource_mut::<FrameDebugger>();
        state.draw = Some(index);
        if pass.is_some() {
            state.selected = pass;
        }
    }
    let entity = row["entity"].as_u64().and_then(Entity::try_from_bits);
    let selected = match entity {
        Some(entity) if select_entity => {
            world
                .get_resource_or_insert_with(crate::selection::Selection::default)
                .select(entity, crate::selection::SelectMode::Replace);
            Some(entity.to_bits())
        }
        _ => None,
    };
    row["selected_entity"] = json!(selected);
    row["pass_targets"] = json!(target_indices(&capture, capture.draws[index].pass));
    Ok(row)
}

/// Steps the selected draw by `delta` (Previous draw / Next draw), clamped to the capture.
fn step_draw(world: &mut World, delta: isize, select_entity: bool) -> Result<Value, RpcError> {
    let (current, len) = world
        .get_resource::<FrameDebugger>()
        .and_then(|state| Some((state.draw, state.capture.as_ref()?.draws.len())))
        .ok_or_else(|| error("frame_capture_missing", "Capture a frame first"))?;
    if len == 0 {
        return Err(error("frame_draw_index", "The capture recorded no draws"));
    }
    let next = match current {
        Some(at) => at.saturating_add_signed(delta).min(len - 1),
        None if delta < 0 => len - 1,
        None => 0,
    };
    select_draw(world, next, select_entity)
}

fn draw_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let select_entity = match params.get("select") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(select)) => *select,
        Some(_) => return Err(error("frame_draw_index", "select must be a boolean")),
    };
    match (params.get("index"), params.get("step")) {
        (Some(index), None) => {
            let index = index
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| error("frame_draw_index", "index must be a nonnegative integer"))?;
            select_draw(world, index, select_entity)
        }
        (None, Some(step)) => {
            let delta = match step.as_str() {
                Some("next") => 1,
                Some("previous") => -1,
                _ => return Err(error("frame_draw_index", "step must be next or previous")),
            };
            step_draw(world, delta, select_entity)
        }
        _ => Err(error(
            "frame_draw_index",
            "give index (a nonnegative integer) or step (next or previous)",
        )),
    }
}

/// Editor protocol schemas and handlers (the extension dispatcher routes these methods).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.frame_capture",
                tool: "frame_capture",
                description: "Capture the next active editor viewport frame, inspect its state, or resume live rendering.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "start, state or resume.",
                    },
                    ParamSchema {
                        name: "passes",
                        kind: "array",
                        required: false,
                        description: "start: built-in pass names to copy after (render.graph steps); omitted or empty copies after every pass.",
                    },
                    ParamSchema {
                        name: "budget_bytes",
                        kind: "integer",
                        required: false,
                        description: "start: staging bytes the pass copies may use (default 268435456); copies over it are listed in skipped.",
                    },
                    ParamSchema {
                        name: "source",
                        kind: "string",
                        required: false,
                        description: "start: viewport (default, the main viewport's active pane) or play_window (the in-process play window).",
                    },
                ],
                result: "{ state, open, pane?, frame?, passes, recorded?, selected?, completed, copies, staging_bytes, pass_copies, skipped, request, draws, draw_selected?, source, limitations, error? }",
                mutating: false,
                permission: Permission::Read,
            },
            capture_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.frame_pass",
                tool: "frame_pass",
                description: "Select a captured pass and read its copied outputs and checkpoint targets as PNG images.",
                params: &[ParamSchema {
                    name: "index",
                    kind: "number",
                    required: true,
                    description: "Zero-based index in editor.frame_capture state passes.",
                }],
                result: "{ index, name, recorded, inputs, outputs, declared_pipelines, gpu_ms?, draw_count?, pipeline_switches?, bind_group_switches?, draws, draw_details, partial_images, target_indices, skipped, targets, targets_unavailable? }",
                mutating: false,
                permission: Permission::Read,
            },
            pass_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.frame_draw",
                tool: "frame_draw",
                description: "Select a recorded draw of the captured frame (by index, or step next/previous), read its record, and select its entity in the Outliner.",
                params: &[
                    ParamSchema {
                        name: "index",
                        kind: "integer",
                        required: false,
                        description: "Zero-based index in editor.frame_capture state draws (give index or step).",
                    },
                    ParamSchema {
                        name: "step",
                        kind: "string",
                        required: false,
                        description: "next or previous: moves from the selected draw (give index or step).",
                    },
                    ParamSchema {
                        name: "select",
                        kind: "boolean",
                        required: false,
                        description: "Select the draw's entity in the Outliner (default true).",
                    },
                ],
                result: "{ index, pass, counted, pass_index, draw_in_pass, draws_in_pass, pipeline, mesh, material, objects, entities, entity?, entity_name?, vertex_count?, index_count?, instance_count?, indirect, target, partial_image, partial_image_unavailable, selected_entity?, pass_targets }",
                mutating: false,
                permission: Permission::Read,
            },
            draw_method as Handler,
        ),
    ]
}

fn panel_ui(ui: &mut egui::Ui, world: &mut World) {
    let frame = ui.ctx().cumulative_frame_nr();
    let mut action = None;
    let mut play = false;
    ui.horizontal(|ui| {
        if crate::agent_parity::action_button(ui, &crate::agent_parity::FRAME_CAPTURE).clicked() {
            action = Some(true);
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::FRAME_RESUME).clicked() {
            action = Some(false);
        }
        // The in-process play window is a capture source while it is open (ADR-8224).
        let play_open = world
            .get_resource::<crate::play_window::PlayWindowStatus>()
            .is_some_and(|status| status.open);
        if play_open
            && crate::agent_parity::action_button(ui, &crate::agent_parity::FRAME_CAPTURE_PLAY_WINDOW)
                .clicked()
        {
            play = true;
        }
    });
    if let Some(start_capture) = action.or(play.then_some(true)) {
        let mut view = world.get_resource_or_insert_with(FrameDebugger::default);
        if view.last_action_frame != Some(frame) {
            view.last_action_frame = Some(frame);
            if play {
                if let Err(err) = start_play_window(world, PassCaptureRequest::all()) {
                    world.resource_mut::<FrameDebugger>().error = Some(err.message);
                }
            } else if start_capture {
                start(world);
            } else {
                resume(world);
            }
        }
    }
    let (capture, activate) = {
        let mut view = world.get_resource_or_insert_with(FrameDebugger::default);
        if let Some(message) = &view.error {
            ui.label(message);
        }
        if view.pending.is_some() {
            ui.label("Waiting for the next viewport frame.");
        }
        let Some(capture) = view.capture.clone() else {
            ui.label("Capture a frame to inspect its render passes.");
            return;
        };
        ui.label("Viewport frozen. Resume returns to live rendering.");
        let selected = view.selected;
        let events = crate::canvas_keys::list_rows(
            ui,
            "frame-debugger-passes",
            &mut view.keys,
            capture.graph.graph.order.len(),
            crate::canvas_keys::LIST_HEIGHT,
            selected,
            &|i| format!("Pass {i}: {}", capture.graph.graph.order[i]),
        );
        let activate = events.into_iter().find_map(|event| match event {
            RowKey::Activate(index) => Some(index),
            _ => None,
        });
        (capture, activate)
    };
    if let Some(index) = activate {
        let _ = select_pass(world, index);
    }
    let Some(index) = world.resource::<FrameDebugger>().selected else {
        return;
    };
    let row = pass_json(&capture, index);
    ui.strong(row["name"].as_str().unwrap_or_default());
    ui.label(format!("Inputs: {}", row["inputs"]));
    ui.label(format!("Outputs: {}", row["outputs"]));
    ui.label(format!("Declared pipeline keys: {}", row["declared_pipelines"]));
    ui.label(format!(
        "Counted draws: {} · GPU ms: {}",
        row["draw_count"], row["gpu_ms"]
    ));
    let name = &capture.graph.graph.order[index];
    draws_ui(ui, world, &capture, name, frame);
    let mut view = world.resource_mut::<FrameDebugger>();
    let targets = target_indices(&capture, name);
    let skipped: Vec<_> = capture
        .skipped
        .iter()
        .filter(|skip| skip.pass == name.as_str())
        .collect();
    if targets.is_empty() && skipped.is_empty() {
        ui.label("No copied output or checkpoint for this pass.");
    }
    for skip in skipped {
        ui.label(format!("{}: not copied", skip.resource))
            .on_hover_text(format!("Agent reason: {}", skip.reason));
    }
    for i in targets {
        let target = &capture.targets[i];
        ui.label(format!(
            "{} at {} ({})",
            target.resource, target.checkpoint, target.encoding
        ));
        if let Some(pixels) = &target.pixels {
            let texture = view.textures.entry(i).or_insert_with(|| {
                ui.ctx().load_texture(
                    format!("frame-debugger-{i}"),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [target.size[0] as usize, target.size[1] as usize],
                        pixels,
                    ),
                    egui::TextureOptions::NEAREST,
                )
            });
            ui.add(
                egui::Image::from_texture(&*texture)
                    .max_width(ui.available_width())
                    .alt_text(format!("{} preview at {}", target.resource, target.checkpoint)),
            );
        } else {
            ui.label(target.unavailable.unwrap_or("capture_unavailable"));
        }
    }
}

/// The selected pass's draws (ADR-8224): a keyboard list, Previous draw / Next draw, and the
/// selected draw's record. Activating a row or a step selects the draw's entity in the Outliner,
/// once per egui frame (`frame`).
fn draws_ui(ui: &mut egui::Ui, world: &mut World, capture: &CapturedFrame, name: &str, frame: u64) {
    if !(records_draws(name) && capture.pass_copies) {
        ui.label("This pass's draws are counted, not recorded one by one.");
        return;
    }
    let draws = pass_draws(capture, name);
    ui.label(format!("Draws: {}", draws.len()));
    let mut step = None;
    ui.horizontal(|ui| {
        if crate::agent_parity::action_button(ui, &crate::agent_parity::FRAME_DRAW_PREVIOUS).clicked() {
            step = Some(-1);
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::FRAME_DRAW_NEXT).clicked() {
            step = Some(1);
        }
    });
    let activate = {
        let mut view = world.resource_mut::<FrameDebugger>();
        let selected = view.draw.and_then(|d| draws.iter().position(|&i| i == d));
        let events = crate::canvas_keys::list_rows(
            ui,
            "frame-debugger-draws",
            &mut view.draw_keys,
            draws.len(),
            crate::canvas_keys::LIST_HEIGHT,
            selected,
            &|row| {
                let draw = &capture.draws[draws[row]];
                format!("Draw {}: {} ({})", draws[row], draw.pipeline, draw.counted)
            },
        );
        events.into_iter().find_map(|event| match event {
            RowKey::Activate(row) => draws.get(row).copied(),
            _ => None,
        })
    };
    if step.is_some() || activate.is_some() {
        let fresh = {
            let mut view = world.resource_mut::<FrameDebugger>();
            if view.last_draw_frame == Some(frame) {
                false
            } else {
                view.last_draw_frame = Some(frame);
                true
            }
        };
        if fresh {
            let result = match (activate, step) {
                (Some(index), _) => select_draw(world, index, true),
                (None, Some(delta)) => step_draw(world, delta, true),
                (None, None) => Ok(Value::Null),
            };
            if let Err(err) = result {
                world.resource_mut::<FrameDebugger>().error = Some(err.message);
            }
        }
    }
    let Some(index) = world.resource::<FrameDebugger>().draw else {
        ui.label("Select a draw to see its record and its entity.");
        return;
    };
    let row = draw_json(world, capture, index);
    ui.strong(format!("Draw {index} of {}", capture.draws.len()));
    ui.label(format!("Pass: {} ({})", row["pass"], row["counted"]));
    ui.label(format!("Pipeline: {}", row["pipeline"]));
    ui.label(format!(
        "Mesh: {} · Material: {} · Objects: {}",
        row["mesh"], row["material"], row["objects"]
    ));
    ui.label(format!(
        "Indices: {} · Vertices: {} · Instances: {} · Indirect: {}",
        row["index_count"], row["vertex_count"], row["instance_count"], row["indirect"]
    ));
    ui.label(format!("Target: {}", row["target"]));
    match row["entity_name"].as_str() {
        Some(entity) => ui.label(format!("Entity: {entity}")),
        None => ui.label("Entity: none in the edited level"),
    };
    ui.label("No partial image: this pass does not replay its draws one by one; the pass output below is the image after all its draws.");
}

/// Draws the inspector when open. Closed panels perform no graph reads or image work.
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(state) = world.get_resource::<FrameDebugger>() else {
        return;
    };
    if !state.open {
        return;
    }
    let _span = tracing::info_span!("frame_debugger_panel").entered();
    let mut open = true;
    egui::Window::new("Frame Debugger")
        .default_pos(crate::tool_windows::default_pos(ctx, "Frame Debugger"))
        .open(&mut open)
        .default_width(600.0)
        .vscroll(true)
        .show(ctx, |ui| panel_ui(ui, world));
    world.resource_mut::<FrameDebugger>().open = open;
}

#[cfg(test)]
mod tests {
    use regia_agent::extension::AgentExtension;
    use regia_ecs::prelude::*;
    use serde_json::{Value, json};

    #[test]
    fn frame_debugger_agent_surface_starts_pending_and_resumes() {
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        let extension = world.resource::<AgentExtension>().clone();
        let (_, capture) = extension
            .method("editor.frame_capture")
            .expect("frame capture route");
        let (_, pass) = extension.method("editor.frame_pass").expect("frame pass route");
        assert_eq!(
            capture(&mut world, &json!({"action":"state"})).expect("state")["state"],
            "idle"
        );
        assert_eq!(
            capture(&mut world, &json!({"action":"start"})).expect("start")["state"],
            "pending"
        );
        assert!(pass(&mut world, &json!({"index":0})).is_err());
        assert_eq!(
            capture(&mut world, &json!({"action":"resume"})).expect("resume")["state"],
            "idle"
        );
    }

    #[test]
    fn frame_debugger_base_has_no_gbuffer_albedo_target() {
        let report = regia_render::FrameRenderer::new().graph_report();
        assert!(report.graph.nodes.iter().any(|n| n.name == "forward"));
        assert!(
            !report
                .graph
                .nodes
                .iter()
                .flat_map(|n| &n.writes)
                .any(|name| name.contains("gbuffer"))
        );
        assert!(regia_render::DebugView::is_supported(
            regia_render::DebugViewMode::BaseColor
        ));
    }

    fn snapshot_fixture() -> regia_render::frame_capture::CapturedFrame {
        regia_render::frame_capture::CapturedFrame {
            graph: regia_render::FrameRenderer::new().graph_report(),
            counts: Default::default(),
            targets: Vec::new(),
            copies: 0,
            staging_bytes: 0,
            skipped: vec![regia_render::frame_capture::SkippedCopy {
                pass: "shadows",
                resource: "shadow_atlas",
                reason: "capture_internal",
            }],
            pass_copies: true,
            draws: Vec::new(),
        }
    }

    /// A capture whose forward pass recorded one opaque draw per entity of `objects` (their
    /// `to_bits` as object keys, as the extraction gives them), then one draw of a key that
    /// names no entity (an editor addition such as the backdrop sky).
    fn draw_fixture(objects: &[Entity]) -> regia_render::frame_capture::CapturedFrame {
        let mut capture = snapshot_fixture();
        for (i, entity) in objects.iter().enumerate() {
            capture.draws.push(regia_render::DrawRecord {
                pass: "forward",
                counted: "opaque",
                pipeline: "standard",
                mesh: Some(100 + i as u64),
                material: Some(200 + i as u64),
                objects: vec![entity.to_bits()],
                index_count: Some(6),
                instance_count: Some(1),
                target: "hdr_color",
                ..Default::default()
            });
        }
        capture.draws.push(regia_render::DrawRecord {
            pass: "forward",
            counted: "opaque",
            pipeline: "standard",
            objects: vec![0x5ce0_0000_0000_0001],
            index_count: Some(36),
            instance_count: Some(1),
            target: "hdr_color",
            ..Default::default()
        });
        capture
    }

    fn forward_index(world: &World) -> usize {
        super::state(world)["passes"]
            .as_array()
            .and_then(|rows| rows.iter().position(|row| row["name"] == "forward"))
            .expect("forward row")
    }

    /// P-310 (ADR-8224): a capture of three mesh entities lists their three draws under the
    /// forward pass with the right entities; selecting a draw selects its entity in the Outliner;
    /// stepping moves through the draws and stops at the ends; a draw has no partial image.
    #[test]
    fn frame_debugger_lists_draws_under_their_pass_and_selecting_one_selects_its_entity() {
        let mut world = World::new();
        let entities: Vec<Entity> = (0..3).map(|_| world.spawn_empty().id()).collect();
        super::complete(&mut world, draw_fixture(&entities));
        let state = super::state(&world);
        let draws = state["draws"].as_array().expect("draws");
        assert_eq!(draws.len(), 4);
        for (i, entity) in entities.iter().enumerate() {
            assert_eq!(draws[i]["entity"], entity.to_bits(), "{}", draws[i]);
            assert_eq!(draws[i]["pass"], "forward");
            assert_eq!(draws[i]["counted"], "opaque");
            assert_eq!(draws[i]["draw_in_pass"], i);
        }
        assert_eq!(
            draws[3]["entity"],
            Value::Null,
            "an editor addition names no entity"
        );
        assert_eq!(
            state["limitations"],
            json!([
                "pass_internal_resources_not_copied",
                "draw_records_listed_passes_only",
                "draw_replay_unsupported"
            ])
        );
        let forward = forward_index(&world);
        let row = &state["passes"][forward];
        assert_eq!(row["draws"], json!([0, 1, 2, 3]));
        assert_eq!(row["draw_details"], "recorded");
        assert_eq!(row["partial_images"], "draw_replay_unsupported");
        let shadows = state["passes"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["name"] == "shadows"))
            .expect("shadows");
        assert_eq!(shadows["draw_details"], "not_recorded");

        let picked = super::draw_method(&mut world, &json!({"index":1})).expect("draw 1");
        assert_eq!(picked["selected_entity"], entities[1].to_bits());
        assert_eq!(picked["partial_image"], Value::Null);
        assert_eq!(picked["partial_image_unavailable"], "draw_replay_unsupported");
        assert_eq!(picked["pass_index"], forward);
        let selection = world.resource::<crate::selection::Selection>();
        assert_eq!(selection.primary(), Some(entities[1]));
        assert_eq!(super::state(&world)["draw_selected"], 1);
        assert_eq!(super::state(&world)["selected"], forward);
        // Step forward and back; the ends hold.
        let next = super::draw_method(&mut world, &json!({"step":"next"})).expect("next");
        assert_eq!(next["index"], 2);
        assert_eq!(
            world.resource::<crate::selection::Selection>().primary(),
            Some(entities[2])
        );
        super::draw_method(&mut world, &json!({"step":"next"})).expect("next");
        let last = super::draw_method(&mut world, &json!({"step":"next"})).expect("held");
        assert_eq!(last["index"], 3, "the last draw holds");
        assert_eq!(last["selected_entity"], Value::Null);
        assert_eq!(
            world.resource::<crate::selection::Selection>().primary(),
            Some(entities[2]),
            "a draw with no entity leaves the selection"
        );
        let back = super::draw_method(&mut world, &json!({"step":"previous"})).expect("previous");
        assert_eq!(back["index"], 2);
        // select: false reads the record only.
        let read = super::draw_method(&mut world, &json!({"index":0, "select":false})).expect("read");
        assert_eq!(read["selected_entity"], Value::Null);
        assert_eq!(
            world.resource::<crate::selection::Selection>().primary(),
            Some(entities[2])
        );
        // Contracted errors; a refused call changes nothing.
        let contract = regia_agent::contract::contract("editor.frame_draw").expect("contract");
        for params in [
            json!({"index":4}),
            json!({"index":-1}),
            json!({"index":"0"}),
            json!({}),
            json!({"step":"sideways"}),
            json!({"index":0, "step":"next"}),
            json!({"index":0, "select":"yes"}),
        ] {
            let err = super::draw_method(&mut world, &params).expect_err("refused");
            let code = err.data.as_ref().expect("code")["code"].clone();
            assert_eq!(code, "frame_draw_index", "{params}");
            assert!(contract.errors.contains(&"frame_draw_index"));
            assert_eq!(super::state(&world)["draw_selected"], 0, "{params}");
        }
        super::resume(&mut world);
        let err = super::draw_method(&mut world, &json!({"index":0})).expect_err("no capture");
        assert_eq!(err.data.as_ref().expect("code")["code"], "frame_capture_missing");
        assert!(contract.errors.contains(&"frame_capture_missing"));
        assert_eq!(super::state(&world)["draws"], json!([]));
    }

    /// P-310 item 3 (ADR-8224): with an in-process play window open, a play-window start is
    /// pending without arming the viewport; `sync_play_window` hands the request to the play
    /// world, the play world's finished capture comes back frozen into the editor (its draws name
    /// no edited entity), and Resume clears the play world's request. Without a play window the
    /// start is refused with `capture_no_play_window` and nothing is armed.
    #[test]
    fn frame_debugger_play_window_capture_crosses_from_the_play_world() {
        let mut edit = World::new();
        let mut play = World::new();
        let err = super::capture_method(&mut edit, &json!({"action":"start", "source":"play_window"}))
            .expect_err("no play window");
        assert_eq!(err.data.as_ref().expect("code")["code"], "capture_no_play_window");
        assert!(
            regia_agent::contract::contract("editor.frame_capture")
                .expect("contract")
                .errors
                .contains(&"capture_no_play_window")
        );
        assert_eq!(super::state(&edit)["state"], "idle");
        let err = super::capture_method(&mut edit, &json!({"action":"start", "source":"window"}))
            .expect_err("bad source");
        assert_eq!(err.data.as_ref().expect("code")["code"], "frame_capture_action");
        edit.insert_resource(crate::play_window::PlayWindowStatus {
            open: true,
            ..Default::default()
        });
        let state = super::capture_method(&mut edit, &json!({"action":"start", "source":"play_window"}))
            .expect("start");
        assert_eq!(state["state"], "pending");
        assert_eq!(state["source"], "play_window");
        assert!(!super::pending(&edit, 0), "the viewport is not armed");
        super::sync_play_window(&mut edit, &mut play);
        assert!(super::play_capture_requested(&play));
        assert_eq!(super::pass_request(&play), super::pass_request(&edit));
        // The play window's render finishes the capture in the play world.
        let entities: Vec<Entity> = (0..3).map(|_| play.spawn_empty().id()).collect();
        super::capture_complete(&mut play, draw_fixture(&entities));
        assert!(!super::play_capture_requested(&play));
        assert!(!play.contains_resource::<super::FrameDebugger>());
        super::sync_play_window(&mut edit, &mut play);
        let state = super::state(&edit);
        assert_eq!(state["state"], "frozen");
        assert_eq!(state["draws"].as_array().map(Vec::len), Some(4));
        for draw in state["draws"].as_array().expect("draws") {
            assert_eq!(
                draw["entity"],
                Value::Null,
                "play-world objects name no edited entity"
            );
        }
        let picked = super::draw_method(&mut edit, &json!({"index":0})).expect("draw");
        assert_eq!(picked["selected_entity"], Value::Null);
        // Frozen: the play world gets no new request.
        super::sync_play_window(&mut edit, &mut play);
        assert!(!super::play_capture_requested(&play));
        super::capture_method(&mut edit, &json!({"action":"resume"})).expect("resume");
        super::sync_play_window(&mut edit, &mut play);
        assert!(!super::play_capture_requested(&play));
        // A failed play-window capture reaches the editor as failed.
        super::capture_method(&mut edit, &json!({"action":"start", "source":"play_window"})).expect("start");
        super::sync_play_window(&mut edit, &mut play);
        super::capture_failed(&mut play, "Split-screen frame capture is not supported".into());
        super::sync_play_window(&mut edit, &mut play);
        assert_eq!(super::state(&edit)["state"], "failed");
    }

    /// The draw list and Previous/Next draw are keyboard operable at every UI scale: Tab reaches
    /// Next draw and Enter selects the first draw's entity; Tab reaches a draw row, Down moves
    /// to the next row and Enter selects that draw and its entity. No widget is unnamed.
    #[test]
    fn frame_debugger_draws_keyboard_next_and_row_activation_at_ui_scales() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        for scale in [1.0, 1.5, 2.0] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let mut world = World::new();
            let entities: Vec<Entity> = (0..3).map(|_| world.spawn_empty().id()).collect();
            super::complete(&mut world, draw_fixture(&entities));
            let forward = forward_index(&world);
            super::select_pass(&mut world, forward).expect("forward");
            let size = [1200.0, 2400.0];
            let drawn = frame(&ctx, size, vec![], |ui| super::panel_ui(ui, &mut world));
            assert!(
                crate::a11y_audit::unnamed_widgets(&drawn).is_empty(),
                "scale {scale}"
            );
            tab_to(&ctx, size, "Next draw", false, 40, |ui| {
                super::panel_ui(ui, &mut world)
            });
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            assert_eq!(super::state(&world)["draw_selected"], 0, "scale {scale}");
            assert_eq!(
                world.resource::<crate::selection::Selection>().primary(),
                Some(entities[0]),
                "scale {scale}"
            );
            tab_to(&ctx, size, "Draw 0: standard (opaque)", false, 40, |ui| {
                super::panel_ui(ui, &mut world)
            });
            frame(
                &ctx,
                size,
                vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            frame(&ctx, size, vec![], |ui| super::panel_ui(ui, &mut world));
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            assert_eq!(super::state(&world)["draw_selected"], 1, "scale {scale}");
            assert_eq!(
                world.resource::<crate::selection::Selection>().primary(),
                Some(entities[1]),
                "scale {scale}"
            );
        }
    }

    #[test]
    fn frame_debugger_keyboard_capture_select_resume_at_ui_scales() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        for scale in [1.0, 1.5, 2.0] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let mut world = World::new();
            let size = [1200.0, 1000.0];
            let accessible = frame(&ctx, size, vec![], |ui| super::panel_ui(ui, &mut world));
            assert!(crate::a11y_audit::unnamed_widgets(&accessible).is_empty());
            tab_to(&ctx, size, "Capture frame", false, 8, |ui| {
                super::panel_ui(ui, &mut world)
            });
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            assert_eq!(super::state(&world)["state"], "pending", "scale {scale}");
            let capture = snapshot_fixture();
            let name = format!("Pass 0: {}", capture.graph.graph.order[0]);
            super::complete(&mut world, capture);
            frame(&ctx, size, vec![], |ui| super::panel_ui(ui, &mut world));
            tab_to(&ctx, size, &name, false, 12, |ui| super::panel_ui(ui, &mut world));
            frame(
                &ctx,
                size,
                vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            frame(&ctx, size, vec![], |ui| super::panel_ui(ui, &mut world));
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            assert_eq!(super::state(&world)["selected"], 1, "scale {scale}");
            tab_to(&ctx, size, "Resume", true, 12, |ui| {
                super::panel_ui(ui, &mut world)
            });
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Space, egui::Modifiers::NONE)],
                |ui| super::panel_ui(ui, &mut world),
            );
            assert_eq!(super::state(&world)["state"], "idle", "scale {scale}");
        }
    }

    #[test]
    fn frame_debugger_start_takes_a_pass_list_and_budget_and_lists_skipped_outputs() {
        let mut world = World::new();
        let state = super::capture_method(
            &mut world,
            &json!({"action":"start", "passes":["forward"], "budget_bytes":4096}),
        )
        .expect("start");
        assert_eq!(state["state"], "pending");
        assert_eq!(state["request"]["passes"], json!(["forward"]));
        assert_eq!(state["request"]["budget_bytes"], 4096);
        assert_eq!(super::pass_request(&world).passes, vec!["forward".to_string()]);
        super::resume(&mut world);
        // A plain start asks for every pass under the default budget.
        super::capture_method(&mut world, &json!({"action":"start"})).expect("start");
        assert_eq!(
            super::pass_request(&world),
            regia_render::frame_capture::PassCaptureRequest::all()
        );
        super::resume(&mut world);
        let contract = regia_agent::contract::contract("editor.frame_capture").expect("contract");
        for (params, code) in [
            (
                json!({"action":"start", "passes":["gbuffer"]}),
                "capture_unknown_pass",
            ),
            (
                json!({"action":"start", "passes":"forward"}),
                "capture_unknown_pass",
            ),
            (json!({"action":"start", "passes":[3]}), "capture_unknown_pass"),
            (
                json!({"action":"start", "budget_bytes":-1}),
                "frame_capture_budget",
            ),
            (
                json!({"action":"start", "budget_bytes":1.5}),
                "frame_capture_budget",
            ),
        ] {
            let err = super::capture_method(&mut world, &params).expect_err("refused");
            assert_eq!(err.data.as_ref().expect("code")["code"], code, "{params}");
            assert!(contract.errors.contains(&code), "{code} is in the contract");
            assert_eq!(
                super::state(&world)["state"],
                "idle",
                "a refused start arms nothing"
            );
        }
        super::complete(&mut world, snapshot_fixture());
        let state = super::state(&world);
        assert_eq!(state["pass_copies"], true);
        assert_eq!(
            state["limitations"],
            json!(["pass_internal_resources_not_copied", "draw_details_not_recorded"])
        );
        assert_eq!(state["skipped"][0]["reason"], "capture_internal");
        let shadows = state["passes"]
            .as_array()
            .and_then(|rows| rows.iter().position(|row| row["name"] == "shadows"))
            .expect("shadows row");
        let row = super::pass_method(&mut world, &json!({"index":shadows})).expect("pass");
        assert_eq!(row["skipped"][0]["resource"], "shadow_atlas");
        assert_eq!(row["targets_unavailable"], "pass_outputs_skipped");
        assert!(
            regia_agent::contract::contract("editor.frame_pass")
                .expect("contract")
                .errors
                .contains(&"frame_pass_index")
        );
    }

    #[test]
    fn frame_debugger_double_ui_pass_does_not_toggle_twice() {
        let mut world = World::new();
        super::toggle_ui(&mut world, 10);
        super::toggle_ui(&mut world, 10);
        assert_eq!(super::state(&world)["open"], true);
        super::toggle_ui(&mut world, 11);
        assert_eq!(super::state(&world)["open"], false);
    }

    #[test]
    fn frame_debugger_closed_and_idle_never_poll_the_renderer() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        let before = regia_render::frame_path_counters();
        for _ in 0..64 {
            super::window(&ctx, &mut world);
        }
        assert!(!world.contains_resource::<super::FrameDebugger>());
        let after = regia_render::frame_path_counters().since(before);
        assert_eq!(
            (
                after.buffers,
                after.textures,
                after.submits,
                after.map_asyncs,
                after.blocking_polls
            ),
            (0, 0, 0, 0, 0)
        );
        super::start(&mut world);
        super::start(&mut world);
        assert_eq!(super::state(&world)["completed"], 0);
        super::resume(&mut world);
        assert!(!super::pending(&world, 0));
    }

    #[test]
    fn frame_debugger_invalid_parameters_are_contracted() {
        let mut world = World::new();
        for action in [json!(null), json!(3), json!("unknown")] {
            let err = super::capture_method(&mut world, &json!({"action":action})).expect_err("action");
            assert_eq!(err.data.as_ref().expect("code")["code"], "frame_capture_action");
            assert!(
                regia_agent::contract::contract("editor.frame_capture")
                    .expect("contract")
                    .errors
                    .contains(&"frame_capture_action")
            );
        }
        for index in [json!(-1), json!(0.5), json!("0"), json!(null)] {
            let err = super::pass_method(&mut world, &json!({"index":index})).expect_err("index");
            assert_eq!(err.data.as_ref().expect("code")["code"], "frame_pass_index");
        }
        assert!(
            regia_agent::contract::contract("editor.frame_pass")
                .expect("contract")
                .errors
                .contains(&"frame_pass_index")
        );
        let err = super::pass_method(&mut world, &json!({"index":0})).expect_err("missing");
        assert_eq!(err.data.as_ref().expect("code")["code"], "frame_capture_missing");
        assert!(
            regia_agent::contract::contract("editor.frame_pass")
                .expect("contract")
                .errors
                .contains(&"frame_capture_missing")
        );
        super::complete(&mut world, snapshot_fixture());
        let before = super::state(&world)["selected"].clone();
        assert!(super::pass_method(&mut world, &json!({"index":u64::MAX})).is_err());
        assert_eq!(super::state(&world)["selected"], before);
    }
}
