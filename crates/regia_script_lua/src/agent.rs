// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface (`book/agents/lua-mods.md`, `book/agents/lua-component-props.md`):
//! `lua.list`, `lua.run`, `lua.globals`, `lua.errors`, `lua.restart`, `lua.reload`, `lua.state`,
//! `lua.watch`, `lua.entity_cap`, `lua.components`, `lua.props.get`, `lua.props.set`,
//! `lua.props.list` and `lua.authority`, registered on the world's
//! [`AgentExtension`](regia_agent::extension::AgentExtension) by
//! [`LuaModsPlugin`](crate::LuaModsPlugin). Script failures come back inside the result with a
//! lowercase code; bad params use `F0002` and an unknown mod `F0003`.

use serde_json::{Value, json};

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;

use crate::ecs::EcsModWorld;
use crate::error::{LuaErrorCode, ModError};
use crate::files::ModFiles;
use crate::host::{LuaMod, LuaMods, MAX_CHUNK_BYTES, ModStatus, ReloadOutcome};
use crate::state::StateCarry;
use crate::value::ScriptValue;
use crate::world::ModWorld;

/// The capability marker `agent.hello` lists when the plugin is installed.
pub(crate) const CAPABILITY: &str = "lua_mods";
/// Most entries `lua.globals` returns.
const MAX_GLOBALS: usize = 512;
/// Most waiting host commands `lua.authority` lists.
const MAX_LISTED_COMMANDS: usize = 64;

fn authority_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.authority",
        tool: "lua_authority",
        description: "Reads the mod host's side of a networked game (host or presentation; ADR-8466), the command names regia.command accepts and its caps, the host commands waiting for the game to drain (without draining them), per-mod refusals, and the snapshot generation.",
        params: &[],
        result: "{ plugin, authority, commands, max_pending_per_mod, max_command_bytes, pending, listed: [{ mod, name, value }], snapshot_generation, mods: [{ id, authority, pending, refused, snapshot_builds }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn lua_authority(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(mods) = world.get_resource::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    let rules = mods.host_command_rules();
    let listed: Vec<Value> = mods
        .peek_host_commands(MAX_LISTED_COMMANDS)
        .iter()
        .map(|c| json!({ "mod": c.mod_id, "name": c.name, "value": c.value.to_json() }))
        .collect();
    let rows: Vec<Value> = mods
        .iter()
        .map(|m| {
            json!({
                "id": m.id(),
                "authority": m.authority().name(),
                "pending": m.pending_host_commands(),
                "refused": m.host_commands_refused(),
                "snapshot_builds": m.snapshot_builds(),
            })
        })
        .collect();
    Ok(json!({
        "plugin": true,
        "authority": mods.authority().name(),
        "commands": rules.names.iter().collect::<Vec<_>>(),
        "max_pending_per_mod": rules.max_pending_per_mod,
        "max_command_bytes": rules.max_command_bytes,
        "pending": mods.pending_host_commands(),
        "listed": listed,
        "snapshot_generation": mods.snapshot_generation(),
        "mods": rows,
    }))
}

fn components_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.components",
        tool: "lua_components",
        description: "Script components declared by the loaded mods (regia.component): names, owning mod, property schemas with types, defaults, ranges and options.",
        params: &[ParamSchema {
            name: "mod",
            kind: "string",
            required: false,
            description: "only this mod's declarations; default every mod",
        }],
        result: "{ plugin, components: [{ name, mod, props: [{ name, type, default, min?, max?, step?, options?, filter?, multiline? }], has_update }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn props_get_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.props.get",
        tool: "lua_props_get",
        description: "One entity's values of a declared script component, as JSON (entity refs as { entity: bits }, vec3 as { vec3: [x,y,z] }).",
        params: &[
            ParamSchema {
                name: "entity",
                kind: "integer",
                required: true,
                description: "the entity's bits (the inspector shows them as `id`)",
            },
            ParamSchema {
                name: "component",
                kind: "string",
                required: true,
                description: "the declared component name",
            },
        ],
        result: "{ plugin, entity, component, props: { name: value }, defaults?: { name: value } }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn props_set_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.props.set",
        tool: "lua_props_set",
        description: "Merges validated edits into an entity's script-component values: numbers clamp to the declared range, unknown properties and wrong types are refused. Survives a script reload; a declaration change reconciles stored values (kept edits, new defaults, removed props go).",
        params: &[
            ParamSchema {
                name: "entity",
                kind: "integer",
                required: true,
                description: "the entity's bits",
            },
            ParamSchema {
                name: "component",
                kind: "string",
                required: true,
                description: "the declared component name",
            },
            ParamSchema {
                name: "props",
                kind: "object",
                required: true,
                description: "{ property: value } edits (vec3 as { vec3: [x,y,z] }, entity ref as { entity: bits }, colour as [r,g,b,a])",
            },
        ],
        result: "{ plugin, entity, component, props: { name: value } }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn props_list_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.props.list",
        tool: "lua_props_list",
        description: "The declared script components an entity carries, with their values.",
        params: &[ParamSchema {
            name: "entity",
            kind: "integer",
            required: true,
            description: "the entity's bits",
        }],
        result: "{ plugin, entity, components: [{ component, props: { name: value } }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn list_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.list",
        tool: "lua_list",
        description: "Loaded Lua mods: status, error code, call and instruction counters, memory, budgets, subscribed events, reload counters, where each is mounted and the key its saved state goes under; ids a second mount claimed; the last state save and restore.",
        params: &[],
        result: "{ plugin, seed, limits, watch_every_frames, emitted_held, emitted_dropped, last_save?, last_restore?, kept_state, contested_ids, held_state, mods: [{ id, name, version, status, code?, message?, calls, last_instructions, peak_instructions, memory_bytes, peak_memory_bytes, instruction_budget, memory_limit_bytes, subscriptions, reloads, failed_reloads, entity_cap, entity_cap_refused, mount?, save_key, last_reload? }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn run_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.run",
        tool: "lua_run",
        description: "Runs a Lua chunk in a throwaway sandbox (or inside a mod's state) against the live world, under the instruction and memory budgets.",
        params: &[
            ParamSchema {
                name: "chunk",
                kind: "string",
                required: true,
                description: "Lua 5.1 source (text only, at most 64 KiB); its return values come back in `results`",
            },
            ParamSchema {
                name: "mod",
                kind: "string",
                required: false,
                description: "run inside this mod's state (an error disables it; a refused call against a disabled mod reports 0); default a fresh sandbox dropped after the call, spawns and all",
            },
        ],
        result: "{ plugin, ok, results, error?: { code, message }, instructions, memory_bytes, log }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn globals_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.globals",
        tool: "lua_globals",
        description: "A mod's globals, or the fields of one of its tables, sorted by key (raw reads; no Lua runs).",
        params: &[
            ParamSchema {
                name: "mod",
                kind: "string",
                required: true,
                description: "the mod id",
            },
            ParamSchema {
                name: "path",
                kind: "string",
                required: false,
                description: "dotted table path, e.g. `config.speed`'s parent `config`; default the globals",
            },
        ],
        result: "{ plugin, mod, path, entries: [{ key, type, value?, len? }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn errors_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.errors",
        tool: "lua_errors",
        description: "Recent errors of every mod (or one), oldest first, with the call that raised each.",
        params: &[ParamSchema {
            name: "mod",
            kind: "string",
            required: false,
            description: "only this mod",
        }],
        result: "{ plugin, errors: [{ mod, call, code, message }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn restart_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.restart",
        tool: "lua_restart",
        description: "Rebuilds a mod's state from its files and starts it again (re-enables a disabled mod).",
        params: &[ParamSchema {
            name: "mod",
            kind: "string",
            required: true,
            description: "the mod id",
        }],
        result: "{ plugin, mod, status, error?: { code, message } }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn reload_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.reload",
        tool: "lua_reload",
        description: "Hot-reloads a mod while it runs, carrying regia.state across; any failure rolls back to the running version.",
        params: &[
            ParamSchema {
                name: "mod",
                kind: "string",
                required: true,
                description: "the mod id",
            },
            ParamSchema {
                name: "files",
                kind: "object",
                required: false,
                description: "the new files as { path: text }; default read the directory the mod was loaded from",
            },
        ],
        result: "{ plugin, mod, ok, stage, state?, from_version?, to_version?, note?, error?: { code, message }, status, version_before, version_after }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn state_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.state",
        tool: "lua_state",
        description: "What a mod would save now: regia.state as data and its size, or why it cannot be saved (raw read; no Lua runs).",
        params: &[
            ParamSchema {
                name: "mod",
                kind: "string",
                required: true,
                description: "the mod id",
            },
            ParamSchema {
                name: "tree",
                kind: "boolean",
                required: false,
                description: "include the state itself as JSON (default false)",
            },
        ],
        result: "{ plugin, mod, ok, state_version?, tables?, items?, bytes?, entities?, tree?, error?: { code, message } }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn watch_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.watch",
        tool: "lua_watch",
        description: "Reads or sets how often directory-backed mods are checked for changed files (hot reload on save).",
        params: &[ParamSchema {
            name: "every_frames",
            kind: "integer",
            required: false,
            description: "frames between two looks; 0 turns the watch off; default leave it",
        }],
        result: "{ plugin, every_frames, mods: [ids read from a directory] }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn entity_cap_schema() -> MethodSchema {
    MethodSchema {
        name: "lua.entity_cap",
        tool: "lua_entity_cap",
        description: "Reads or sets a mod's entity cap: how many entities it may own at once (spawned and not despawned). A spawn past it is refused with lua_entity_cap. A mod's cap is at most the host's LuaLimits::max_entities_per_mod; null goes back to the host's.",
        params: &[
            ParamSchema {
                name: "mod",
                kind: "string",
                required: true,
                description: "the mod id",
            },
            ParamSchema {
                name: "cap",
                kind: "integer",
                required: false,
                description: "the new cap (0 refuses every spawn), or null for the host's; omit to read",
            },
        ],
        result: "{ plugin, mod, entity_cap, host_cap, override, owned, refused }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn lua_entity_cap(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, true)?.unwrap_or_default().to_owned();
    // `cap` absent: read; `null`: back to the host's; a number: set.
    let change = match params.get("cap") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(
            value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`cap` is a whole number from 0, or null"))?,
        )),
    };
    if world.get_resource::<LuaMods>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    let owned = crate::ecs::owned_count(world, &id);
    let Some(mut mods) = world.get_resource_mut::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    if let Some(cap) = change {
        mods.set_entity_cap(&id, cap).map_err(|_| no_mod(&id))?;
    }
    let host_cap = mods.limits().max_entities_per_mod;
    let module = mods.get(&id).ok_or_else(|| no_mod(&id))?;
    Ok(json!({
        "plugin": true,
        "mod": id,
        "entity_cap": module.entity_cap(),
        "host_cap": host_cap,
        "override": module.entity_cap_override(),
        "owned": owned,
        "refused": module.entity_cap_refused(),
    }))
}

fn error_json(error: &ModError) -> Value {
    json!({ "code": error.code.code(), "message": error.message })
}

/// `from_version` and `to_version` of a carry that changed versions.
fn carry_versions(carry: StateCarry) -> Option<(u32, u32)> {
    match carry {
        StateCarry::Migrated { from, to } | StateCarry::Reset { from, to } => Some((from, to)),
        StateCarry::Fresh | StateCarry::Carried => None,
    }
}

fn reload_json(outcome: &ReloadOutcome) -> Value {
    let mut out = json!({
        "mod": outcome.id,
        "ok": outcome.ok,
        "stage": outcome.stage.name(),
        "version_before": outcome.version_before,
        "version_after": outcome.version_after,
    });
    if let Some(carry) = outcome.state {
        out["state"] = json!(carry.name());
        if let Some((from, to)) = carry_versions(carry) {
            out["from_version"] = json!(from);
            out["to_version"] = json!(to);
        }
    }
    if let Some(note) = &outcome.note {
        out["note"] = json!(note);
    }
    if let Some(error) = &outcome.error {
        out["error"] = error_json(error);
    }
    out
}

fn status_json(module: &LuaMod) -> (&'static str, Value) {
    match module.status() {
        ModStatus::Active => ("active", Value::Null),
        ModStatus::Disabled { code, message } => {
            ("disabled", json!({ "code": code.code(), "message": message }))
        }
    }
}

fn mod_param(params: &Value, required: bool) -> Result<Option<&str>, RpcError> {
    match params.get("mod") {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        _ => Err(RpcError::invalid_params("`mod` (a mod id string) is required")),
    }
}

fn no_mod(id: &str) -> RpcError {
    RpcError::not_found(
        format!("no Lua mod has id `{id}`"),
        "lua.list shows the loaded mods",
    )
}

fn lua_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(mods) = world.get_resource::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    let list: Vec<Value> = mods
        .iter()
        .map(|m| {
            let (status, error) = status_json(m);
            let c = m.counters();
            let limits = m.limits();
            let mut row = json!({
                "id": m.id(),
                "name": m.manifest().name,
                "version": m.manifest().version,
                "status": status,
                "calls": c.calls,
                "last_instructions": c.last_instructions,
                "peak_instructions": c.peak_instructions,
                "memory_bytes": c.memory_bytes,
                "peak_memory_bytes": c.peak_memory_bytes,
                "instruction_budget": limits.instruction_budget,
                "memory_limit_bytes": limits.memory_limit_bytes,
                "subscriptions": m.subscriptions(),
                "reloads": c.reloads,
                "failed_reloads": c.failed_reloads,
                "entity_cap": m.entity_cap(),
                "entity_cap_refused": m.entity_cap_refused(),
                "save_key": m.save_key(),
            });
            if let Some(mount) = m.mount() {
                row["mount"] = json!(mount);
            }
            if let Some(outcome) = m.last_reload() {
                row["last_reload"] = reload_json(outcome);
            }
            if let Value::Object(e) = error {
                row["code"] = e.get("code").cloned().unwrap_or(Value::Null);
                row["message"] = e.get("message").cloned().unwrap_or(Value::Null);
            }
            row
        })
        .collect();
    let mut out = json!({
        "plugin": true,
        "seed": mods.seed(),
        "limits": serde_json::to_value(mods.limits()).unwrap_or(Value::Null),
        "watch_every_frames": mods.watch_every(),
        "emitted_held": mods.emitted_held(),
        "emitted_dropped": mods.emitted_dropped(),
        "kept_state": mods.kept_state().collect::<Vec<_>>(),
        "contested_ids": mods.contested_ids().collect::<Vec<_>>(),
        "held_state": mods.held_state().collect::<Vec<_>>(),
        "mods": list,
    });
    if let Some(report) = mods.last_save() {
        out["last_save"] = serde_json::to_value(report).unwrap_or(Value::Null);
    }
    if let Some(report) = mods.last_restore() {
        out["last_restore"] = serde_json::to_value(report).unwrap_or(Value::Null);
    }
    Ok(out)
}

fn lua_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let chunk = params
        .get("chunk")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`chunk` (Lua source text) is required"))?;
    if chunk.len() > MAX_CHUNK_BYTES {
        return Err(RpcError::invalid_params(format!(
            "`chunk` is at most {MAX_CHUNK_BYTES} bytes"
        )));
    }
    let id = mod_param(params, false)?.map(str::to_owned);
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let outcome = world.resource_scope::<LuaMods, _>(|world, mut mods| {
        mods.run_chunk(&mut EcsModWorld::new(world), id.as_deref(), chunk)
    });
    match outcome {
        Ok(o) => {
            let mut out = json!({
                "plugin": true,
                "ok": o.ok,
                "results": o.results.iter().map(crate::ScriptValue::to_json).collect::<Vec<_>>(),
                "instructions": o.instructions,
                "memory_bytes": o.memory_bytes,
                "log": o.log,
            });
            if let Some(error) = &o.error {
                out["error"] = error_json(error);
            }
            Ok(out)
        }
        Err(error) if error.code == LuaErrorCode::UnknownMod => Err(no_mod(id.as_deref().unwrap_or(""))),
        Err(error) => Ok(json!({ "plugin": true, "ok": false, "results": [], "error": error_json(&error) })),
    }
}

fn lua_globals(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, true)?.unwrap_or_default().to_owned();
    let path = params.get("path").and_then(Value::as_str).map(str::to_owned);
    let Some(mods) = world.get_resource::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    match mods.globals(&id, path.as_deref(), MAX_GLOBALS) {
        Ok(entries) => {
            let entries: Vec<Value> = entries
                .iter()
                .map(|e| {
                    let mut row = json!({ "key": e.key, "type": e.kind });
                    if let Some(v) = &e.value {
                        row["value"] = v.to_json();
                    }
                    if let Some(len) = e.len {
                        row["len"] = json!(len);
                    }
                    row
                })
                .collect();
            Ok(json!({ "plugin": true, "mod": id, "path": path.unwrap_or_default(), "entries": entries }))
        }
        Err(error) if error.code == LuaErrorCode::UnknownMod => Err(no_mod(&id)),
        Err(error) => Ok(json!({
            "plugin": true,
            "mod": id,
            "path": path.unwrap_or_default(),
            "entries": [],
            "error": error_json(&error),
        })),
    }
}

fn lua_errors(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, false)?.map(str::to_owned);
    let Some(mods) = world.get_resource::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    if let Some(id) = &id
        && mods.get(id).is_none()
    {
        return Err(no_mod(id));
    }
    let errors: Vec<Value> = mods
        .iter()
        .filter(|m| id.as_deref().is_none_or(|id| m.id() == id))
        .flat_map(|m| {
            m.errors()
                .map(|(call, e)| json!({ "mod": m.id(), "call": call, "code": e.code.code(), "message": e.message }))
                .collect::<Vec<_>>()
        })
        .collect();
    Ok(json!({ "plugin": true, "errors": errors }))
}

fn lua_restart(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, true)?.unwrap_or_default().to_owned();
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let result = world.resource_scope::<LuaMods, _>(|world, mut mods| {
        let result = mods.restart(&id, &mut EcsModWorld::new(world));
        let status = mods.get(&id).map(|m| status_json(m).0);
        (result, status)
    });
    match result {
        (Err(error), _) if error.code == LuaErrorCode::UnknownMod => Err(no_mod(&id)),
        (Ok(()), status) => Ok(json!({ "plugin": true, "mod": id, "status": status })),
        (Err(error), status) => {
            Ok(json!({ "plugin": true, "mod": id, "status": status, "error": error_json(&error) }))
        }
    }
}

/// The `files` param: `{ path: text }`. A path a mod may not hold is refused here: silently dropping
/// it would turn into a confusing `lua_require` later.
fn files_param(params: &Value) -> Result<Option<ModFiles>, RpcError> {
    let Some(value) = params.get("files").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params("`files` is an object of { relative path: text }");
    let map = value.as_object().ok_or_else(bad)?;
    let entries = map
        .iter()
        .map(|(path, text)| Ok((path.clone(), text.as_str().ok_or_else(bad)?.to_owned())))
        .collect::<Result<Vec<_>, RpcError>>()?;
    let files = ModFiles::from_map(entries);
    if files.paths().count() != map.len() {
        return Err(RpcError::invalid_params(
            "`files` has a path that leaves the mod (absolute, `..` or a backslash)",
        ));
    }
    Ok(Some(files))
}

fn lua_reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, true)?.unwrap_or_default().to_owned();
    let files = files_param(params)?;
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let result = world.resource_scope::<LuaMods, _>(|world, mut mods| {
        let outcome = mods.reload(&id, files, &mut EcsModWorld::new(world));
        let status = mods.get(&id).map(|m| status_json(m).0);
        (outcome, status)
    });
    match result {
        (Ok(outcome), status) => {
            let mut out = reload_json(&outcome);
            out["plugin"] = json!(true);
            out["status"] = json!(status);
            Ok(out)
        }
        (Err(_), _) => Err(no_mod(&id)),
    }
}

fn lua_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, true)?.unwrap_or_default().to_owned();
    let tree = params.get("tree").and_then(Value::as_bool).unwrap_or(false);
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let peek =
        world.resource_scope::<LuaMods, _>(|world, mods| mods.peek_state(&id, &mut EcsModWorld::new(world)));
    match peek {
        Ok(state) => {
            let mut out = json!({
                "plugin": true,
                "mod": id,
                "ok": true,
                "state_version": state.state_version,
                "tables": state.tree.tables.len(),
                "items": state.tree.items(),
                "bytes": crate::state::encoded_size(&state).unwrap_or(0),
                "entities": state.entities.len(),
            });
            if tree {
                out["tree"] = state.tree.to_json();
            }
            Ok(out)
        }
        Err(error) if error.code == LuaErrorCode::UnknownMod => Err(no_mod(&id)),
        Err(error) => Ok(json!({ "plugin": true, "mod": id, "ok": false, "error": error_json(&error) })),
    }
}

fn lua_watch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frames = match params.get("every_frames") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`every_frames` is a whole number from 0"))?,
        ),
    };
    let Some(mut mods) = world.get_resource_mut::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    if let Some(frames) = frames {
        mods.set_watch(frames);
    }
    let watched: Vec<&str> = mods
        .iter()
        .filter(|m| m.files().source_dir().is_some())
        .map(LuaMod::id)
        .collect();
    Ok(json!({ "plugin": true, "every_frames": mods.watch_every(), "mods": watched }))
}

// --- Script components (ADR-3356): lua.components, lua.props.get/set/list ---

/// JSON → [`ScriptValue`]. `null` is nil. `{ "vec3": [x, y, z] }` and `{ "entity": bits }` are
/// the shapes [`ScriptValue::to_json`] writes; any other object is a map.
fn script_value_of(value: &Value) -> Result<ScriptValue, RpcError> {
    match value {
        Value::Null => Ok(ScriptValue::Nil),
        Value::Bool(b) => Ok(ScriptValue::Bool(*b)),
        Value::Number(n) => {
            Ok(ScriptValue::Number(n.as_f64().ok_or_else(|| {
                RpcError::invalid_params("numbers must be finite doubles")
            })?))
        }
        Value::String(s) => Ok(ScriptValue::String(s.clone())),
        Value::Array(items) => Ok(ScriptValue::List(
            items.iter().map(script_value_of).collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(map) => {
            if let Some(tagged) = tagged_vec3_or_entity(map)? {
                Ok(tagged)
            } else {
                Ok(ScriptValue::Map(
                    map.iter()
                        .map(|(k, v)| Ok((k.clone(), script_value_of(v)?)))
                        .collect::<Result<std::collections::BTreeMap<_, _>, RpcError>>()?,
                ))
            }
        }
    }
}

/// The single-key objects [`ScriptValue::to_json`] emits for a vector and an entity handle.
/// Any other object, including a malformed `vec3` or `entity` payload, is not tagged.
fn tagged_vec3_or_entity(map: &serde_json::Map<String, Value>) -> Result<Option<ScriptValue>, RpcError> {
    if map.len() != 1 {
        return Ok(None);
    }
    if let Some(value) = map.get("vec3") {
        let Some(items) = value.as_array() else {
            return Ok(None);
        };
        if items.len() != 3 {
            return Ok(None);
        }
        let mut xyz = [0.0f32; 3];
        for (index, item) in items.iter().enumerate() {
            let Some(n) = item.as_f64() else {
                return Ok(None);
            };
            if !n.is_finite() {
                return Err(RpcError::invalid_params("a vec3 is three finite numbers"));
            }
            xyz[index] = n as f32;
        }
        return Ok(Some(ScriptValue::Vec3(xyz)));
    }
    if let Some(value) = map.get("entity") {
        return Ok(value.as_u64().map(ScriptValue::Entity));
    }
    Ok(None)
}

/// The `entity` param: entity bits.
fn entity_param(params: &Value) -> Result<u64, RpcError> {
    params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (the entity's bits) is required"))
}

/// The `component` param.
fn component_param(params: &Value) -> Result<&str, RpcError> {
    params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` (a declared component name) is required"))
}

/// The declaration of `name` among the loaded mods; the name is globally unique.
fn find_decl(mods: &LuaMods, name: &str) -> Option<crate::components::ComponentDecl> {
    mods.iter()
        .find_map(|m| m.components().into_iter().find(|decl| decl.name == name))
}

/// Values as a JSON object keyed by property name.
fn props_json(values: &[(String, ScriptValue)]) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(name, value)| (name.clone(), value.to_json()))
            .collect(),
    )
}

fn lua_components(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = mod_param(params, false)?.map(str::to_owned);
    let Some(mods) = world.get_resource::<LuaMods>() else {
        return Ok(json!({ "plugin": false }));
    };
    if let Some(id) = &id
        && mods.get(id).is_none()
    {
        return Err(no_mod(id));
    }
    let components: Vec<Value> = mods
        .iter()
        .filter(|m| id.as_deref().is_none_or(|id| m.id() == id))
        .flat_map(|m| {
            m.components().into_iter().map(move |decl| {
                let props: Vec<Value> = decl
                    .props
                    .iter()
                    .map(|prop| {
                        let mut row = json!({
                            "name": prop.name,
                            "type": prop.ty.name(),
                            "default": prop.default.to_json(),
                            "multiline": prop.multiline,
                        });
                        for (key, value) in [
                            ("min", prop.min.map(|n| json!(n))),
                            ("max", prop.max.map(|n| json!(n))),
                            ("step", prop.step.map(|n| json!(n))),
                            (
                                "options",
                                (!prop.options.is_empty())
                                    .then(|| Value::Array(prop.options.iter().map(|o| json!(o)).collect())),
                            ),
                            ("filter", prop.filter.as_ref().map(|f| json!(f))),
                        ] {
                            if let Some(value) = value {
                                row[key] = value;
                            }
                        }
                        row
                    })
                    .collect();
                json!({
                    "name": decl.name,
                    "mod": m.id(),
                    "props": props,
                    "has_update": decl.has_update,
                })
            })
        })
        .collect();
    Ok(json!({ "plugin": true, "components": components }))
}

fn lua_props_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    let component = component_param(params)?.to_owned();
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let Some(decl) = find_decl(world.resource::<LuaMods>(), &component) else {
        return Err(RpcError::not_found(
            format!("no loaded mod declares component `{component}`"),
            "lua.components lists the declared components",
        ));
    };
    let mut ecs = EcsModWorld::new(world);
    match ecs.component_props(entity, &component) {
        Some(values) => {
            let defaults: std::collections::BTreeMap<String, ScriptValue> =
                decl.initial_values().into_iter().collect();
            Ok(json!({
                "plugin": true,
                "entity": entity,
                "component": component,
                "props": props_json(&values),
                "defaults": props_json(&defaults.into_iter().collect::<Vec<_>>()),
            }))
        }
        None => Err(RpcError::not_found(
            format!("the entity does not carry component `{component}`"),
            "lua.props.list lists an entity's components",
        )),
    }
}

fn lua_props_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    let component = component_param(params)?.to_owned();
    let edits = params
        .get("props")
        .and_then(Value::as_object)
        .ok_or_else(|| RpcError::invalid_params("`props` (an object of { property: value }) is required"))?;
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let Some(decl) = find_decl(world.resource::<LuaMods>(), &component) else {
        return Err(RpcError::not_found(
            format!("no loaded mod declares component `{component}`"),
            "lua.components lists the declared components",
        ));
    };
    // Coerce every edit before writing anything.
    let mut coerced: Vec<(String, ScriptValue)> = Vec::with_capacity(edits.len());
    for (name, value) in edits {
        let Some(prop) = decl.prop(name) else {
            return Err(RpcError::invalid_params(format!(
                "component `{component}` has no property `{name}` (lua.components lists them)"
            )));
        };
        let value = script_value_of(value)?;
        coerced.push((
            name.clone(),
            prop.coerce(value).map_err(RpcError::invalid_params)?,
        ));
    }
    let mut ecs = EcsModWorld::new(world);
    let Some(mut current) = ecs.component_props(entity, &component) else {
        return Err(RpcError::not_found(
            format!("the entity does not carry component `{component}`"),
            "a script's e:add or the inspector attaches it first",
        ));
    };
    for (name, value) in coerced {
        match current.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, slot)) => *slot = value,
            None => current.push((name, value)),
        }
    }
    current.sort_by(|a, b| a.0.cmp(&b.0));
    ecs.set_component_props(entity, &component, current.clone());
    Ok(json!({
        "plugin": true,
        "entity": entity,
        "component": component,
        "props": props_json(&current),
    }))
}

fn lua_props_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    if !world.contains_resource::<LuaMods>() {
        return Ok(json!({ "plugin": false }));
    }
    let names = lua_components_names(world);
    let mut ecs = EcsModWorld::new(world);
    let components: Vec<Value> = names
        .iter()
        .filter_map(|decl| {
            ecs.component_props(entity, decl)
                .map(|values| json!({ "component": decl, "props": props_json(&values) }))
        })
        .collect();
    Ok(json!({ "plugin": true, "entity": entity, "components": components }))
}

/// Every declared component name (used by `lua.props.list`).
fn lua_components_names(world: &mut World) -> Vec<String> {
    world
        .get_resource::<LuaMods>()
        .map(|mods| {
            let mut names: Vec<String> = mods
                .iter()
                .flat_map(|m| m.components().into_iter().map(|d| d.name))
                .collect();
            names.sort();
            names.dedup();
            names
        })
        .unwrap_or_default()
}

/// Registers the `lua.*` methods and the `lua_mods` capability on the world's `AgentExtension`
/// (creating it when absent).
pub(crate) fn install(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(list_schema(), lua_list);
    extension.register(run_schema(), lua_run);
    extension.register(globals_schema(), lua_globals);
    extension.register(errors_schema(), lua_errors);
    extension.register(restart_schema(), lua_restart);
    extension.register(reload_schema(), lua_reload);
    extension.register(state_schema(), lua_state);
    extension.register(watch_schema(), lua_watch);
    // The per-mod entity cap (ADR-2418).
    extension.register(entity_cap_schema(), lua_entity_cap);
    // Script components (ADR-3356).
    extension.register(components_schema(), lua_components);
    extension.register(props_get_schema(), lua_props_get);
    extension.register(props_set_schema(), lua_props_set);
    extension.register(props_list_schema(), lua_props_list);
    // Host and presentation runtimes, the host command queue, the snapshot (ADR-8466).
    extension.register(authority_schema(), lua_authority);
    extension.add_capability(
        CAPABILITY,
        "Lua 5.1 mods (regia_script_lua): lua.list, lua.run, lua.globals, lua.errors, lua.restart, lua.reload, lua.state, lua.watch, lua.entity_cap, lua.components, lua.props.get, lua.props.set, lua.props.list, lua.authority",
    );
    world.insert_resource(extension);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The method names the book page documents.
    const METHODS: [&str; 14] = [
        "lua.list",
        "lua.run",
        "lua.globals",
        "lua.errors",
        "lua.restart",
        "lua.reload",
        "lua.state",
        "lua.watch",
        "lua.entity_cap",
        "lua.components",
        "lua.props.get",
        "lua.props.set",
        "lua.props.list",
        "lua.authority",
    ];

    #[test]
    fn install_registers_every_documented_method_and_the_capability() {
        let mut world = World::new();
        install(&mut world);
        let extension = world.resource::<AgentExtension>();
        let mut names = extension.method_names();
        names.retain(|n| n.starts_with("lua."));
        names.sort_unstable();
        let mut expected = METHODS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(
            extension
                .capabilities()
                .iter()
                .any(|(name, _)| *name == CAPABILITY)
        );
        for name in METHODS {
            let schema = extension.method_schema(name).expect("registered");
            assert_eq!(schema.tool, name.replace('.', "_"));
        }
    }

    /// Every field `lua.list` sends, at the top and per mod, is named in its schema's result,
    /// so an agent reading the schema sees `entity_cap` and `entity_cap_refused` (spec 84 § 13).
    #[test]
    fn the_list_schema_names_every_field_the_list_sends() {
        use crate::sandbox::LuaLimits;
        use crate::world::NoWorld;
        let files = || {
            ModFiles::from_map([
                (
                    "luamod.ron".to_owned(),
                    r#"(format_version: 1, id: "a", name: "a", version: "1", entry: "main.lua", api_version: 1)"#
                        .to_owned(),
                ),
                ("main.lua".to_owned(), "regia.state.n = 1".to_owned()),
            ])
        };
        let mut mods = LuaMods::new(7, LuaLimits::default());
        mods.load(files(), &mut NoWorld).expect("loads");
        let _ = mods.save_state(&mut NoWorld);
        let _ = mods.reload("a", Some(files()), &mut NoWorld);
        let mut world = World::new();
        world.insert_resource(mods);
        let out = lua_list(&mut world, &json!({})).expect("lists");
        let schema = list_schema().result;
        let (top, rows) = schema.split_once("mods: [").expect("a mods list");
        // Whole-identifier membership, not substring: `entity_cap` must not pass because
        // `entity_cap_refused` contains it, and `reloads` must not pass because `failed_reloads`
        // does (spec 84 § 13).
        fn idents(text: &str) -> std::collections::HashSet<&str> {
            text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|s| !s.is_empty())
                .collect()
        }
        let top_fields = idents(top);
        let row_fields = idents(rows);
        let mut checked = 0;
        for key in out.as_object().expect("an object").keys() {
            assert!(
                top_fields.contains(key.as_str()) || key == "mods",
                "`{key}` is not in {schema}"
            );
            checked += 1;
        }
        let row = out["mods"][0].as_object().expect("one mod");
        for key in row.keys() {
            assert!(
                row_fields.contains(key.as_str()),
                "mod field `{key}` is not in {schema}"
            );
            checked += 1;
        }
        for key in ["entity_cap", "entity_cap_refused", "last_reload"] {
            assert!(row.contains_key(key), "{key}");
        }
        assert!(out.get("last_save").is_some(), "{out}");
        assert!(checked > 20, "{checked}");
    }

    #[test]
    fn the_files_param_takes_text_under_safe_paths_only() {
        assert_eq!(files_param(&json!({})).ok(), Some(None));
        assert_eq!(files_param(&json!({ "files": null })).ok(), Some(None));
        let files = files_param(&json!({ "files": { "main.lua": "return 1" } }))
            .ok()
            .flatten()
            .expect("files");
        assert_eq!(files.get("main.lua"), Some("return 1"));
        for bad in [
            json!({ "files": 3 }),
            json!({ "files": { "main.lua": 1 } }),
            json!({ "files": { "../main.lua": "x" } }),
        ] {
            assert!(files_param(&bad).is_err(), "{bad}");
        }
    }

    /// `lua.authority` sends every field its schema names, lists waiting commands without
    /// draining them, and reports the presentation side.
    #[test]
    fn the_authority_method_reports_the_queue_without_draining_it() {
        use crate::authority::{HostCommandRules, ScriptAuthority};
        use crate::sandbox::LuaLimits;
        use crate::world::NoWorld;
        let files = |main: &str| {
            ModFiles::from_map([
                (
                    "luamod.ron".to_owned(),
                    r#"(format_version: 1, id: "a", name: "a", version: "1", entry: "main.lua", api_version: 1)"#
                        .to_owned(),
                ),
                ("main.lua".to_owned(), main.to_owned()),
            ])
        };
        let mut mods =
            LuaMods::new(7, LuaLimits::default()).with_host_commands(HostCommandRules::new(["order"]));
        mods.load(
            files("regia.command('order', 2) regia.command('nope')"),
            &mut NoWorld,
        )
        .expect("loads");
        let mut world = World::new();
        world.insert_resource(mods);
        let out = lua_authority(&mut world, &json!({})).expect("reads");
        assert_eq!(out["authority"], "host");
        assert_eq!(out["commands"], json!(["order"]));
        assert_eq!(out["pending"], 1);
        assert_eq!(
            out["listed"],
            json!([{ "mod": "a", "name": "order", "value": 2.0 }])
        );
        assert_eq!(out["mods"][0]["refused"], 1);
        let schema = authority_schema().result;
        for key in out.as_object().expect("object").keys() {
            assert!(schema.contains(key.as_str()), "`{key}` is not in {schema}");
        }
        for key in out["mods"][0].as_object().expect("row").keys() {
            assert!(schema.contains(key.as_str()), "`{key}` is not in {schema}");
        }
        assert_eq!(
            world.resource::<LuaMods>().pending_host_commands(),
            1,
            "not drained"
        );

        let mut client = World::new();
        let mut mods = LuaMods::new(7, LuaLimits::default()).with_authority(ScriptAuthority::Presentation);
        mods.load(files("assert(regia.command == nil)"), &mut NoWorld)
            .expect("loads");
        client.insert_resource(mods);
        let out = lua_authority(&mut client, &json!({})).expect("reads");
        assert_eq!(out["authority"], "presentation");
        assert_eq!(out["mods"][0]["authority"], "presentation");
        assert_eq!(out["pending"], 0);
        assert_eq!(
            lua_authority(&mut World::new(), &json!({})).expect("reads"),
            json!({ "plugin": false })
        );
    }

    #[test]
    fn a_missing_required_mod_param_is_invalid_params() {
        assert!(mod_param(&json!({}), true).is_err());
        assert!(mod_param(&json!({ "mod": 3 }), false).is_err());
        assert_eq!(mod_param(&json!({}), false).ok(), Some(None));
        assert_eq!(mod_param(&json!({ "mod": "a" }), true).ok(), Some(Some("a")));
    }
}
