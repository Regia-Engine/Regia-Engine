// SPDX-License-Identifier: MPL-2.0
//! The Tools agent surface (`book/agents/editor-tools.md`, ADR-1523).
//!
//! Agent-side methods list, launch, stop, call and drive tools; tool-side methods (called by a
//! launched tool with its scoped token) take work, answer calls, report progress and set the
//! panel view. Both act on the [`ToolHost`] resource the Tools panel draws, so a person and an
//! agent see one state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_agent::extension::{AgentDeferredHandler, DeferredReply};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_agent::scope::{ToolScope, current_caller};
use regia_agent::{AgentEventLog, AgentPermissions};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::host::{
    DEFAULT_CALL_TIMEOUT, LOG_LINES, MAX_CALL_TIMEOUT, MAX_POLL_WAIT, MAX_TEXT_BYTES, MAX_VIEW_ITEMS,
    TOOL_CALLER_REQUIRED, TOOL_ERROR, TaskState, ToolError, ToolHost, ToolTask, ViewItem, bad_params,
};
use super::manifest::{MAX_NAME_BYTES, discovery_dirs};
use crate::viewport_agent::Handler;

/// The `events.read` kind of a tool's progress records.
pub const PROGRESS_EVENT: &str = "tool_progress";

const TOOL: ParamSchema = ParamSchema {
    name: "tool",
    kind: "string",
    required: true,
    description: "The tool's manifest id (editor.tool_list).",
};

fn project_root(world: &World) -> Option<std::path::PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
}

fn host(world: &mut World) -> Mut<'_, ToolHost> {
    world.get_resource_or_insert_with(ToolHost::default)
}

fn rpc(error: ToolError) -> RpcError {
    error.rpc()
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (a string) is required")))
}

/// `timeout_ms` as a duration in `0..=max` (`default` when absent).
fn timeout_param(params: &Value, default: Duration, max: Duration) -> Result<Duration, RpcError> {
    match params.get("timeout_ms") {
        None => Ok(default),
        Some(value) => {
            let ms = value.as_u64().ok_or_else(|| {
                RpcError::invalid_params("`timeout_ms` must be a whole number of milliseconds")
            })?;
            let wait = Duration::from_millis(ms);
            if wait > max {
                return Err(RpcError::invalid_params(format!(
                    "`timeout_ms` is at most {}",
                    max.as_millis()
                )));
            }
            Ok(wait)
        }
    }
}

/// The scoped caller, required by the tool-side methods.
fn caller(world: &World, method: &str) -> Result<Arc<ToolScope>, RpcError> {
    current_caller(world).ok_or_else(|| {
        rpc(ToolError::new(
            TOOL_CALLER_REQUIRED,
            format!("{method} is called by a launched tool with its own token"),
        ))
    })
}

/// Launches `id` from the world: starts the scoped listener once (inserting its inbox, with the
/// editor's event-loop waker so a tool request draws a frame), then the tool.
///
/// # Errors
///
/// The launch's [`ToolError`].
pub fn launch_tool(world: &mut World, id: &str) -> Result<(), ToolError> {
    let project = project_root(world);
    let wake = world
        .get_resource::<crate::spacemouse::SpaceMouseWaker>()
        .map(|waker| Arc::clone(&waker.0));
    let inbox = {
        let mut host = host(world);
        host.ensure_discovered(project.as_deref());
        host.ensure_listener(wake)?
    };
    if let Some(inbox) = inbox {
        world.insert_resource(inbox);
    }
    host(world).launch(id, project.as_deref())
}

/// `Last` (exclusive): re-reads the manifests when the project changes, autostarts tools that
/// ask for it (manifests outside the project, with a project open), and ticks the host.
pub fn tick_tools(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_editor", "tick_tools").entered();
    let project = project_root(world);
    let autostart = {
        let Some(mut host) = world.get_resource_mut::<ToolHost>() else {
            return;
        };
        host.ensure_discovered(project.as_deref());
        host.tick(Instant::now());
        host.take_autostart(project.as_deref())
    };
    for id in autostart {
        if let Err(error) = launch_tool(world, &id) {
            tracing::warn!(target: "regia_editor", "tool {id} did not autostart: {}", error.message);
        }
    }
}

fn tool_json(host: &ToolHost, tool: &super::manifest::DiscoveredTool) -> Value {
    let id = tool.id();
    let status = host.status(&id);
    let (name, version, description, provides, panel, autostart) = match &tool.manifest {
        Ok(m) => (
            m.name.clone(),
            m.version.clone(),
            m.description.clone(),
            m.provides.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            json!({ "title": m.title(), "url": m.panel.url }),
            tool.autostarts(),
        ),
        Err(_) => (
            id.clone(),
            String::new(),
            String::new(),
            Vec::new(),
            Value::Null,
            false,
        ),
    };
    json!({
        "id": id,
        "name": name,
        "version": version,
        "description": description,
        "source": tool.source.name(),
        "path": tool.path.display().to_string(),
        "valid": tool.manifest.is_ok(),
        "error": tool.manifest.as_ref().err(),
        "status": status.name(),
        "provides": provides,
        "panel": panel,
        "autostart": autostart,
    })
}

/// `editor.tool_list { refresh }`.
fn tool_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let project = project_root(world);
    let refresh = params.get("refresh").and_then(Value::as_bool).unwrap_or(false);
    let mut host = host(world);
    if refresh {
        host.refresh(project.as_deref());
    } else {
        host.ensure_discovered(project.as_deref());
    }
    let tools: Vec<Value> = host
        .discovered()
        .iter()
        .map(|tool| tool_json(&host, tool))
        .collect();
    let dirs: Vec<Value> = discovery_dirs(project.as_deref())
        .iter()
        .map(|(dir, source)| json!({ "dir": dir.display().to_string(), "source": source.name() }))
        .collect();
    Ok(json!({ "tools": tools, "dirs": dirs }))
}

/// `editor.tool_status { tool, log_lines }`.
fn tool_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = str_param(params, "tool")?;
    let lines = params
        .get("log_lines")
        .and_then(Value::as_u64)
        .map_or(50, |n| usize::try_from(n).unwrap_or(LOG_LINES).min(LOG_LINES));
    let project = project_root(world);
    let mut host = host(world);
    host.ensure_discovered(project.as_deref());
    if host.find(id).is_none() && host.runtime(id).is_none() {
        return Err(rpc(ToolError::new(
            super::host::TOOL_UNKNOWN,
            format!("no tool manifest has id `{id}`"),
        )));
    }
    let status = host.status(id);
    let exit_code = match &status {
        super::host::ToolStatus::Exited(code) => json!(code),
        _ => Value::Null,
    };
    let failure = match &status {
        super::host::ToolStatus::Failed(message) => json!(message),
        _ => Value::Null,
    };
    let runtime = host.runtime(id);
    Ok(json!({
        "tool": id,
        "status": status.name(),
        "exit_code": exit_code,
        "failure": failure,
        // A tool that exited (or never ran) is not connected, whatever it asked before
        // (Red-Rouge v0.2.6 and v0.2.8: `exited` read `connected: true`, ADR-1965).
        "connected": status.is_live() && runtime.is_some_and(|r| r.requests() > 0),
        "requests": runtime.map_or(0, super::host::ToolRuntime::requests),
        "launches": runtime.map_or(0, super::host::ToolRuntime::launches),
        "calls_in_flight": runtime.map_or(0, super::host::ToolRuntime::calls_in_flight),
        "queued": runtime.map_or(0, super::host::ToolRuntime::queued),
        "tasks": runtime.map_or_else(Vec::new, |r| r.tasks().iter().map(ToolTask::to_json).collect()),
        "view": runtime.map_or_else(Vec::new, |r| r.view().iter().map(ViewItem::to_json).collect()),
        "log": runtime.map_or_else(Vec::new, |r| r.log_tail(lines)),
        "selected": host.selected.as_deref() == Some(id),
    }))
}

/// `editor.tool_methods { tool }`.
fn tool_methods(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted = params.get("tool").and_then(Value::as_str);
    let project = project_root(world);
    let mut host = host(world);
    host.ensure_discovered(project.as_deref());
    if let Some(id) = wanted {
        host.manifest(id).map_err(rpc)?;
    }
    let methods: Vec<Value> = host
        .discovered()
        .iter()
        .filter_map(|tool| tool.manifest.as_ref().ok())
        .filter(|m| wanted.is_none_or(|id| m.id == id))
        .flat_map(|m| m.provides.iter().map(|p| p.to_json(&m.id)))
        .collect();
    Ok(json!({ "methods": methods }))
}

/// `editor.tool_launch { tool }`.
fn tool_launch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = str_param(params, "tool")?;
    launch_tool(world, id).map_err(rpc)?;
    let host = host(world);
    Ok(json!({
        "tool": id,
        "status": host.status(id).name(),
        "endpoint": host.endpoint().map(|addr| format!("ws://{addr}")),
    }))
}

/// `editor.tool_stop { tool, force }`.
fn tool_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = str_param(params, "tool")?;
    let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
    let mut host = host(world);
    host.stop(id, force, Instant::now()).map_err(rpc)?;
    Ok(json!({ "tool": id, "status": host.status(id).name(), "force": force }))
}

/// `editor.tool_click { tool, item }`.
fn tool_click(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = str_param(params, "tool")?;
    let item = str_param(params, "item")?;
    host(world).click(id, item).map_err(rpc)?;
    Ok(json!({ "tool": id, "item": item, "queued": true }))
}

/// `editor.tool_panel { tool }`: select a tool and focus the Tools tab.
fn tool_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted = params.get("tool").and_then(Value::as_str);
    let project = project_root(world);
    let mut host = host(world);
    host.ensure_discovered(project.as_deref());
    if let Some(id) = wanted {
        if host.find(id).is_none() {
            return Err(rpc(ToolError::new(
                super::host::TOOL_UNKNOWN,
                format!("no tool manifest has id `{id}`"),
            )));
        }
        host.selected = Some(id.to_string());
    }
    host.focus_panel = true;
    Ok(json!({
        "selected": host.selected,
        "focused": true,
        "tools": host.discovered().len(),
    }))
}

/// `editor.tool_progress { task, label, fraction, state, message }` (tool side).
fn tool_progress(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scope = caller(world, "editor.tool_progress")?;
    let id = str_param(params, "task")?;
    if id.is_empty() || id.len() > super::manifest::MAX_NAME_BYTES {
        return Err(RpcError::invalid_params("`task` must be 1-64 bytes"));
    }
    let state = match params.get("state").and_then(Value::as_str) {
        None => TaskState::Running,
        Some(name) => TaskState::parse(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "`state` {name:?} must be running, done, failed or cancelled"
            ))
        })?,
    };
    let fraction = match params.get("fraction") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|f| f.is_finite())
                .ok_or_else(|| RpcError::invalid_params("`fraction` must be a finite number from 0 to 1"))?
                .clamp(0.0, 1.0) as f32,
        ),
    };
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let task = ToolTask {
        id: id.to_string(),
        label: text("label"),
        fraction,
        state,
        message: text("message"),
        seq: 0,
    };
    let stored = host(world).progress(&scope, task).map_err(rpc)?;
    let frame = world
        .get_resource::<regia_app::prelude::Time>()
        .map_or(0, |t| t.frame_count());
    let mut record = stored.to_json();
    record["tool"] = json!(scope.tool);
    if let Some(mut log) = world.get_resource_mut::<AgentEventLog>() {
        log.push_host(frame, PROGRESS_EVENT, record.clone());
    }
    Ok(record)
}

/// `editor.tool_reply { call, result | error }` (tool side).
fn tool_reply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scope = caller(world, "editor.tool_reply")?;
    let call = params
        .get("call")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`call` (the id from editor.tool_poll) is required"))?;
    let result = match (params.get("result"), params.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => Err(tool_error(&scope.tool, error)),
        _ => {
            return Err(RpcError::invalid_params(
                "pass exactly one of `result` and `error`",
            ));
        }
    };
    host(world).reply(&scope, call, result).map_err(rpc)?;
    Ok(json!({ "call": call, "delivered": true }))
}

/// The caller's error for a tool's `error` reply: `tool_error` with the tool's message, and its
/// own code as `data.tool_code`.
fn tool_error(tool: &str, error: &Value) -> RpcError {
    let message = match error {
        Value::String(message) => message.clone(),
        other => other
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the tool reported an error")
            .to_string(),
    };
    let mut rpc_error = rpc(ToolError::new(TOOL_ERROR, format!("{tool}: {message}")));
    if let Some(code) = error.get("code").and_then(Value::as_str)
        && let Some(data) = rpc_error.data.as_mut()
    {
        data["tool_code"] = json!(code);
    }
    rpc_error
}

/// `editor.tool_set_view { items }` (tool side).
fn tool_set_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scope = caller(world, "editor.tool_set_view")?;
    let items = params
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`items` (an array of view items) is required"))?;
    let parsed = items
        .iter()
        .enumerate()
        .map(|(index, item)| ViewItem::parse(item).map_err(|e| bad_params(format!("items[{index}]: {e}"))))
        .collect::<Result<Vec<_>, _>>()
        .map_err(rpc)?;
    let count = host(world).set_view(&scope, parsed).map_err(rpc)?;
    Ok(json!({ "items": count }))
}

/// `editor.tool_call { tool, method, params, timeout_ms }` (deferred): the declared permission is
/// checked against the project profile, and against the caller's scope when a tool calls.
fn tool_call(world: &mut World, params: &Value, reply: DeferredReply) -> Result<(), RpcError> {
    let id = str_param(params, "tool")?;
    let method = str_param(params, "method")?;
    let call_params = params.get("params").cloned().unwrap_or_else(|| json!({}));
    if !call_params.is_object() {
        return Err(RpcError::invalid_params("`params` must be an object"));
    }
    let timeout = timeout_param(params, DEFAULT_CALL_TIMEOUT, MAX_CALL_TIMEOUT)?;
    if timeout.is_zero() {
        return Err(RpcError::invalid_params("`timeout_ms` must be at least 1"));
    }
    let project = project_root(world);
    let declared = {
        let mut host = host(world);
        host.ensure_discovered(project.as_deref());
        let manifest = host.manifest(id).map_err(rpc)?;
        manifest
            .provided(method)
            .map(super::manifest::ProvidedMethod::permission)
    };
    if let Some(permission) = declared {
        let profile = world
            .get_resource::<AgentPermissions>()
            .cloned()
            .unwrap_or_default();
        if !profile.allows(permission) {
            return Err(profile.denied_error(permission));
        }
        if let Some(scope) = current_caller(world)
            && !scope.allows_permission(permission)
        {
            return Err(scope.denied_error(method, permission));
        }
    }
    host(world)
        .call(id, method, call_params, timeout, reply, Instant::now())
        .map_err(rpc)?;
    Ok(())
}

/// `editor.tool_poll { timeout_ms }` (tool side, deferred).
fn tool_poll(world: &mut World, params: &Value, reply: DeferredReply) -> Result<(), RpcError> {
    let scope = caller(world, "editor.tool_poll")?;
    let wait = timeout_param(params, Duration::ZERO, MAX_POLL_WAIT)?;
    host(world).poll(&scope, wait, reply, Instant::now()).map_err(rpc)
}

const fn schema(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
    mutating: bool,
    permission: Permission,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating,
        permission,
    }
}

/// The synchronous Tools methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            schema(
                "editor.tool_list",
                "tool_list",
                "Editor tools: every manifest found (valid or not, with its error), its status and the directories searched.",
                &[ParamSchema {
                    name: "refresh",
                    kind: "boolean",
                    required: false,
                    description: "Re-read the manifest directories first.",
                }],
                "{ tools: [{id, name, version, description, source, path, valid, error, status, provides, panel, autostart}], dirs: [{dir, source}] }",
                false,
                Permission::Read,
            ),
            tool_list as Handler,
        ),
        (
            schema(
                "editor.tool_status",
                "tool_status",
                "One editor tool's status, tasks, panel view, log tail and calls in flight.",
                &[
                    TOOL,
                    ParamSchema {
                        name: "log_lines",
                        kind: "integer",
                        required: false,
                        description: "Log lines to return (default 50, at most 200).",
                    },
                ],
                "{ tool, status, exit_code, failure, connected, requests, launches, calls_in_flight, queued, tasks, view, log, selected }",
                false,
                Permission::Read,
            ),
            tool_status as Handler,
        ),
        (
            schema(
                "editor.tool_methods",
                "tool_methods",
                "The methods editor tools provide (call them with editor.tool_call).",
                &[ParamSchema {
                    name: "tool",
                    kind: "string",
                    required: false,
                    description: "Only this tool's methods.",
                }],
                "{ methods: [{tool, name, description, permission, params}] }",
                false,
                Permission::Read,
            ),
            tool_methods as Handler,
        ),
        (
            schema(
                "editor.tool_launch",
                "tool_launch",
                "Launch an editor tool with a fresh scoped token (needs the tools permission).",
                &[TOOL],
                "{ tool, status, endpoint }",
                true,
                Permission::Tools,
            ),
            tool_launch as Handler,
        ),
        (
            schema(
                "editor.tool_stop",
                "tool_stop",
                "Stop an editor tool: a shutdown event, then a kill after 5 s (at once with force).",
                &[
                    TOOL,
                    ParamSchema {
                        name: "force",
                        kind: "boolean",
                        required: false,
                        description: "Kill at once.",
                    },
                ],
                "{ tool, status, force }",
                true,
                Permission::Tools,
            ),
            tool_stop as Handler,
        ),
        (
            schema(
                "editor.tool_click",
                "tool_click",
                "Press a button of an editor tool's panel view (the tool gets a click event).",
                &[
                    TOOL,
                    ParamSchema {
                        name: "item",
                        kind: "string",
                        required: true,
                        description: "The button's id (editor.tool_status view).",
                    },
                ],
                "{ tool, item, queued }",
                true,
                Permission::Tools,
            ),
            tool_click as Handler,
        ),
        (
            schema(
                "editor.tool_panel",
                "tool_panel",
                "Show the Tools panel, optionally selecting a tool.",
                &[ParamSchema {
                    name: "tool",
                    kind: "string",
                    required: false,
                    description: "The tool to select.",
                }],
                "{ selected, focused, tools }",
                false,
                Permission::Read,
            ),
            tool_panel as Handler,
        ),
        (
            schema(
                "editor.tool_progress",
                "tool_progress",
                "Tool side: report a task's progress (shown in the Tools panel, recorded as a tool_progress event). An agent token answers tool_caller_required.",
                &[
                    ParamSchema {
                        name: "task",
                        kind: "string",
                        required: true,
                        description: "The task id (1-64 bytes); a new id adds a task.",
                    },
                    ParamSchema {
                        name: "label",
                        kind: "string",
                        required: false,
                        description: "What the panel shows (default: the id).",
                    },
                    ParamSchema {
                        name: "fraction",
                        kind: "number",
                        required: false,
                        description: "0 to 1; absent or null is indeterminate.",
                    },
                    ParamSchema {
                        name: "state",
                        kind: "string",
                        required: false,
                        description: "running (default), done, failed or cancelled.",
                    },
                    ParamSchema {
                        name: "message",
                        kind: "string",
                        required: false,
                        description: "The latest status line.",
                    },
                ],
                "{ task, label, fraction, state, message, tool }",
                false,
                Permission::Read,
            ),
            tool_progress as Handler,
        ),
        (
            schema(
                "editor.tool_reply",
                "tool_reply",
                "Tool side: answer a proxied call taken from editor.tool_poll. An agent token answers tool_caller_required.",
                &[
                    ParamSchema {
                        name: "call",
                        kind: "integer",
                        required: true,
                        description: "The call id: the whole number from the `call` event of editor.tool_poll.",
                    },
                    ParamSchema {
                        name: "result",
                        kind: "any",
                        required: false,
                        description: "The result (passed to the caller unchanged).",
                    },
                    ParamSchema {
                        name: "error",
                        kind: "any",
                        required: false,
                        description: "Instead of result: a message, or { message, code }.",
                    },
                ],
                "{ call, delivered }",
                false,
                Permission::Read,
            ),
            tool_reply as Handler,
        ),
        (
            schema(
                "editor.tool_set_view",
                "tool_set_view",
                "Tool side: replace the tool's panel view (heading, label, value, progress, button, link, separator items). An agent token answers tool_caller_required.",
                &[ParamSchema {
                    name: "items",
                    kind: "array",
                    required: true,
                    description: "The whole view, replacing the old one: at most 256 items, each { kind, ... }.",
                }],
                "{ items }",
                false,
                Permission::Read,
            ),
            tool_set_view as Handler,
        ),
    ]
}

/// The deferred Tools methods (registered with `AgentExtension::register_deferred`).
#[must_use]
pub fn deferred_methods() -> Vec<(MethodSchema, AgentDeferredHandler)> {
    vec![
        (
            schema(
                "editor.tool_call",
                "tool_call",
                "Call a method an editor tool provides (editor.tool_methods); the tool's answer passes through.",
                &[
                    TOOL,
                    ParamSchema {
                        name: "method",
                        kind: "string",
                        required: true,
                        description: "The provided method's name.",
                    },
                    ParamSchema {
                        name: "params",
                        kind: "object",
                        required: false,
                        description: "Its parameters (default {}).",
                    },
                    ParamSchema {
                        name: "timeout_ms",
                        kind: "integer",
                        required: false,
                        description: "How long to wait for the tool (default 25000, at most 600000).",
                    },
                ],
                "the tool's result",
                true,
                Permission::Read,
            ),
            tool_call as AgentDeferredHandler,
        ),
        (
            schema(
                "editor.tool_poll",
                "tool_poll",
                "Tool side: take queued work (calls, clicks, shutdown); waits up to timeout_ms when there is none. An agent token answers tool_caller_required.",
                &[ParamSchema {
                    name: "timeout_ms",
                    kind: "integer",
                    required: false,
                    description: "How long to wait for work (default 0, at most 60000).",
                }],
                "{ events: [{kind: call|click|shutdown, ...}] }",
                false,
                Permission::Read,
            ),
            tool_poll as AgentDeferredHandler,
        ),
    ]
}

/// The JSON Schema of one view item, one alternative per kind (the shapes [`ViewItem::parse`]
/// reads and `ViewItem::to_json` writes).
#[must_use]
pub fn view_item_shape() -> Value {
    let text = |description: &str| json!({ "type": "string", "maxLength": MAX_TEXT_BYTES, "description": description });
    let kind = |name: &str| json!({ "const": name });
    let item = |name: &str, properties: Value, required: &[&str]| {
        let mut properties = properties;
        properties["kind"] = kind(name);
        let mut required = required.to_vec();
        required.insert(0, "kind");
        json!({ "type": "object", "properties": properties, "required": required })
    };
    json!({
        "oneOf": [
            item("heading", json!({ "text": text("The heading.") }), &["text"]),
            item("label", json!({ "text": text("The line of text.") }), &["text"]),
            item(
                "value",
                json!({ "label": text("Shown on the left."), "value": text("Shown on the right.") }),
                &["label", "value"],
            ),
            item(
                "progress",
                json!({
                    "label": text("The text on the bar (default empty)."),
                    "fraction": { "type": "number", "minimum": 0, "maximum": 1, "description": "0 to 1; clamped." },
                }),
                &["fraction"],
            ),
            item(
                "button",
                json!({
                    "id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_NAME_BYTES,
                        "description": "The id the click event carries (1-64 bytes).",
                    },
                    "label": text("The text on the button (default: the id)."),
                }),
                &["id"],
            ),
            item(
                "link",
                json!({
                    "url": {
                        "type": "string",
                        "pattern": "^https?://",
                        "maxLength": MAX_TEXT_BYTES,
                        "description": "An http or https URL; opens in the browser.",
                    },
                    "label": text("The link text (default: the URL)."),
                }),
                &["url"],
            ),
            item("separator", json!({}), &[]),
        ],
    })
}

/// The param shapes `agent.schema` shows for the Tools methods: `(method, param, JSON Schema)`.
/// Everything the static `ParamSchema::kind` cannot say: enums, ranges, the view items and the
/// free-form `result` and `error` of a reply.
#[must_use]
pub fn param_shapes() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "editor.tool_status",
            "log_lines",
            json!({ "type": "integer", "minimum": 0, "maximum": LOG_LINES, "default": 50 }),
        ),
        (
            "editor.tool_call",
            "timeout_ms",
            json!({
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_CALL_TIMEOUT.as_secs() * 1000,
                "default": DEFAULT_CALL_TIMEOUT.as_secs() * 1000,
            }),
        ),
        (
            "editor.tool_poll",
            "timeout_ms",
            json!({ "type": "integer", "minimum": 0, "maximum": MAX_POLL_WAIT.as_secs() * 1000, "default": 0 }),
        ),
        (
            "editor.tool_progress",
            "task",
            json!({ "type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES }),
        ),
        (
            "editor.tool_progress",
            "fraction",
            json!({ "type": ["number", "null"], "minimum": 0, "maximum": 1 }),
        ),
        (
            "editor.tool_progress",
            "state",
            json!({
                "type": "string",
                "enum": ["running", "done", "failed", "cancelled"],
                "default": "running",
            }),
        ),
        (
            "editor.tool_reply",
            "call",
            json!({ "type": "integer", "minimum": 0 }),
        ),
        // Any JSON value passes through, so the schema constrains nothing (the empty schema).
        ("editor.tool_reply", "result", json!({})),
        (
            "editor.tool_reply",
            "error",
            json!({
                "oneOf": [
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": {
                            "message": { "type": "string" },
                            "code": { "type": "string", "description": "Your own code; the caller sees it as data.tool_code." },
                        },
                    },
                ],
            }),
        ),
        (
            "editor.tool_set_view",
            "items",
            json!({ "type": "array", "maxItems": MAX_VIEW_ITEMS, "items": view_item_shape() }),
        ),
    ]
}

/// The JSON-RPC error class a tool code answers with.
#[must_use]
pub fn error_class(code: &'static str) -> i64 {
    ToolError::new(code, "").rpc().code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::host::{TOOL_ALREADY_RUNNING, TOOL_UNKNOWN, TOOL_UNKNOWN_METHOD};
    use crate::tools::manifest::ToolManifest;
    use regia_agent::protocol;
    use regia_agent::scope::AgentCaller;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject { root: None });
        world.init_resource::<AgentEventLog>();
        world.insert_resource(ToolHost::new());
        world
    }

    fn manifest() -> ToolManifest {
        ToolManifest::parse(
            "format = 1\nid = \"red-rouge\"\n[launch]\nprogram = \"p\"\n[[provides]]\nname = \"pick.list\"\n\
             [[provides]]\nname = \"queue.approve\"\npermission = \"edit_scene\"\n",
        )
        .expect("valid")
    }

    /// Attaches the test tool and makes it the caller of the next handler call.
    fn as_tool(world: &mut World) -> Arc<ToolScope> {
        let scope = world.resource_mut::<ToolHost>().attach_for_test(manifest());
        world.insert_resource(AgentCaller(Arc::clone(&scope)));
        scope
    }

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn deferred() -> (
        DeferredReply,
        crossbeam_channel::Receiver<regia_agent::protocol::Response>,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        (DeferredReply::new(json!(9), tx), rx)
    }

    #[test]
    fn every_method_is_an_editor_method_with_a_description() {
        let names: Vec<&str> = methods()
            .iter()
            .map(|(m, _)| m.name)
            .chain(deferred_methods().iter().map(|(m, _)| m.name))
            .collect();
        for name in &names {
            assert!(name.starts_with("editor.tool_"), "{name}");
        }
        for base in super::super::host::BASE_METHODS {
            if base.starts_with("editor.") {
                assert!(names.contains(base), "{base} is registered");
            }
        }
        assert!(names.contains(&"editor.tool_call"));
        assert!(names.contains(&"editor.tool_panel"));
    }

    /// The tool-side methods refuse a master-token caller.
    #[test]
    fn tool_side_methods_need_a_tool_caller() {
        let mut world = world();
        let error = tool_progress(&mut world, &json!({ "task": "t" })).expect_err("no caller");
        assert_eq!(code(&error), TOOL_CALLER_REQUIRED);
        let error = tool_set_view(&mut world, &json!({ "items": [] })).expect_err("no caller");
        assert_eq!(code(&error), TOOL_CALLER_REQUIRED);
        let error = tool_reply(&mut world, &json!({ "call": 1, "result": {} })).expect_err("no caller");
        assert_eq!(code(&error), TOOL_CALLER_REQUIRED);
        let (reply, _) = deferred();
        let error = tool_poll(&mut world, &json!({}), reply).expect_err("no caller");
        assert_eq!(code(&error), TOOL_CALLER_REQUIRED);
    }

    /// The whole loop through the handlers: an agent calls a provided method, the tool polls it,
    /// replies with an error carrying its own code, and the agent gets `tool_error`.
    #[test]
    fn a_call_round_trips_through_the_handlers() {
        let mut world = world();
        let scope = as_tool(&mut world);
        world.remove_resource::<AgentCaller>();

        let (agent_reply, agent_rx) = deferred();
        tool_call(
            &mut world,
            &json!({ "tool": "red-rouge", "method": "pick.list", "params": { "n": 2 } }),
            agent_reply,
        )
        .expect("queued");

        world.insert_resource(AgentCaller(Arc::clone(&scope)));
        let (poll_reply, poll_rx) = deferred();
        tool_poll(&mut world, &json!({}), poll_reply).expect("polled");
        let events = poll_rx.try_recv().expect("answered").result.expect("result");
        let call = events["events"][0]["call"].as_u64().expect("call id");
        assert_eq!(events["events"][0]["params"], json!({ "n": 2 }));
        tool_reply(
            &mut world,
            &json!({ "call": call, "error": { "message": "no scenes yet", "code": "empty" } }),
        )
        .expect("delivered");
        let error = agent_rx.try_recv().expect("agent answered").error.expect("error");
        assert_eq!(code(&error), TOOL_ERROR);
        assert!(error.message.contains("no scenes yet"), "{}", error.message);
        assert_eq!(error.data.expect("data")["tool_code"], "empty");

        let bad = tool_reply(&mut world, &json!({ "call": call })).expect_err("neither");
        assert_eq!(code(&bad), "F0002");
    }

    /// A provided method's declared permission is checked against the project profile, and a
    /// tool calling another tool also needs it in its own scope.
    #[test]
    fn calls_check_the_declared_permission() {
        let mut world = world();
        let scope = as_tool(&mut world);
        world.remove_resource::<AgentCaller>();
        world.insert_resource(AgentPermissions {
            config: regia_agent::AgentConfig::fail_closed(),
            source: None,
        });
        let (reply, _) = deferred();
        let error = tool_call(
            &mut world,
            &json!({ "tool": "red-rouge", "method": "queue.approve" }),
            reply,
        )
        .expect_err("read only");
        assert_eq!(code(&error), "F0012");
        world.insert_resource(AgentPermissions::default());
        world.insert_resource(AgentCaller(scope));
        let (reply, _) = deferred();
        let error = tool_call(
            &mut world,
            &json!({ "tool": "red-rouge", "method": "queue.approve" }),
            reply,
        )
        .expect_err("the calling tool's scope has no edit_scene");
        assert_eq!(code(&error), regia_agent::scope::TOOL_SCOPE_DENIED);
        let (reply, _) = deferred();
        let error = tool_call(
            &mut world,
            &json!({ "tool": "red-rouge", "method": "nope" }),
            reply,
        )
        .expect_err("undeclared");
        assert_eq!(code(&error), TOOL_UNKNOWN_METHOD);
        let (reply, _) = deferred();
        let error = tool_call(
            &mut world,
            &json!({ "tool": "red-rouge", "method": "pick.list", "timeout_ms": 700_000 }),
            reply,
        )
        .expect_err("too long");
        assert_eq!(code(&error), "F0002");
    }

    /// Progress lands in the host and in `events.read` as `tool_progress`; the view is replaced
    /// and an agent presses its button.
    #[test]
    fn progress_view_and_click() {
        let mut world = world();
        let _scope = as_tool(&mut world);
        let record = tool_progress(
            &mut world,
            &json!({ "task": "upload", "label": "Upload", "fraction": 0.5, "message": "3 of 6" }),
        )
        .expect("stored");
        assert_eq!(record["state"], "running");
        assert_eq!(record["tool"], "red-rouge");
        let events = world.resource::<AgentEventLog>().records_snapshot(8);
        let last = events.last().expect("recorded");
        assert_eq!(last.kind, PROGRESS_EVENT);
        assert_eq!(last.data["task"], "upload");
        let error =
            tool_progress(&mut world, &json!({ "task": "upload", "state": "paused" })).expect_err("bad");
        assert_eq!(code(&error), "F0002");

        let out = tool_set_view(
            &mut world,
            &json!({ "items": [
                { "kind": "heading", "text": "Review queue" },
                { "kind": "button", "id": "approve", "label": "Approve" },
            ] }),
        )
        .expect("set");
        assert_eq!(out["items"], 2);
        let error =
            tool_set_view(&mut world, &json!({ "items": [{ "kind": "marquee" }] })).expect_err("bad item");
        assert!(error.message.contains("items[0]"), "{}", error.message);

        world.remove_resource::<AgentCaller>();
        tool_click(&mut world, &json!({ "tool": "red-rouge", "item": "approve" })).expect("clicked");
        let status = tool_status(&mut world, &json!({ "tool": "red-rouge" })).expect("status");
        assert_eq!(status["status"], "running");
        assert_eq!(status["queued"], 1);
        assert_eq!(status["tasks"][0]["label"], "Upload");
        assert_eq!(status["view"][1]["id"], "approve");
        assert_eq!(status["connected"], true);
        // Red-Rouge v0.2.6 and v0.2.8: after the stop `exited` still read `connected: true`
        // (ADR-1965). Stopping keeps the token (the tool may answer its shutdown); exited does not.
        tool_stop(&mut world, &json!({ "tool": "red-rouge" })).expect("stops");
        let status = tool_status(&mut world, &json!({ "tool": "red-rouge" })).expect("status");
        assert_eq!(status["status"], "stopping");
        assert_eq!(status["connected"], true, "{status}");
        world
            .resource_mut::<ToolHost>()
            .exit_for_test("red-rouge", Some(0));
        let status = tool_status(&mut world, &json!({ "tool": "red-rouge" })).expect("status");
        assert_eq!(status["status"], "exited");
        assert_eq!(status["exit_code"], 0);
        assert_eq!(status["connected"], false, "{status}");
    }

    #[test]
    fn panel_list_and_methods() {
        let mut world = world();
        let _scope = as_tool(&mut world);
        world.remove_resource::<AgentCaller>();
        let out = tool_panel(&mut world, &json!({ "tool": "red-rouge" })).expect("focused");
        assert_eq!(out["selected"], "red-rouge");
        assert!(world.resource::<ToolHost>().focus_panel);
        let error = tool_panel(&mut world, &json!({ "tool": "ghost" })).expect_err("unknown");
        assert_eq!(code(&error), TOOL_UNKNOWN);
        let methods = tool_methods(&mut world, &json!({ "tool": "red-rouge" })).expect("methods");
        let names: Vec<&str> = methods["methods"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert_eq!(names, ["pick.list", "queue.approve"]);
        assert_eq!(methods["methods"][1]["permission"], "edit_scene");
        let error = tool_status(&mut world, &json!({ "tool": "ghost" })).expect_err("unknown");
        assert_eq!(code(&error), TOOL_UNKNOWN);
        let error = tool_launch(&mut world, &json!({ "tool": "red-rouge" })).expect_err("running");
        assert_eq!(code(&error), TOOL_ALREADY_RUNNING);
        let stopped = tool_stop(&mut world, &json!({ "tool": "red-rouge" })).expect("stops");
        assert_eq!(stopped["status"], "stopping");
    }

    #[test]
    fn error_classes_follow_the_codes() {
        assert_eq!(error_class(TOOL_UNKNOWN), protocol::NOT_FOUND);
        assert_eq!(error_class(TOOL_ERROR), protocol::INTERNAL_ERROR);
    }
}
