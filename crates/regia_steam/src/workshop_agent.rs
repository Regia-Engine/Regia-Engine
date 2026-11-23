// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the Steam Workshop (spec 24 § 2, ADR-2418, `book/agents/workshop.md`):
//!
//! - `steam.workshop_status` reads the service, the items seen (subscribed, install folder, the
//!   `mods://` mod id while mounted, the last error) and the uploads.
//! - `steam.workshop_upload` sends a folder as a new item or an update. It publishes outside the
//!   machine, so it needs the `tools` permission, and the real Workshop also needs the game's
//!   opt-in (`SteamWorkshopPlugin::allow_agent_upload`); the mock always takes it.
//! - `steam.workshop_subscribe` and `steam.workshop_unsubscribe` change subscriptions; an
//!   installed item mounts at `mods://` on a later frame (step a frame, then `mods.list`).
//!
//! Without `SteamWorkshopPlugin` every method answers `{ plugin: false }`. Errors carry their
//! code in `data.code` (one of `WorkshopError::CODES`, or `F0002` for a bad parameter).

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;

use crate::workshop::{
    SteamWorkshop, UploadStatus, WorkshopError, WorkshopItemId, WorkshopUpload, WorkshopVisibility,
    workshop_mod_id,
};

fn workshop_error(e: &WorkshopError) -> RpcError {
    let hint = match e {
        WorkshopError::Unavailable => "Steam is not running, or SteamWorkshopPlugin has no service",
        WorkshopError::Invalid(_) => "see the limits on book/agents/workshop.md",
        WorkshopError::Content(_) => "the content folder must mount as mods:// (no links, valid names)",
        WorkshopError::UnknownItem(_) => "steam.workshop_status lists the items seen",
        WorkshopError::Platform(_) => "Steam refused the call; see the message",
        WorkshopError::UploadNotAllowed => "the game must add SteamWorkshopPlugin::allow_agent_upload",
    };
    RpcError::new(INVALID_PARAMS, e.code(), e.to_string(), hint)
}

fn bad(message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message)
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(bad(format!("`{name}` must be a string, got {other}"))),
    }
}

/// An item id as a decimal string or an integer.
fn item_param(params: &Value, name: &str, required: bool) -> Result<Option<WorkshopItemId>, RpcError> {
    let id = match params.get(name) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(
            s.parse::<u64>()
                .map_err(|_| bad(format!("`{name}` must be a 64-bit item id, got `{s}`")))?,
        ),
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| bad(format!("`{name}` must be a 64-bit item id, got {v}")))?,
        ),
    };
    if required && id.is_none() {
        return Err(bad(format!("`{name}` (item id) is required")));
    }
    Ok(id.map(WorkshopItemId))
}

/// Confines an upload path (ADR-2418, like `mods.mount` and `replay.load`): it must resolve, links
/// followed, under the project root, the system temp dir or the working dir. An upload publishes
/// what it reads, and the mock serves it back through `mods.read`, so it may not reach any host
/// path.
fn confine(world: &World, name: &str, path: &str) -> Result<std::path::PathBuf, RpcError> {
    let refused = |why: String| {
        let mut e = RpcError::new(
            INVALID_PARAMS,
            "F0002",
            format!("`{name}`: {why}"),
            "use a path under the project root, the system temp dir or the working dir; see book/agents/workshop.md",
        );
        if let Some(Value::Object(data)) = e.data.as_mut() {
            data.insert("reason".into(), json!("path_outside_project"));
            data.insert(
                "allowed_roots".into(),
                json!(["project", "temp_dir", "working_dir"]),
            );
        }
        e
    };
    let mut roots = vec![std::env::temp_dir()];
    if let Ok(dir) = std::env::current_dir() {
        roots.push(dir);
    }
    if let Some(server) = world.get_resource::<regia_asset::AssetServer>() {
        roots.push(server.project_root().to_path_buf());
    }
    let given = std::path::Path::new(path);
    let given = if given.is_relative() {
        roots
            .last()
            .map_or_else(|| given.to_path_buf(), |root| root.join(given))
    } else {
        given.to_path_buf()
    };
    let real = std::fs::canonicalize(&given).map_err(|e| refused(format!("{}: {e}", given.display())))?;
    let inside = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| real.starts_with(root));
    if inside {
        Ok(real)
    } else {
        Err(refused(format!(
            "{} is outside every allowed root",
            real.display()
        )))
    }
}

fn not_running() -> Value {
    json!({ "plugin": false })
}

fn status_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.workshop_status",
        tool: "steam_workshop_status",
        description: "The Steam Workshop: which service (steam, mock, unavailable), whether installed items mount at mods:// by themselves, the items seen (subscribed, install folder, the mods:// mod id while mounted, installs, the last error) and the uploads (pending, done with the item id, failed with the code).",
        params: &[],
        result: "{ plugin, service, auto_mount, agent_upload, items: [{ item, subscribed, installed, downloading, needs_update, folder, mod_id, mounted, installs, error }], uploads: [{ request, state, item?, needs_legal_agreement?, code?, message? }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_workshop_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(workshop) = world.get_resource::<SteamWorkshop>() else {
        return Ok(not_running());
    };
    let items: Vec<Value> = workshop
        .items()
        .iter()
        .map(|(item, status)| {
            let state = workshop.item_state(*item);
            json!({
                "item": item.0.to_string(),
                "subscribed": status.subscribed,
                "installed": state.installed,
                "downloading": state.downloading,
                "needs_update": state.needs_update,
                "folder": status.folder.as_ref().map(|f| f.display().to_string()),
                "mod_id": workshop_mod_id(*item),
                "mounted": status.mounted.is_some(),
                "installs": status.installs,
                "error": status.error,
            })
        })
        .collect();
    let uploads: Vec<Value> = workshop
        .uploads()
        .iter()
        .map(|(request, status)| match status {
            UploadStatus::Pending => json!({ "request": request.0, "state": "pending" }),
            UploadStatus::Done {
                item,
                needs_legal_agreement,
            } => json!({
                "request": request.0,
                "state": "done",
                "item": item.0.to_string(),
                "needs_legal_agreement": needs_legal_agreement,
            }),
            UploadStatus::Failed(e) => json!({
                "request": request.0,
                "state": "failed",
                "code": e.code(),
                "message": e.to_string(),
            }),
        })
        .collect();
    Ok(json!({
        "plugin": true,
        "service": workshop.service_name(),
        "auto_mount": workshop.auto_mount(),
        "agent_upload": workshop.agent_upload_allowed(),
        "items": items,
        "uploads": uploads,
    }))
}

fn upload_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.workshop_upload",
        tool: "steam_workshop_upload",
        description: "Upload a folder to the Steam Workshop as a new item, or as an update of `item`. The folder is checked like a mods:// mount first (no links, valid names, not empty); its content hash goes into the item metadata unless `metadata` is given. Publishes outside the machine: needs the tools permission, and the real Workshop also needs the game's opt-in. Answers at once with a request id; the result shows in steam.workshop_status on a later frame.",
        params: &[
            ParamSchema {
                name: "title",
                kind: "string",
                required: true,
                description: "1..=128 bytes",
            },
            ParamSchema {
                name: "content_dir",
                kind: "string",
                required: true,
                description: "host folder whose files become the item",
            },
            ParamSchema {
                name: "item",
                kind: "string",
                required: false,
                description: "an item id to update (decimal); omit to create one",
            },
            ParamSchema {
                name: "description",
                kind: "string",
                required: false,
                description: "at most 8,000 bytes",
            },
            ParamSchema {
                name: "preview",
                kind: "string",
                required: false,
                description: "host path of a preview image",
            },
            ParamSchema {
                name: "tags",
                kind: "array",
                required: false,
                description: "at most 32 strings of 1..=255 bytes, no commas",
            },
            ParamSchema {
                name: "visibility",
                kind: "string",
                required: false,
                description: "public, friends_only, private (default) or unlisted",
            },
            ParamSchema {
                name: "change_note",
                kind: "string",
                required: false,
                description: "the note in the item's history",
            },
            ParamSchema {
                name: "metadata",
                kind: "string",
                required: false,
                description: "developer metadata, at most 5,000 bytes (default regia.mods.content=<hash>)",
            },
        ],
        result: "{ plugin, request, service }",
        mutating: true,
        permission: Permission::Tools,
    }
}

fn steam_workshop_upload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let title = str_param(params, "title")?.ok_or_else(|| bad("`title` (string) is required"))?;
    let dir = str_param(params, "content_dir")?.ok_or_else(|| bad("`content_dir` (string) is required"))?;
    let dir = confine(world, "content_dir", dir)?;
    let mut upload = WorkshopUpload::new(title, dir);
    upload.item = item_param(params, "item", false)?;
    upload.description = str_param(params, "description")?.unwrap_or_default().to_owned();
    upload.preview = str_param(params, "preview")?
        .map(|p| confine(world, "preview", p))
        .transpose()?;
    upload.change_note = str_param(params, "change_note")?.unwrap_or_default().to_owned();
    upload.metadata = str_param(params, "metadata")?.unwrap_or_default().to_owned();
    if let Some(name) = str_param(params, "visibility")? {
        upload.visibility = WorkshopVisibility::from_name(name).ok_or_else(|| {
            bad(format!(
                "unknown visibility `{name}`: public, friends_only, private or unlisted"
            ))
        })?;
    }
    match params.get("tags") {
        None | Some(Value::Null) => {}
        Some(Value::Array(tags)) => {
            upload.tags = tags
                .iter()
                .map(|t| {
                    t.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| bad("`tags` must be strings"))
                })
                .collect::<Result<_, _>>()?;
        }
        Some(other) => return Err(bad(format!("`tags` must be an array of strings, got {other}"))),
    }
    let Some(mut workshop) = world.get_resource_mut::<SteamWorkshop>() else {
        return Ok(not_running());
    };
    if !workshop.agent_upload_allowed() {
        return Err(workshop_error(&WorkshopError::UploadNotAllowed));
    }
    let request = workshop.upload(&upload).map_err(|e| workshop_error(&e))?;
    Ok(json!({ "plugin": true, "request": request.0, "service": workshop.service_name() }))
}

fn subscribe_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.workshop_subscribe",
        tool: "steam_workshop_subscribe",
        description: "Subscribe to a Workshop item. Steam downloads it; once installed it mounts at mods:// as workshop.<item> at the end of the load order (a later frame: step, then mods.list).",
        params: &[ParamSchema {
            name: "item",
            kind: "string",
            required: true,
            description: "the item id (decimal string or integer)",
        }],
        result: "{ plugin, item, mod_id }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn steam_workshop_subscribe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let item = item_param(params, "item", true)?.ok_or_else(|| bad("`item` is required"))?;
    let Some(mut workshop) = world.get_resource_mut::<SteamWorkshop>() else {
        return Ok(not_running());
    };
    workshop.subscribe(item).map_err(|e| workshop_error(&e))?;
    Ok(json!({ "plugin": true, "item": item.0.to_string(), "mod_id": workshop_mod_id(item) }))
}

fn unsubscribe_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.workshop_unsubscribe",
        tool: "steam_workshop_unsubscribe",
        description: "Unsubscribe from a Workshop item; it unmounts from mods:// when Steam confirms (a later frame).",
        params: &[ParamSchema {
            name: "item",
            kind: "string",
            required: true,
            description: "the item id (decimal string or integer)",
        }],
        result: "{ plugin, item }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

fn steam_workshop_unsubscribe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let item = item_param(params, "item", true)?.ok_or_else(|| bad("`item` is required"))?;
    let Some(mut workshop) = world.get_resource_mut::<SteamWorkshop>() else {
        return Ok(not_running());
    };
    workshop.unsubscribe(item).map_err(|e| workshop_error(&e))?;
    Ok(json!({ "plugin": true, "item": item.0.to_string() }))
}

/// Registers `steam.workshop_*` on the world's `AgentExtension` (creating it when absent).
pub(crate) fn install(world: &mut World) {
    let mut extension = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(status_schema(), steam_workshop_status);
    extension.register(upload_schema(), steam_workshop_upload);
    extension.register(subscribe_schema(), steam_workshop_subscribe);
    extension.register(unsubscribe_schema(), steam_workshop_unsubscribe);
    world.insert_resource(extension);
}
