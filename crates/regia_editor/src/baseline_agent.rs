// SPDX-License-Identifier: MPL-2.0
//! Agent methods of the editor baseline (ADR-2321, `book/agents/editor-preferences.md`):
//! preferences (`editor.prefs_*`), the keymap (`editor.keymap_*`) and the dock layout
//! (`editor.layout_*`). They edit the same resources the Preferences panel and the shell use; the
//! shell writes the per-user files when they change and applies layout requests on its next frame.

use std::path::PathBuf;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command_palette::{COMMANDS, CommandKeymap, KeyChord};
use crate::editor_keymap::{
    ImportMode, KeymapStore, ShortcutScope, conflicts, import_keymap, keymap_to_text,
};
use crate::editor_layout::{LayoutDoc, LayoutRequest, LayoutRequests, LayoutSnapshot};
use crate::editor_prefs::{EditorConfigPaths, EditorPreferences, PrefKind, PrefValue, PrefsStore};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

// ---------------------------------------------------------------------------------------------
// Preferences

fn pref_json(prefs: &EditorPreferences, def: &crate::editor_prefs::PrefDef) -> Value {
    let (min, max, choices) = match def.kind {
        PrefKind::Int { min, max } => (json!(min), json!(max), Value::Null),
        PrefKind::Float { min, max } => (json!(min), json!(max), Value::Null),
        PrefKind::Choice(c) => (Value::Null, Value::Null, json!(c)),
        PrefKind::Bool | PrefKind::Text { .. } => (Value::Null, Value::Null, Value::Null),
    };
    json!({
        "key": def.key,
        "category": def.category,
        "label": def.label,
        "description": def.description,
        "kind": def.kind.name(),
        "value": prefs.get(def.key).map(|v| v.to_json()),
        "default": def.default.value().to_json(),
        "is_default": prefs.is_default(def.key),
        "changed": !prefs.is_default(def.key),
        "min": min,
        "max": max,
        "choices": choices,
    })
}

fn prefs_reply(world: &World, query: &str) -> Value {
    let prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let store = world.get_resource::<PrefsStore>().cloned().unwrap_or_default();
    let path = world
        .get_resource::<EditorConfigPaths>()
        .and_then(EditorConfigPaths::prefs_file);
    let rows: Vec<Value> = EditorPreferences::search(query)
        .into_iter()
        .map(|def| pref_json(&prefs, def))
        .collect();
    json!({
        "query": query,
        "count": rows.len(),
        "prefs": rows,
        "path": path.as_ref().map(|p| p.display().to_string()),
        "format_version": crate::editor_prefs::EDITOR_PREFS_FORMAT_VERSION,
        "generation": prefs.generation(),
        "saved": path.is_some() && store.saved_generation == prefs.generation() && store.read_only.is_none(),
        "read_only": store.read_only,
        "diagnostics": store.diagnostics,
        "unknown_keys": prefs.unknown_keys(),
    })
}

fn query_param(params: &Value) -> Result<&str, RpcError> {
    match params.get("query") {
        None | Some(Value::Null) => Ok(""),
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(bad(
            "bad_query",
            "`query` must be a string",
            "words of a label, key or description",
        )),
    }
}

/// `editor.prefs_get { query? }`.
fn prefs_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = query_param(params)?;
    Ok(prefs_reply(world, query))
}

/// `editor.prefs_set { values }`: all or nothing.
fn prefs_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let values = params.get("values").and_then(Value::as_object).ok_or_else(|| {
        bad(
            "bad_pref_value",
            "`values` must be an object of key: value",
            "{ \"appearance.ui_scale\": 1.25 }",
        )
    })?;
    let mut prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let mut changed = Vec::new();
    for (key, value) in values {
        let value = PrefValue::from_json(value).ok_or_else(|| {
            bad(
                "bad_pref_value",
                format!("{key}: not a bool, number or string"),
                "",
            )
        })?;
        if crate::editor_prefs::OWNER_ONLY_PREFS.contains(&key.as_str()) {
            return Err(bad(
                "pref_owner_only",
                format!("{key} names a program the editor runs; only the owner sets it"),
                "Edit ▸ Preferences…, or the owner's editor_prefs.ron",
            ));
        }
        match prefs.set(key, &value) {
            Ok(true) => changed.push(key.clone()),
            Ok(false) => {}
            Err(err) => {
                return Err(bad(
                    err.code(),
                    err.to_string(),
                    "list them with editor.prefs_get",
                ));
            }
        }
    }
    world.insert_resource(prefs);
    let mut out = prefs_reply(world, "");
    out["changed"] = json!(changed);
    Ok(out)
}

/// `editor.prefs_reset { keys? }`.
fn prefs_reset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    if let Some(category) = params.get("category").filter(|v| !v.is_null()) {
        let name = category.as_str().unwrap_or_default();
        let reset = prefs.reset_category(name).map_err(|e| {
            bad(
                e.code(),
                e.to_string(),
                "categories as editor.prefs_get lists them",
            )
        })?;
        world.insert_resource(prefs);
        let mut out = prefs_reply(world, "");
        out["reset"] = json!(reset);
        out["category"] = json!(name);
        return Ok(out);
    }
    let reset: Vec<String> = match params.get("keys") {
        None | Some(Value::Null) => {
            let set: Vec<String> = crate::editor_prefs::PREFS
                .iter()
                .filter(|d| !prefs.is_default(d.key))
                .map(|d| d.key.to_owned())
                .collect();
            prefs.reset_all();
            set
        }
        Some(Value::Array(keys)) => {
            let mut out = Vec::new();
            for key in keys {
                let key = key
                    .as_str()
                    .ok_or_else(|| bad("unknown_pref", "`keys` must be strings", ""))?;
                if crate::editor_prefs::find_pref(key).is_none() {
                    return Err(bad(
                        "unknown_pref",
                        format!("no preference is called {key:?}"),
                        "",
                    ));
                }
            }
            for key in keys.iter().filter_map(Value::as_str) {
                if prefs.reset(key).unwrap_or(false) {
                    out.push(key.to_owned());
                }
            }
            out
        }
        Some(_) => return Err(bad("unknown_pref", "`keys` must be an array of keys", "")),
    };
    world.insert_resource(prefs);
    let mut out = prefs_reply(world, "");
    out["reset"] = json!(reset);
    Ok(out)
}

/// `editor.prefs_panel { tab?, query? }`: opens the Preferences tab and sets its tab and search.
fn prefs_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut state = world
        .get_resource::<crate::prefs_panel::PrefsPanelState>()
        .cloned()
        .unwrap_or_default();
    match params.get("tab") {
        None | Some(Value::Null) => {}
        Some(Value::String(t)) if state.set_tab(t) => {}
        Some(_) => {
            return Err(bad(
                "bad_tab",
                "`tab` is \"settings\", \"keymap\", \"theme\" or \"project\"",
                "",
            ));
        }
    }
    if params.get("query").is_some() {
        query_param(params)?.clone_into(&mut state.query);
    }
    // The shared search box finds Project Settings keys too (ADR-7767).
    let project_matches: Vec<String> = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .and_then(|root| crate::project_settings_ext::search_project_settings(&root, &state.query).ok())
        .map(|rows| rows.into_iter().map(|r| r.key).collect())
        .unwrap_or_default();
    let reply = json!({
        "tab": state.tab_name(),
        "query": state.query,
        "matches": EditorPreferences::search(&state.query).iter().map(|d| d.key).collect::<Vec<_>>(),
        "project_matches": project_matches,
    });
    world.insert_resource(state);
    LayoutRequests::push(world, LayoutRequest::Open(crate::shell::Panel::Preferences));
    Ok(reply)
}

// ---------------------------------------------------------------------------------------------
// Keymap

fn keymap_reply(world: &World) -> Value {
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let store = world.get_resource::<KeymapStore>().cloned().unwrap_or_default();
    let path = world
        .get_resource::<EditorConfigPaths>()
        .and_then(EditorConfigPaths::keymap_file);
    let commands: Vec<Value> = COMMANDS
        .iter()
        .map(|def| {
            json!({
                "id": def.id,
                "category": def.category,
                "title": def.title,
                "shortcut": keymap.shortcut_of(def),
                "default_shortcut": def.shortcut,
                "user": keymap.bindings.get(def.id).map(KeyChord::label),
                "scope": if keymap.bindings.contains_key(def.id) {
                    keymap.scope_of(def.id).name()
                } else {
                    crate::editor_keymap::builtin_scope(def.id).name()
                },
            })
        })
        .collect();
    let found: Vec<Value> = conflicts(&keymap)
        .into_iter()
        .map(|c| json!({ "keys": c.keys, "command": c.command, "shadows": c.shadows, "scope": c.scope.name() }))
        .collect();
    json!({
        "commands": commands,
        "conflicts": found,
        "path": path.as_ref().map(|p| p.display().to_string()),
        "format_version": crate::editor_keymap::EDITOR_KEYMAP_FORMAT_VERSION,
        "saved": path.is_some()
            && store.saved == keymap.bindings
            && store.saved_scopes == keymap.scopes
            && store.read_only.is_none(),
        "read_only": store.read_only,
        "problems": store.problems,
        "scopes": ShortcutScope::ALL.iter().map(|s| s.name()).collect::<Vec<_>>(),
        "active_preset": store.active_preset,
        "unknown_commands": store.unknown.keys().collect::<Vec<_>>(),
    })
}

/// `editor.keymap_get`.
fn keymap_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(keymap_reply(world))
}

/// `editor.keymap_bind { bindings }`: all or nothing.
fn keymap_bind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bindings = params.get("bindings").and_then(Value::as_object).ok_or_else(|| {
        bad(
            "bad_binding",
            "`bindings` must be an object of command id: chord or null",
            "{ \"view.toggle_grid\": \"Ctrl+G\" }",
        )
    })?;
    // `scope` (ADR-7767) applies to every binding of the call; absent is global, as before scopes.
    let scope = match params.get("scope") {
        None | Some(Value::Null) => ShortcutScope::Global,
        Some(v) => v.as_str().and_then(ShortcutScope::parse).ok_or_else(|| {
            bad(
                "bad_scope",
                "`scope` is one of global, viewport, outliner, inspector, content, graph, timeline, text",
                "global is the default",
            )
        })?,
    };
    let mut keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let mut moved = Vec::new();
    for (id, keys) in bindings {
        if crate::command_palette::find(id).is_none() {
            return Err(bad(
                "unknown_command",
                format!("no command has the id {id:?}"),
                "see editor.keymap_get",
            ));
        }
        match keys {
            Value::Null => {
                keymap.unbind(id);
            }
            Value::String(text) => {
                let chord = KeyChord::parse(text).ok_or_else(|| {
                    bad(
                        "bad_binding",
                        format!("{text:?} is not a key chord"),
                        "modifiers Ctrl (Cmd), Alt, Shift, then one key such as K, F5 or Space",
                    )
                })?;
                match keymap.bind_in(id, chord, scope) {
                    Ok(Some(from)) => moved.push(json!({ "keys": chord.label(), "from": from, "to": id })),
                    Ok(None) => {}
                    Err(crate::command_palette::BindError::SafetyKey) => {
                        return Err(bad(
                            "shortcut_safety_key",
                            format!(
                                "{} is a safety key (stop play, save, undo, redo) and stays global",
                                chord.label()
                            ),
                            "bind it with scope global, or pick another chord",
                        ));
                    }
                    Err(_) => {
                        return Err(bad(
                            "bad_binding",
                            "Ctrl+Shift+P opens the palette and cannot be rebound",
                            "pick another chord",
                        ));
                    }
                }
            }
            _ => return Err(bad("bad_binding", format!("{id}: a chord string or null"), "")),
        }
    }
    world.insert_resource(keymap);
    let mut out = keymap_reply(world);
    out["moved"] = json!(moved);
    Ok(out)
}

/// The file-name ending `editor.keymap_export` requires (`editor_keymap.ron`, `my.keymap.ron`), so
/// an export never writes over a level, a prefab or another document of the project.
pub(crate) const KEYMAP_EXPORT_SUFFIX: &str = "keymap.ron";

/// Most bytes `editor.keymap_import` reads from a file (a keymap is a few KiB).
const MAX_KEYMAP_BYTES: u64 = 1 << 20;

/// The `path` an agent names for a document the keymap and trace tools read or write: the
/// editor's one rule for request paths naming a document (ADR-7388,
/// `agent_ext::document_request_path`): relative under the project, absolute only under the
/// project or the temp directory, never `..` or a symlink leading out (`path_outside_project`).
/// An empty or missing `path` is `bad_path` (rc2 review, paths finding 1: an absolute path was
/// taken as given, so `editor.keymap_export` could write anywhere).
pub(crate) fn agent_path(world: &World, params: &Value) -> Result<PathBuf, RpcError> {
    let text = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            bad(
                "bad_path",
                "`path` is required",
                "a path relative to the project, or an absolute one under the project or the temp directory",
            )
        })?;
    crate::agent_ext::document_request_path(world, "path", text)
}

/// `editor.keymap_import { path | text, mode?, dry_run? }`.
fn keymap_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = match params.get("text").and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        None => {
            let path = agent_path(world, params)?;
            let bytes = regia_agent::req_path::read_file_capped(&path, MAX_KEYMAP_BYTES)
                .map_err(|e| bad("keymap_io", format!("cannot read {}: {e}", path.display()), ""))?;
            String::from_utf8(bytes)
                .map_err(|_| bad("keymap_io", format!("{} is not UTF-8 text", path.display()), ""))?
        }
    };
    let mode = match params.get("mode") {
        None | Some(Value::Null) => ImportMode::Merge,
        Some(v) => v.as_str().and_then(ImportMode::parse).ok_or_else(|| {
            bad(
                "bad_mode",
                "`mode` is \"merge\" or \"replace\"",
                "merge is the default",
            )
        })?,
    };
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let current = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let report = import_keymap(&current, &text, mode).map_err(|e| bad(e.code(), e.to_string(), ""))?;
    let summary = json!({
        "applied": report.applied.iter().map(|(id, keys)| json!({ "id": id, "keys": keys })).collect::<Vec<_>>(),
        "moved_from": report.moved_from,
        "problems": report.problems.iter().map(|p| json!({ "kind": p.kind(), "message": p.message() })).collect::<Vec<_>>(),
        "dry_run": dry_run,
    });
    if !dry_run {
        world.insert_resource(report.keymap);
    }
    let mut out = keymap_reply(world);
    if dry_run {
        out["conflicts"] = json!(
            report
                .conflicts
                .iter()
                .map(|c| json!({ "keys": c.keys, "command": c.command, "shadows": c.shadows }))
                .collect::<Vec<_>>()
        );
    }
    out["import"] = summary;
    Ok(out)
}

/// `editor.keymap_export { path }`.
fn keymap_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = agent_path(world, params)?;
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(KEYMAP_EXPORT_SUFFIX))
    {
        return Err(bad(
            "bad_path",
            format!(
                "`path` must name a `*{KEYMAP_EXPORT_SUFFIX}` file: {}",
                path.display()
            ),
            "for example my.keymap.ron",
        ));
    }
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let unknown = world
        .get_resource::<KeymapStore>()
        .map(|s| s.unknown.clone())
        .unwrap_or_default();
    let text = keymap_to_text(&keymap, &unknown);
    crate::level_io::write_atomic(&path, text.as_bytes()).map_err(|e| bad("keymap_io", e, ""))?;
    Ok(json!({ "path": path.display().to_string(), "bindings": keymap.bindings.len() }))
}

/// `editor.keymap_reset`.
fn keymap_reset(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let n = keymap.bindings.len();
    keymap.bindings.clear();
    keymap.scopes.clear();
    keymap.recording = None;
    world.insert_resource(keymap);
    let mut out = keymap_reply(world);
    out["cleared"] = json!(n);
    Ok(out)
}

/// Most bytes `editor.prefs_import` reads from a file.
const MAX_PREFS_BYTES: u64 = 1 << 20;

/// `editor.prefs_export { path }` (ADR-7767): the preferences file another editor can import.
fn prefs_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = agent_path(world, params)?;
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(crate::editor_prefs::PREFS_EXPORT_SUFFIX))
    {
        return Err(bad(
            "bad_path",
            format!(
                "`path` must name a `*{}` file: {}",
                crate::editor_prefs::PREFS_EXPORT_SUFFIX,
                path.display()
            ),
            "for example my.prefs.ron",
        ));
    }
    let prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    crate::level_io::write_atomic(&path, prefs.to_text().as_bytes()).map_err(|e| bad("prefs_io", e, ""))?;
    let set = crate::editor_prefs::PREFS
        .iter()
        .filter(|d| !prefs.is_default(d.key))
        .count();
    Ok(json!({ "path": path.display().to_string(), "values": set }))
}

/// `editor.prefs_import { path | text, mode?, dry_run? }` (ADR-7767). Owner-only keys keep their
/// value.
fn prefs_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = match params.get("text").and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        None => {
            let path = agent_path(world, params)?;
            let bytes = regia_agent::req_path::read_file_capped(&path, MAX_PREFS_BYTES)
                .map_err(|e| bad("prefs_io", format!("cannot read {}: {e}", path.display()), ""))?;
            String::from_utf8(bytes)
                .map_err(|_| bad("prefs_io", format!("{} is not UTF-8 text", path.display()), ""))?
        }
    };
    let mode = match params.get("mode") {
        None | Some(Value::Null) => ImportMode::Merge,
        Some(v) => v.as_str().and_then(ImportMode::parse).ok_or_else(|| {
            bad(
                "bad_mode",
                "`mode` is \"merge\" or \"replace\"",
                "merge is the default",
            )
        })?,
    };
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let current = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let report = current
        .import(&text, mode, false)
        .map_err(|e| bad(e.code(), e.to_string(), ""))?;
    let summary = json!({
        "applied": report.applied,
        "changed": report.changed,
        "skipped_owner_only": report.skipped,
        "diagnostics": report.diagnostics,
        "dry_run": dry_run,
    });
    if !dry_run {
        world.insert_resource(report.prefs);
    }
    let mut out = prefs_reply(world, "");
    out["import"] = summary;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Layout

fn layout_reply(world: &World) -> Value {
    let pending = world
        .get_resource::<LayoutRequests>()
        .map_or(0, |r| r.pending.len());
    let mut reply = match world.get_resource::<LayoutSnapshot>() {
        Some(snapshot) => json!({
            "layout": serde_json::to_value(&snapshot.doc).unwrap_or(Value::Null),
            "panels": snapshot.doc.panel_ids(),
            "source": snapshot.source.as_str(),
            "dropped": snapshot.report.dropped,
            "added": snapshot.report.added,
            "pending_requests": pending,
            "format_version": crate::editor_layout::EDITOR_LAYOUT_FORMAT_VERSION,
        }),
        None => json!({
            "layout": Value::Null,
            "panels": [],
            "source": Value::Null,
            "dropped": [],
            "added": [],
            "pending_requests": pending,
            "format_version": crate::editor_layout::EDITOR_LAYOUT_FORMAT_VERSION,
        }),
    };
    let observation = world.get_resource::<crate::editor_layout::LayoutObservation>();
    reply["panel_regions"] = observation.map_or_else(|| json!({}), |o| json!(o.panel_regions));
    reply["pixels_per_point"] = observation.map_or(Value::Null, |o| json!(o.pixels_per_point));
    reply["focused_widget"] = observation.map_or(Value::Null, |o| json!(o.focused_widget));
    reply
}

/// `editor.layout_get`.
fn layout_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(layout_reply(world))
}

/// `editor.layout_save { scope? }`.
fn layout_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scope = params.get("scope").and_then(Value::as_str).unwrap_or("project");
    let path = match scope {
        "project" => world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|p| p.root.clone())
            .map(|root| crate::editor_layout::project_layout_path(&root))
            .ok_or_else(|| {
                bad(
                    "no_project",
                    "no project is open",
                    "scope \"user\" saves your default",
                )
            })?,
        "user" => world
            .get_resource::<EditorConfigPaths>()
            .and_then(EditorConfigPaths::user_layout_file)
            .ok_or_else(|| {
                bad(
                    "no_user_config",
                    "this editor keeps no per-user files",
                    "run the editor binary",
                )
            })?,
        _ => {
            return Err(bad(
                "bad_scope",
                "`scope` is \"project\" or \"user\"",
                "project is the default",
            ));
        }
    };
    let doc = world
        .get_resource::<LayoutSnapshot>()
        .map(|s| s.doc.clone())
        .ok_or_else(|| {
            bad(
                "no_layout",
                "the editor window has not drawn a layout yet",
                "call again after a frame",
            )
        })?;
    crate::editor_layout::write_layout_file(&path, &doc).map_err(|e| bad(e.code(), e.to_string(), ""))?;
    let mut out = layout_reply(world);
    out["saved_to"] = json!(path.display().to_string());
    Ok(out)
}

/// `editor.layout_apply { layout | open | reload }`.
fn layout_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut queued = Vec::new();
    if let Some(layout) = params.get("layout").filter(|v| !v.is_null()) {
        let mut doc: LayoutDoc = serde_json::from_value(layout.clone())
            .map_err(|e| bad("layout_parse", e.to_string(), "the `layout` of editor.layout_get"))?;
        doc.format_version = crate::editor_layout::EDITOR_LAYOUT_FORMAT_VERSION;
        // Refuse a layout that would not build now, not on the next frame.
        let (_, report) = doc.to_dock().map_err(|e| bad(e.code(), e.to_string(), ""))?;
        LayoutRequests::push(world, LayoutRequest::Apply(doc));
        queued.push(json!({ "apply": { "dropped": report.dropped, "added": report.added } }));
    }
    if let Some(open) = params.get("open").filter(|v| !v.is_null()) {
        let id = open.as_str().unwrap_or_default();
        let panel = crate::shell::Panel::from_id(id).ok_or_else(|| {
            bad(
                "unknown_panel",
                format!("no panel has the id {id:?}"),
                "ids such as preferences, project_settings, content",
            )
        })?;
        LayoutRequests::push(world, LayoutRequest::Open(panel));
        queued.push(json!({ "open": id }));
    }
    if params.get("reload").and_then(Value::as_bool) == Some(true) {
        LayoutRequests::push(world, LayoutRequest::Reload);
        queued.push(json!("reload"));
    }
    if queued.is_empty() {
        return Err(bad(
            "bad_layout_request",
            "nothing to do",
            "pass layout, open or reload",
        ));
    }
    let mut out = layout_reply(world);
    out["queued"] = json!(queued);
    Ok(out)
}

/// `editor.layout_reset`.
fn layout_reset(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    LayoutRequests::push(world, LayoutRequest::Reset);
    let mut out = layout_reply(world);
    out["queued"] = json!(["reset"]);
    Ok(out)
}

const QUERY: ParamSchema = ParamSchema {
    name: "query",
    kind: "string",
    required: false,
    description: "Search words over category, label, key and description; empty lists every preference.",
};

/// The baseline's agent methods, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.prefs_get",
                tool: "prefs_get",
                description: "The editor preferences (typed per-user settings: theme, UI scale, camera, viewport, autosave, layout) matching a search, with each value, default, kind and range, and where they are saved.",
                params: &[QUERY],
                result: "{ query, count, prefs: [{ key, category, label, description, kind, value, default, is_default, changed, min, max, choices }], path, format_version, generation, saved, read_only, diagnostics, unknown_keys }",
                mutating: false,
                permission: Permission::Read,
            },
            prefs_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefs_set",
                tool: "prefs_set",
                description: "Change editor preferences, all or nothing (a refused value changes none). They apply on the editor's next frame and are saved to the user's editor_prefs.ron.",
                params: &[ParamSchema {
                    name: "values",
                    kind: "object",
                    required: true,
                    description: "{ key: value }, e.g. { \"appearance.ui_scale\": 1.25, \"autosave.interval_minutes\": 0 }.",
                }],
                result: "the editor.prefs_get result plus changed",
                mutating: true,
                permission: Permission::EditScene,
            },
            prefs_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefs_reset",
                tool: "prefs_reset",
                description: "Return editor preferences to their defaults: the named keys, one category (the Preferences section, as Reset category does), or every one. unknown_pref, unknown_category.",
                params: &[
                    ParamSchema {
                        name: "keys",
                        kind: "array",
                        required: false,
                        description: "Keys to reset; omitted (with no category) resets all.",
                    },
                    ParamSchema {
                        name: "category",
                        kind: "string",
                        required: false,
                        description: "A category (Appearance, Camera, ...): reset only its settings; wins over keys.",
                    },
                ],
                result: "the editor.prefs_get result plus reset (and category)",
                mutating: true,
                permission: Permission::EditScene,
            },
            prefs_reset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefs_export",
                tool: "prefs_export",
                description: "Write the editor preferences to a file (the editor_prefs.ron shape) another editor can import, as Preferences ▸ Export does.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "A `*prefs.ron` file: relative to the project, or absolute under the project or the temp directory.",
                }],
                result: "{ path, values }",
                mutating: true,
                permission: Permission::EditScene,
            },
            prefs_export as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefs_import",
                tool: "prefs_import",
                description: "Import a preferences file by merging into or replacing the current preferences, as Preferences ▸ Import does; owner-only keys (code.editor_command) keep their value. dry_run changes nothing.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The file: relative to the project, or absolute under the project or the temp directory. Required unless text is given.",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "The file's text instead of a path.",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "\"merge\" (default) or \"replace\".",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Report without changing the preferences.",
                    },
                ],
                result: "the editor.prefs_get result plus import: { applied, changed, skipped_owner_only, diagnostics, dry_run }",
                mutating: true,
                permission: Permission::EditScene,
            },
            prefs_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefs_panel",
                tool: "prefs_panel",
                description: "Open the Settings window's tab (next frame) and set which tab it shows and its search text, as a person would; returns the preference keys and the Regia.toml keys the shared search matches.",
                params: &[
                    ParamSchema {
                        name: "tab",
                        kind: "string",
                        required: false,
                        description: "\"settings\" (Preferences), \"keymap\", \"theme\" or \"project\" (Project Settings).",
                    },
                    QUERY,
                ],
                result: "{ tab, query, matches, project_matches }",
                mutating: true,
                permission: Permission::EditScene,
            },
            prefs_panel as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_get",
                tool: "keymap_get",
                description: "Every editor command with its shortcut (the user's when bound, else the built-in one), the conflicts of the user shortcuts, and where the keymap is saved.",
                params: &[],
                result: "{ commands: [{ id, category, title, shortcut, default_shortcut, user, scope }], conflicts: [{ keys, command, shadows, scope }], path, format_version, saved, read_only, problems, unknown_commands, scopes, active_preset }",
                mutating: false,
                permission: Permission::Read,
            },
            keymap_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_bind",
                tool: "keymap_bind",
                description: "Set or clear user shortcuts, all or nothing. A chord already bound to another command in the same scope moves; Ctrl+Shift+P is reserved; a safety key (Escape, Ctrl+S, Ctrl+Z, Ctrl+Shift+Z, Ctrl+Y) bound in a non-global scope is shortcut_safety_key. Saved to the user's editor_keymap.ron.",
                params: &[
                    ParamSchema {
                        name: "bindings",
                        kind: "object",
                        required: true,
                        description: "{ command id: \"Ctrl+Shift+G\" | null } (null returns to the built-in shortcut).",
                    },
                    ParamSchema {
                        name: "scope",
                        kind: "string",
                        required: false,
                        description: "Where the bindings apply: global (default), viewport, outliner, inspector, content, graph, timeline or text. The focused panel's scope wins over global.",
                    },
                ],
                result: "the editor.keymap_get result plus moved: [{ keys, from, to }]",
                mutating: true,
                permission: Permission::EditScene,
            },
            keymap_bind as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_import",
                tool: "keymap_import",
                description: "Import a keymap file (editor_keymap.ron shape) by merging into or replacing the user shortcuts; reports what applied, what was dropped and the conflicts. dry_run changes nothing.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The file: absolute, or relative to the project. Required unless text is given.",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "The file's text instead of a path.",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "\"merge\" (default) or \"replace\".",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Report without changing the keymap.",
                    },
                ],
                result: "the editor.keymap_get result plus import: { applied, moved_from, problems: [{ kind, message }], dry_run }",
                mutating: true,
                permission: Permission::EditScene,
            },
            keymap_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_export",
                tool: "keymap_export",
                description: "Write the user shortcuts to a keymap file another editor can import.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "The file to write: absolute, or relative to the project.",
                }],
                result: "{ path, bindings }",
                mutating: true,
                permission: Permission::EditScene,
            },
            keymap_export as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_reset",
                tool: "keymap_reset",
                description: "Remove every user shortcut (each command is back to its built-in one).",
                params: &[],
                result: "the editor.keymap_get result plus cleared",
                mutating: true,
                permission: Permission::EditScene,
            },
            keymap_reset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_get",
                tool: "layout_get",
                description: "The editor's dock layout as last drawn: the tree of tab groups and splits by panel id, where it came from (default, project, user, session) and what the last restore dropped or added.",
                params: &[],
                result: "{ layout: { format_version, root, known_panels } | null, panels, source, dropped, added, pending_requests, format_version, panel_regions: { panel_id: [x,y,w,h] in logical points }, pixels_per_point: number | null, focused_widget: string | null }",
                mutating: false,
                permission: Permission::Read,
            },
            layout_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_save",
                tool: "layout_save",
                description: "Save the shown dock layout into the project (.regia/editor_layout.ron) or as the user's default layout.",
                params: &[ParamSchema {
                    name: "scope",
                    kind: "string",
                    required: false,
                    description: "\"project\" (default) or \"user\".",
                }],
                result: "the editor.layout_get result plus saved_to",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_apply",
                tool: "layout_apply",
                description: "Change the dock layout on the next frame: show a layout (the shape editor.layout_get returns, validated now), bring a panel forward by id, or reload the saved layout.",
                params: &[
                    ParamSchema {
                        name: "layout",
                        kind: "object",
                        required: false,
                        description: "A layout as editor.layout_get returns it.",
                    },
                    ParamSchema {
                        name: "open",
                        kind: "string",
                        required: false,
                        description: "A panel id to bring forward (added when closed), e.g. \"preferences\".",
                    },
                    ParamSchema {
                        name: "reload",
                        kind: "boolean",
                        required: false,
                        description: "Re-read the project's (else the user's) saved layout.",
                    },
                ],
                result: "the editor.layout_get result plus queued",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_apply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_reset",
                tool: "layout_reset",
                description: "Return the dock to the built-in layout on the next frame (the saved files are not touched).",
                params: &[],
                result: "the editor.layout_get result plus queued",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_reset as Handler,
        ),
    ]
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// An absolute path outside the project and the temp directory, in a folder that does not
    /// exist (so an unconfined write or read fails instead of touching anything).
    fn escaping(name: &str) -> String {
        let folder = format!("regia-escape-{}", std::process::id());
        if cfg!(windows) {
            format!("C:\\{folder}\\{name}")
        } else {
            format!("/{folder}/{name}")
        }
    }

    fn world_in(project: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(project.to_path_buf()),
        });
        crate::agent_ext::install(&mut world);
        world
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason").or_else(|| d.get("code")))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    /// rc2 review (rev-zai-rc-a, paths finding 1): `editor.keymap_import`,
    /// `editor.keymap_export` and `editor.trace_timeline` took an absolute path as given. They
    /// follow the editor's document rule now, and the export writes only `*keymap.ron` files.
    #[test]
    fn keymap_and_trace_paths_stay_inside_the_project() {
        let project = regia_test_utils::unique_temp_dir("baseline-agent-paths");
        std::fs::create_dir_all(&project).expect("project");
        let mut world = world_in(&project);

        let out = escaping("x.keymap.ron");
        let err =
            call(&mut world, "editor.keymap_export", json!({ "path": out })).expect_err("export escapes");
        assert_eq!(reason(&err), "path_outside_project", "{err:?}");
        assert!(!std::path::Path::new(&out).exists());

        let err = call(
            &mut world,
            "editor.keymap_import",
            json!({ "path": escaping("x.keymap.ron") }),
        )
        .expect_err("import escapes");
        assert_eq!(reason(&err), "path_outside_project", "{err:?}");

        let err = call(
            &mut world,
            "editor.trace_timeline",
            json!({ "path": escaping("x.regiatrace") }),
        )
        .expect_err("trace escapes");
        assert_eq!(reason(&err), "path_outside_project", "{err:?}");

        for relative in ["../x.keymap.ron", "assets/../../x.keymap.ron"] {
            let err = call(&mut world, "editor.keymap_export", json!({ "path": relative }))
                .expect_err("`..` leaves the project");
            assert_eq!(reason(&err), "path_outside_project", "{relative}: {err:?}");
        }

        // Inside the project: only a `*keymap.ron` name is written.
        let level = project.join("main.level.ron");
        std::fs::write(&level, "the level").expect("level");
        let err = call(
            &mut world,
            "editor.keymap_export",
            json!({ "path": "main.level.ron" }),
        )
        .expect_err("not a keymap file");
        assert_eq!(reason(&err), "bad_path", "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&level).expect("level"),
            "the level",
            "untouched"
        );

        let done = call(
            &mut world,
            "editor.keymap_export",
            json!({ "path": "mine.keymap.ron" }),
        )
        .expect("export");
        assert_eq!(done["bindings"], 0);
        assert!(project.join("mine.keymap.ron").is_file());
        let absolute = project.join("abs.keymap.ron");
        call(
            &mut world,
            "editor.keymap_export",
            json!({ "path": absolute.display().to_string() }),
        )
        .expect("an absolute path under the project");
        call(
            &mut world,
            "editor.keymap_import",
            json!({ "path": "mine.keymap.ron", "dry_run": true }),
        )
        .expect("import from the project");

        let err = call(&mut world, "editor.keymap_export", json!({ "path": "" })).expect_err("empty");
        assert_eq!(reason(&err), "bad_path", "{err:?}");
        let _ = std::fs::remove_dir_all(&project);
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use crate::editor_layout::LayoutObservation;

    #[test]
    fn layout_observation_reports_body_scale_and_focus() {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(2.0);
        let mut world = World::new();
        let mut focused = None;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 480.0),
                )),
                ..Default::default()
            },
            |ui| {
                let ctx = ui.ctx().clone();
                let mut observation = LayoutObservation::begin(&ctx);
                egui::CentralPanel::default().show(ui, |ui| {
                    // Use a known body clip, independent of the root UI's sizing policy.
                    ui.set_clip_rect(egui::Rect::from_min_size(
                        egui::pos2(20.0, 30.0),
                        egui::vec2(100.0, 60.0),
                    ));
                    observation.record_panel("inspector", ui);
                    let button = ui.button("A real panel widget");
                    button.request_focus();
                    focused = Some(format!("{:016x}", button.id.value()));
                });
                observation.finish(&ctx);
                world.insert_resource(observation);
            },
        );
        output.textures_delta.clear();
        let reply = layout_get(&mut world, &json!({})).expect("layout read");
        assert_eq!(reply["pixels_per_point"], 2.0);
        assert_eq!(reply["focused_widget"], json!(focused));
        assert_eq!(
            reply["panel_regions"]["inspector"],
            json!([20.0, 30.0, 100.0, 60.0]),
            "body uses its clip, excluding surrounding chrome"
        );

        world.insert_resource(LayoutObservation::begin(&ctx));
        let next = layout_get(&mut world, &json!({})).expect("fresh pass");
        assert_eq!(
            next["panel_regions"],
            json!({}),
            "closed panels cannot keep stale rects"
        );
        assert_eq!(next["focused_widget"], Value::Null);
    }

    #[test]
    fn layout_observation_before_a_window_draw_has_no_fabricated_evidence() {
        let reply = layout_get(&mut World::new(), &json!({})).expect("layout read");
        assert_eq!(reply["panel_regions"], json!({}));
        assert_eq!(reply["pixels_per_point"], Value::Null);
        assert_eq!(reply["focused_widget"], Value::Null);
    }
}

#[cfg(test)]
mod observation_pixels_tests {
    use crate::editor_layout::LayoutObservation;

    struct StubPanel {
        draw: bool,
        observation: LayoutObservation,
    }

    impl egui_dock::TabViewer for StubPanel {
        type Tab = ();

        fn id(&mut self, _tab: &mut ()) -> egui::Id {
            egui::Id::new("smoke-stub-panel")
        }

        fn title(&mut self, _tab: &mut ()) -> egui::WidgetText {
            "Visible tab chrome".into()
        }

        fn ui(&mut self, ui: &mut egui::Ui, _tab: &mut ()) {
            self.observation.record_panel("stub", ui);
            if self.draw {
                ui.label("Real panel content");
            }
        }
    }

    #[test]
    fn a_blank_dock_body_is_uniform_despite_visible_chrome() {
        let device = regia_render::RenderDevice::headless().expect("lavapipe device");
        for scale in [1.0, 2.0] {
            for draw in [false, true] {
                let mut dock = egui_dock::DockState::new(vec![()]);
                let mut viewer = StubPanel {
                    draw,
                    observation: LayoutObservation::default(),
                };
                let image = crate::ui_capture::render_ui_offscreen(&device, [320, 240], scale, 3, |ui| {
                    viewer.observation = LayoutObservation::begin(ui.ctx());
                    egui_dock::DockArea::new(&mut dock).show_inside(ui, &mut viewer);
                })
                .expect("render stub panel");
                let [x, y, w, h] = viewer.observation.panel_regions["stub"];
                let left = (x * scale).ceil() as usize;
                let top = (y * scale).ceil() as usize;
                let right = ((x + w) * scale).floor() as usize;
                let bottom = ((y + h) * scale).floor() as usize;
                assert!(left < right && right <= image.size[0]);
                assert!(top < bottom && bottom <= image.size[1]);
                let first = image.pixels[top * image.size[0] + left].to_array();
                let uniform = (top..bottom).all(|row| {
                    (left..right).all(|col| {
                        image.pixels[row * image.size[0] + col]
                            .to_array()
                            .iter()
                            .zip(first)
                            .all(|(a, b)| a.abs_diff(b) <= 2)
                    })
                });
                assert_eq!(
                    uniform,
                    !draw,
                    "draw={draw}, scale={scale}, rect={:?}",
                    [x, y, w, h]
                );
                assert!(
                    image.pixels.iter().any(|p| p.to_array() != first),
                    "tab chrome is visible"
                );
            }
        }
    }
}
