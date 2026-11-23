// SPDX-License-Identifier: MPL-2.0
//! Viewport tools and overlays extensions register (ADR-7791 (4), spec addendum § 20.2).
//!
//! A **viewport tool** ([`ViewportToolDesc`]) is an entry with an icon and a name in the
//! viewport's Tools menu. While it is the active tool, the viewport draws its handles through the
//! same handle API the collider grips use (`crate::handles::handles`: points, arrows, dials,
//! drawn and hit-tested from raw pointer input) and routes their drags to the tool's callback:
//! [`ToolPhase::Begin`], then [`ToolPhase::Drag`] per move, then [`ToolPhase::End`]. The callback
//! previews on the world as it goes and answers, at the end, the [`EditorCommand`] of the whole
//! drag; the editor pushes it on the shared undo stack without applying it again, so one drag is
//! one undo step. The viewport asks the tool for its handles after the transform gizmo and the
//! built-in grips decided the press (one owner per press, ADR-7674): a press on a tool handle is
//! the tool's, any other press stays the pane's.
//!
//! An **overlay** ([`OverlayDesc`]) is a small panel drawn over each viewport pane in a corner,
//! shown until the user (Tools menu) or an agent hides it.
//!
//! The agent surface is `editor.extension_tools` (`book/agents/editor-extensions.md`).

use std::collections::BTreeSet;
use std::sync::Arc;

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::extension::ExtensionRegistry;
use regia_framework::extension::registry::RegistrationKind;
use serde_json::{Value, json};

use super::context::{EditorExtensions, PanelDraw};
use crate::command::{EditorCommand, UndoStack};
use crate::handles::{DragPhase, Handle, SnapSet};
use crate::views::ViewCamera;

/// The handles a tool shows now, read from the world each frame.
pub type ToolHandles = Arc<dyn Fn(&World) -> Vec<Handle> + Send + Sync>;

/// A drag phase a tool sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    /// The button went down on a handle.
    Begin,
    /// The handle moved.
    Drag,
    /// The button went up: answer the drag's undo command.
    End,
}

/// One step of a handle drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolDrag {
    /// Index into the tool's handles.
    pub handle: usize,
    /// The phase.
    pub phase: ToolPhase,
    /// Where the handle's anchor now is (world space).
    pub position: Vec3,
    /// Movement since the previous step.
    pub delta: Vec3,
    /// Movement since the drag began.
    pub total: Vec3,
}

/// Applies a drag step to the world; on [`ToolPhase::End`] answers the command that undoes and
/// redoes the whole drag (already applied: the editor only records it), or `None` for no edit.
pub type ToolDragRun = Arc<dyn Fn(&mut World, &ToolDrag) -> Option<Box<dyn EditorCommand>> + Send + Sync>;

/// A viewport tool an extension registers.
#[derive(Clone)]
pub struct ViewportToolDesc {
    /// `<ext>.<tool>`.
    pub id: String,
    /// The name in the Tools menu (also its accessible name).
    pub title: String,
    /// A short glyph shown before the name.
    pub icon: String,
    /// Its handles.
    pub handles: ToolHandles,
    /// Its drag callback.
    pub drag: ToolDragRun,
}

/// The corner of the viewport pane an overlay sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayCorner {
    /// Top left.
    TopLeft,
    /// Top right.
    #[default]
    TopRight,
    /// Bottom left.
    BottomLeft,
    /// Bottom right.
    BottomRight,
}

impl OverlayCorner {
    /// The lowercase name the agent surface uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top_left",
            Self::TopRight => "top_right",
            Self::BottomLeft => "bottom_left",
            Self::BottomRight => "bottom_right",
        }
    }

    fn align(self) -> egui::Align2 {
        match self {
            Self::TopLeft => egui::Align2::LEFT_TOP,
            Self::TopRight => egui::Align2::RIGHT_TOP,
            Self::BottomLeft => egui::Align2::LEFT_BOTTOM,
            Self::BottomRight => egui::Align2::RIGHT_BOTTOM,
        }
    }
}

/// A small panel drawn over the viewport.
#[derive(Clone)]
pub struct OverlayDesc {
    /// `<ext>.<overlay>`.
    pub id: String,
    /// Its name in the Tools menu's show/hide list.
    pub title: String,
    /// Where it sits.
    pub corner: OverlayCorner,
    /// Draws its contents.
    pub draw: PanelDraw,
}

/// The active extension tool, the overlays the user hid, and the drag in progress.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct ExtensionToolState {
    /// The active tool's id.
    pub active: Option<String>,
    /// Overlays hidden by the user.
    pub hidden_overlays: BTreeSet<String>,
    /// `(tool, handle, start position, last position)` of the drag in progress.
    drag: Option<(String, usize, Vec3, Vec3)>,
}

fn live<T: Clone>(
    world: &World,
    kind: RegistrationKind,
    map: impl Fn(&EditorExtensions) -> &std::collections::BTreeMap<String, (String, T)>,
) -> Vec<(String, T)> {
    let (Some(editor), Some(registry)) = (
        world.get_resource::<EditorExtensions>(),
        world.get_resource::<ExtensionRegistry>(),
    ) else {
        return Vec::new();
    };
    map(editor)
        .iter()
        .filter(|(name, (owner, _))| registry.owner_of(kind, name) == Some(owner.as_str()))
        .map(|(_, (owner, desc))| (owner.clone(), desc.clone()))
        .collect()
}

/// `(owner, tool)` of every live viewport tool, by id.
#[must_use]
pub fn tools(world: &World) -> Vec<(String, ViewportToolDesc)> {
    live(world, RegistrationKind::ViewportTool, |editor| &editor.tools)
}

/// `(owner, overlay)` of every live overlay, by id.
#[must_use]
pub fn overlays(world: &World) -> Vec<(String, OverlayDesc)> {
    live(world, RegistrationKind::Overlay, |editor| &editor.overlays)
}

/// The active tool, when it is still live.
#[must_use]
pub fn active_tool(world: &World) -> Option<ViewportToolDesc> {
    let active = world.get_resource::<ExtensionToolState>()?.active.clone()?;
    tools(world)
        .into_iter()
        .map(|(_, tool)| tool)
        .find(|tool| tool.id == active)
}

/// Makes `id` the active tool (`None`: no extension tool). A drag in progress ends first.
///
/// # Errors
///
/// `extension_tool_not_found`.
pub fn activate(world: &mut World, id: Option<&str>) -> Result<(), RpcError> {
    if let Some(id) = id
        && !tools(world).iter().any(|(_, tool)| tool.id == id)
    {
        return Err(RpcError::new(
            protocol::NOT_FOUND,
            "extension_tool_not_found",
            format!("no extension viewport tool `{id}`"),
            "editor.extension_tools lists the tools of the loaded extensions",
        ));
    }
    finish_drag(world);
    world
        .get_resource_or_insert_with(ExtensionToolState::default)
        .active = id.map(str::to_owned);
    Ok(())
}

/// Runs one drag step of the active tool's `handle`; returns whether the drag pushed an undo entry
/// (on `End`).
fn step(world: &mut World, tool: &ViewportToolDesc, handle: usize, phase: ToolPhase, position: Vec3) -> bool {
    let mut state = world
        .get_resource_or_insert_with(ExtensionToolState::default)
        .clone();
    let (start, last) = match (&state.drag, phase) {
        (_, ToolPhase::Begin) | (None, _) => (position, position),
        (Some((_, _, start, last)), _) => (*start, *last),
    };
    state.drag = (phase != ToolPhase::End).then(|| (tool.id.clone(), handle, start, position));
    world.insert_resource(state);
    let drag = ToolDrag {
        handle,
        phase,
        position,
        delta: position - last,
        total: position - start,
    };
    let run = Arc::clone(&tool.drag);
    let command = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(world, &drag)))
        .unwrap_or_else(|_| {
            tracing::warn!(target: "regia_editor", "extension viewport tool {} panicked", tool.id);
            None
        });
    match command {
        Some(command) if phase == ToolPhase::End => {
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .push(command);
            true
        }
        _ => false,
    }
}

/// Ends a drag whose tool went away or changed, as an `End` at its last position, so what it
/// previewed stays one undo step.
fn finish_drag(world: &mut World) {
    let Some((id, handle, _, last)) = world
        .get_resource::<ExtensionToolState>()
        .and_then(|state| state.drag.clone())
    else {
        return;
    };
    match tools(world)
        .into_iter()
        .map(|(_, tool)| tool)
        .find(|tool| tool.id == id)
    {
        Some(tool) => {
            step(world, &tool, handle, ToolPhase::End, last);
        }
        None => {
            if let Some(mut state) = world.get_resource_mut::<ExtensionToolState>() {
                state.drag = None;
            }
        }
    }
}

/// Draws the active tool's handles over `rect` (viewport pane `pane`) and runs their drags.
/// Returns whether a handle owns the pointer this frame (hovered or dragged), so the pane does
/// not also pick or box-select.
pub fn viewport_input(
    ui: &mut egui::Ui,
    world: &mut World,
    view: &ViewCamera,
    rect: egui::Rect,
    pane: usize,
) -> bool {
    let dragging = world
        .get_resource::<ExtensionToolState>()
        .and_then(|state| state.drag.as_ref().map(|drag| drag.0.clone()));
    let Some(tool) = active_tool(world) else {
        if dragging.is_some() {
            finish_drag(world);
        }
        return false;
    };
    if dragging.is_some_and(|id| id != tool.id) {
        finish_drag(world);
    }
    let _span = tracing::info_span!(target: "regia_editor", "extension_viewport_tool", id = tool.id.as_str())
        .entered();
    let shapes = (tool.handles)(world);
    if shapes.is_empty() {
        return false;
    }
    let id = ui.id().with(("extension-tool", tool.id.as_str(), pane));
    let Some(event) = crate::handles::handles(ui, view, rect, id, &shapes, &SnapSet::empty()) else {
        return false;
    };
    let phase = match event.phase {
        DragPhase::Hover => return true,
        DragPhase::Begin => ToolPhase::Begin,
        DragPhase::Drag => ToolPhase::Drag,
        DragPhase::End => ToolPhase::End,
    };
    step(world, &tool, event.handle, phase, event.position);
    true
}

/// The Tools menu entries: one toggle per live tool (`icon name`; selecting the active one turns
/// it off) and one show/hide toggle per overlay. Focusable widgets, so Tab and Enter work.
/// Returns whether an entry changed this frame.
pub fn tool_buttons(ui: &mut egui::Ui, world: &mut World) -> bool {
    let list = tools(world);
    let shown = overlays(world);
    if list.is_empty() && shown.is_empty() {
        return false;
    }
    let state = world
        .get_resource::<ExtensionToolState>()
        .cloned()
        .unwrap_or_default();
    let mut changed = false;
    ui.separator();
    for (_, tool) in &list {
        let active = state.active.as_deref() == Some(tool.id.as_str());
        let text = if tool.icon.is_empty() {
            tool.title.clone()
        } else {
            format!("{} {}", tool.icon, tool.title)
        };
        if ui
            .selectable_label(active, text)
            .on_hover_text(format!("{} (extension tool {})", tool.title, tool.id))
            .clicked()
        {
            let next = if active { None } else { Some(tool.id.as_str()) };
            if activate(world, next).is_ok() {
                changed = true;
            }
        }
    }
    for (_, overlay) in &shown {
        let mut on = !state.hidden_overlays.contains(&overlay.id);
        if ui.checkbox(&mut on, overlay.title.as_str()).changed() {
            set_overlay_shown(world, &overlay.id, on);
            changed = true;
        }
    }
    changed
}

fn set_overlay_shown(world: &mut World, id: &str, shown: bool) {
    let mut state = world.get_resource_or_insert_with(ExtensionToolState::default);
    if shown {
        state.hidden_overlays.remove(id);
    } else {
        state.hidden_overlays.insert(id.to_owned());
    }
}

/// Draws the shown overlays over `rect` (viewport pane `pane`), each in its corner.
pub fn draw_overlays(ui: &mut egui::Ui, world: &mut World, rect: egui::Rect, pane: usize) {
    let list = overlays(world);
    if list.is_empty() {
        return;
    }
    let _span = tracing::info_span!(target: "regia_editor", "extension_overlays").entered();
    let hidden = world
        .get_resource::<ExtensionToolState>()
        .map(|state| state.hidden_overlays.clone())
        .unwrap_or_default();
    let inner = rect.shrink(8.0);
    for (_, overlay) in list {
        if hidden.contains(&overlay.id) {
            continue;
        }
        let align = overlay.corner.align();
        let anchor = align.pos_in_rect(&inner);
        let draw = Arc::clone(&overlay.draw);
        egui::Area::new(ui.id().with(("extension-overlay", overlay.id.as_str(), pane)))
            .order(egui::Order::Middle)
            .pivot(align)
            .fixed_pos(anchor)
            .constrain_to(rect)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| draw(ui, world));
            });
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn tools_json(world: &World) -> Value {
    let state = world
        .get_resource::<ExtensionToolState>()
        .cloned()
        .unwrap_or_default();
    let tools: Vec<Value> = tools(world)
        .into_iter()
        .map(|(owner, tool)| {
            json!({
                "id": tool.id,
                "title": tool.title,
                "icon": tool.icon,
                "owner": owner,
                "active": state.active.as_deref() == Some(tool.id.as_str()),
            })
        })
        .collect();
    let overlays: Vec<Value> = overlays(world)
        .into_iter()
        .map(|(owner, overlay)| {
            json!({
                "id": overlay.id,
                "title": overlay.title,
                "corner": overlay.corner.name(),
                "owner": owner,
                "shown": !state.hidden_overlays.contains(&overlay.id),
            })
        })
        .collect();
    let active = active_tool(world);
    let handles: Option<Vec<[f32; 3]>> = active.as_ref().map(|tool| {
        (tool.handles)(world)
            .iter()
            .map(|h| h.anchor().to_array())
            .collect()
    });
    json!({
        "tools": tools,
        "overlays": overlays,
        "active": active.map(|tool| tool.id),
        "handles": handles,
    })
}

fn refused(code: i64, reason: &str, message: String) -> RpcError {
    RpcError::new(
        code,
        reason,
        message,
        "editor.extension_tools lists the tools, the active one's handles and the overlays",
    )
}

fn extension_tools(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(activate_param) = params.get("activate") {
        let id = match activate_param {
            Value::Null => None,
            Value::String(id) if id.is_empty() => None,
            Value::String(id) => Some(id.clone()),
            _ => return Err(RpcError::invalid_params("`activate` is a tool id, \"\" or null")),
        };
        activate(world, id.as_deref())?;
    }
    if let Some(overlay) = params.get("overlay").and_then(Value::as_str) {
        if !overlays(world).iter().any(|(_, o)| o.id == overlay) {
            return Err(refused(
                protocol::NOT_FOUND,
                "extension_overlay_not_found",
                format!("no extension overlay `{overlay}`"),
            ));
        }
        if let Some(shown) = params.get("shown").and_then(Value::as_bool) {
            set_overlay_shown(world, overlay, shown);
        }
    }
    let mut undo_entry = None;
    if let Some(drag) = params.get("drag") {
        let Some(tool) = active_tool(world) else {
            return Err(refused(
                protocol::INVALID_PARAMS,
                "extension_tool_inactive",
                "no extension viewport tool is active".to_owned(),
            ));
        };
        let handle = drag
            .get("handle")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid_params("`drag.handle` (an index) is required"))?
            as usize;
        let to = crate::agent_ext::vec3_param(drag, "to")?
            .ok_or_else(|| RpcError::invalid_params("`drag.to` ([x, y, z]) is required"))?;
        let steps = drag
            .get("steps")
            .and_then(Value::as_u64)
            .map_or(4, |s| s.clamp(1, 240)) as u32;
        let shapes = (tool.handles)(world);
        let Some(start) = shapes.get(handle).map(Handle::anchor) else {
            return Err(refused(
                protocol::INVALID_PARAMS,
                "extension_tool_handle_unknown",
                format!("`{}` has {} handles, not {}", tool.id, shapes.len(), handle + 1),
            ));
        };
        step(world, &tool, handle, ToolPhase::Begin, start);
        for n in 1..=steps {
            step(
                world,
                &tool,
                handle,
                ToolPhase::Drag,
                start.lerp(to, n as f32 / steps as f32),
            );
        }
        undo_entry = Some(step(world, &tool, handle, ToolPhase::End, to));
    }
    let mut out = tools_json(world);
    if let Some(pushed) = undo_entry {
        out["undo_entry"] = json!(pushed);
    }
    if let Some(id) = params.get("overlay_text").and_then(Value::as_str) {
        let Some((_, overlay)) = overlays(world).into_iter().find(|(_, o)| o.id == id) else {
            return Err(refused(
                protocol::NOT_FOUND,
                "extension_overlay_not_found",
                format!("no extension overlay `{id}`"),
            ));
        };
        out["text"] = json!(super::actions::offscreen_text(world, &overlay.draw));
    }
    Ok(out)
}

/// The agent method of extension viewport tools and overlays.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.extension_tools",
            tool: "editor_extension_tools",
            description: "Lists the viewport tools (id, title, icon, owner, active) and overlays (id, title, corner, owner, shown) of loaded extensions with the active tool's handle positions; activates a tool as its Tools menu entry does, shows or hides an overlay, drags a handle of the active tool in steps exactly as the pointer does (one undo step per drag), or draws an overlay off screen and answers its text. Errors: extension_tool_not_found, extension_tool_inactive, extension_tool_handle_unknown, extension_overlay_not_found.",
            params: &[
                ParamSchema {
                    name: "activate",
                    kind: "string",
                    required: false,
                    description: "A tool id to make active; \"\" or null for none.",
                },
                ParamSchema {
                    name: "overlay",
                    kind: "string",
                    required: false,
                    description: "An overlay id, with `shown`.",
                },
                ParamSchema {
                    name: "shown",
                    kind: "boolean",
                    required: false,
                    description: "Show or hide `overlay`.",
                },
                ParamSchema {
                    name: "drag",
                    kind: "object",
                    required: false,
                    description: "{ handle, to: [x, y, z], steps? (default 4) }: drag a handle of the active tool.",
                },
                ParamSchema {
                    name: "overlay_text",
                    kind: "string",
                    required: false,
                    description: "An overlay id: answer the text it draws, in paint order.",
                },
            ],
            result: "{ tools: [{ id, title, icon, owner, active }], overlays: [{ id, title, corner, owner, shown }], active, handles: [[x, y, z]] | null, undo_entry?, text? }",
            mutating: true,
            permission: Permission::EditScene,
        },
        extension_tools as Handler,
    )]
}
