// SPDX-License-Identifier: MPL-2.0
//! `mods.*`: the `mods://` file system (`regia_asset::mods_vfs`, ADR-2418, `book/agents/mods.md`).
//!
//! - `mods.list` reads the mounted mods in load order, the served file count, the overrides and the
//!   content-set hash.
//! - `mods.mount` mounts a folder or a `.fpak` at the end of the load order; `mods.unmount` removes
//!   one. Host paths are confined like `replay.load` (project root, temp dir,
//!   `output_roots` and `replay_roots` of `.regia/agent.toml`).
//! - `mods.order` reads or sets the load order (first = base, later overrides earlier).
//! - `mods.hash` reads the content-set hash and each mod's content hash; `publish: true` also
//!   writes it into the current lobby's data under `mods.content_set` (the lobby owner only).
//! - `mods.read` reads one served file: which mod serves it, its size and BLAKE3, and its text when
//!   it is UTF-8.
//!
//! Without a `ModsVfs` resource the read methods answer an empty set; `mods.mount` inserts one.
//! Refusals carry the `mods_*` code of `ModsErrorCode` in `data.code`.

use regia_app::prelude::*;
use regia_asset::mods_vfs::{ModConflict, ModInfo, ModSource, ModsError, ModsErrorCode, ModsVfs};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most conflicts `mods.list` returns (the count is always exact).
const MAX_LISTED_CONFLICTS: usize = 256;

/// Largest file `mods.read` returns as text.
const MAX_TEXT_BYTES: usize = 64 * 1024;

/// The kinds of directory a `mods.mount` path may lie under (`data.allowed_roots` of a refusal).
const MOUNT_ROOT_KINDS: &[&str] = &["project", "temp_dir", "output_roots", "replay_roots"];

fn mods_error(e: &ModsError) -> RpcError {
    let hint = match e.code {
        ModsErrorCode::PathEscape => "mods:// paths are relative: no leading `/`, no drive letter, no `..`",
        ModsErrorCode::PathInvalid => "use `/` separators and plain names (see book/agents/mods.md)",
        ModsErrorCode::Symlink => "a mod folder may not hold links; copy the files in",
        ModsErrorCode::NotFound => "mods.list shows what is mounted",
        ModsErrorCode::UnknownMod => "mods.list shows the mounted mod ids",
        ModsErrorCode::Duplicate => "unmount it first (mods.unmount) or pick another id",
        ModsErrorCode::IdInvalid => "mod ids are [a-z0-9_.-], 1..=64 bytes",
        ModsErrorCode::OrderInvalid => "name every mounted mod exactly once",
        ModsErrorCode::Changed => "the file changed after the mount; unmount and mount the mod again",
        ModsErrorCode::ReadOnly => "mods:// is read-only",
        ModsErrorCode::Io | ModsErrorCode::Pak | ModsErrorCode::TooLarge => "see the message",
    };
    RpcError::new(INVALID_PARAMS, e.code.code(), e.message.clone(), hint)
}

fn required_str<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    match params.get(name) {
        Some(Value::String(s)) => Ok(s),
        _ => Err(RpcError::invalid_params(format!("`{name}` (string) is required"))),
    }
}

fn info_json(m: &ModInfo) -> Value {
    json!({
        "id": m.id,
        "order": m.order,
        "kind": m.kind,
        "origin": m.origin,
        "files": m.files,
        "bytes": m.bytes,
        "overridden": m.overridden,
        "content_hash": m.content_hash.to_hex(),
    })
}

fn conflict_json(c: &ModConflict) -> Value {
    json!({ "path": c.path, "winner": c.winner, "overridden": c.overridden })
}

fn empty() -> ModsVfs {
    ModsVfs::new()
}

/// `mods.list`.
pub(crate) fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let with_conflicts = match params.get("conflicts") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`conflicts` must be a boolean, got {other}"
            )));
        }
    };
    let fallback = empty();
    let mods = world.get_resource::<ModsVfs>().unwrap_or(&fallback);
    let conflicts: Vec<Value> = if with_conflicts {
        mods.conflicts()
            .iter()
            .take(MAX_LISTED_CONFLICTS)
            .map(conflict_json)
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "order": mods.order(),
        "mods": mods.mods().iter().map(info_json).collect::<Vec<_>>(),
        "file_count": mods.file_count(),
        "conflict_count": mods.conflicts().len(),
        "conflicts": conflicts,
        "conflicts_truncated": with_conflicts && mods.conflicts().len() > MAX_LISTED_CONFLICTS,
        "content_set_hash": mods.content_set_hash().to_hex(),
    }))
}

/// Confines a `mods.mount` path like `replay.load`'s (ADR-1965).
fn confine_mount_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    let root = crate::req_path::project_root(world);
    let mut extra = Vec::new();
    if let Some(permissions) = world.get_resource::<crate::safety::AgentPermissions>() {
        extra.extend(permissions.config.output_roots.iter().cloned());
        extra.extend(permissions.config.replay_roots.iter().cloned());
    }
    crate::render_out::resolve_under_roots(&root, &extra, std::path::Path::new(path)).map_err(|e| {
        let mut refused = crate::req_path::refused_with(
            "`path` must be relative to the project root or an absolute path under an allowed root \
             (project root, system temp dir, `output_roots` or `replay_roots` in \
             .regia/agent.toml), without `..`",
            e.code(),
            "use a path relative to the project, or list the mods folder in `replay_roots` \
             (.regia/agent.toml); see book/agents/mods.md",
        );
        crate::req_path::name_allowed_roots(&mut refused, MOUNT_ROOT_KINDS);
        refused
    })
}

/// `mods.mount`.
pub(crate) fn mount(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = required_str(params, "id")?;
    let path = required_str(params, "path")?;
    let kind = match params.get("kind") {
        None | Some(Value::Null) => "auto",
        Some(Value::String(s)) if matches!(s.as_str(), "auto" | "dir" | "pak") => s.as_str(),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`kind` must be auto, dir or pak, got {other}"
            )));
        }
    };
    if path.is_empty() {
        return Err(RpcError::invalid_params(
            "`path` (a mod folder or .fpak) is required",
        ));
    }
    let host = confine_mount_path(world, path)?;
    let source = match kind {
        "dir" => ModSource::Dir(host),
        "pak" => ModSource::Pak(host),
        _ => ModSource::detect(&host).map_err(|e| mods_error(&e))?,
    };
    let mut mods = world.get_resource_or_insert_with(ModsVfs::new);
    let info = mods.mount(id, source).map_err(|e| mods_error(&e))?;
    Ok(json!({
        "mod": info_json(&info),
        "order": mods.order(),
        "conflict_count": mods.conflicts().len(),
        "content_set_hash": mods.content_set_hash().to_hex(),
    }))
}

/// `mods.unmount`.
pub(crate) fn unmount(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = required_str(params, "id")?;
    let Some(mut mods) = world.get_resource_mut::<ModsVfs>() else {
        return Err(mods_error(&ModsError {
            code: ModsErrorCode::UnknownMod,
            message: format!("no mod `{id}` is mounted"),
        }));
    };
    mods.unmount(id).map_err(|e| mods_error(&e))?;
    Ok(json!({
        "order": mods.order(),
        "content_set_hash": mods.content_set_hash().to_hex(),
    }))
}

/// `mods.order`.
pub(crate) fn order(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted: Option<Vec<String>> = match params.get("order") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| RpcError::invalid_params("`order` must be an array of mod ids"))
                })
                .collect::<Result<_, _>>()?,
        ),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`order` must be an array of mod ids, got {other}"
            )));
        }
    };
    let previous_hash;
    let (order, hash) = match wanted {
        Some(wanted) => {
            let mut mods = world.get_resource_or_insert_with(ModsVfs::new);
            previous_hash = Some(mods.content_set_hash().to_hex());
            mods.set_order(&wanted).map_err(|e| mods_error(&e))?;
            (mods.order(), mods.content_set_hash().to_hex())
        }
        None => {
            previous_hash = None;
            let fallback = empty();
            let mods = world.get_resource::<ModsVfs>().unwrap_or(&fallback);
            (mods.order(), mods.content_set_hash().to_hex())
        }
    };
    Ok(json!({ "order": order, "content_set_hash": hash, "previous_hash": previous_hash }))
}

/// `mods.hash`.
pub(crate) fn hash(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let publish = match params.get("publish") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`publish` must be a boolean, got {other}"
            )));
        }
    };
    let fallback = empty();
    let mods = world.get_resource::<ModsVfs>().unwrap_or(&fallback);
    let (key, value) = mods.lobby_entry();
    let per_mod: Vec<Value> = mods
        .mods()
        .iter()
        .map(|m| json!({ "id": m.id, "content_hash": m.content_hash.to_hex() }))
        .collect();
    let mut out = json!({
        "content_set_hash": value,
        "lobby_key": key,
        "mods": per_mod,
    });
    if publish {
        let published = match world.get_resource_mut::<regia_net::lobby::Lobbies>() {
            None => json!({ "ok": false, "error": "no Lobbies resource (add regia_net's LobbyPlugin)" }),
            Some(mut lobbies) => match lobbies.set_data(key, Some(&value)) {
                Ok(()) => json!({ "ok": true, "lobby": lobbies.current().map(|l| l.0.to_string()) }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            },
        };
        out["published"] = published;
    }
    Ok(out)
}

/// `mods.read`.
pub(crate) fn read(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = required_str(params, "path")?;
    let from = match params.get("mod") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`mod` must be a string, got {other}"
            )));
        }
    };
    let fallback = empty();
    let mods = world.get_resource::<ModsVfs>().unwrap_or(&fallback);
    let (owner, bytes) = match from {
        Some(id) => (Some(id.to_owned()), mods.read_from_mod(id, path)),
        None => (
            mods.owner(path).map_err(|e| mods_error(&e))?.map(str::to_owned),
            mods.read(path),
        ),
    };
    let bytes = bytes.map_err(|e| mods_error(&e))?;
    let text = (bytes.len() <= MAX_TEXT_BYTES)
        .then(|| std::str::from_utf8(&bytes).ok().map(str::to_owned))
        .flatten();
    Ok(json!({
        "path": path,
        "mod": owner,
        "bytes": bytes.len(),
        "blake3": blake3::hash(&bytes).to_hex().to_string(),
        "text": text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn mount_order_hash_read_and_refusals() {
        let mut world = World::new();
        let base = regia_test_utils::unique_temp_dir("agent_mods_base");
        let patch = regia_test_utils::unique_temp_dir("agent_mods_patch");
        std::fs::write(base.join("a.txt"), "base").expect("write");
        std::fs::write(patch.join("a.txt"), "patch").expect("write");
        let empty_hash = list(&mut world, &json!({})).expect("list")["content_set_hash"].clone();
        mount(
            &mut world,
            &json!({ "id": "base", "path": base.display().to_string() }),
        )
        .expect("base");
        let out = mount(
            &mut world,
            &json!({ "id": "patch", "path": patch.display().to_string(), "kind": "dir" }),
        )
        .expect("patch");
        assert_eq!(out["conflict_count"], json!(1));
        let read_out = read(&mut world, &json!({ "path": "mods://a.txt" })).expect("read");
        assert_eq!(read_out["text"], json!("patch"));
        assert_eq!(read_out["mod"], json!("patch"));
        let from_base = read(&mut world, &json!({ "path": "a.txt", "mod": "base" })).expect("read");
        assert_eq!(from_base["text"], json!("base"));
        let forward = hash(&mut world, &json!({})).expect("hash")["content_set_hash"].clone();
        assert_ne!(forward, empty_hash);
        let reordered = order(&mut world, &json!({ "order": ["patch", "base"] })).expect("order");
        assert_eq!(reordered["previous_hash"], forward);
        assert_ne!(reordered["content_set_hash"], forward);
        assert_eq!(
            read(&mut world, &json!({ "path": "a.txt" })).expect("read")["text"],
            json!("base")
        );
        let listed = list(&mut world, &json!({})).expect("list");
        assert_eq!(listed["order"], json!(["patch", "base"]));
        assert_eq!(listed["conflicts"][0]["winner"], json!("base"));
        let published = hash(&mut world, &json!({ "publish": true })).expect("hash");
        assert_eq!(published["published"]["ok"], json!(false));

        for (params, expected) in [
            (json!({ "path": "mods://../x" }), "mods_path_escape"),
            (json!({ "path": "/etc/passwd" }), "mods_path_escape"),
            (json!({ "path": "mods://nope.txt" }), "mods_not_found"),
            (json!({ "path": "a.txt", "mod": "zz" }), "mods_unknown_mod"),
            (json!({}), "F0002"),
        ] {
            let err = read(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), expected, "{params}");
        }
        let err = order(&mut world, &json!({ "order": ["base"] })).expect_err("refused");
        assert_eq!(code(&err), "mods_order_invalid");
        let err = mount(
            &mut world,
            &json!({ "id": "base", "path": base.display().to_string() }),
        )
        .expect_err("dup");
        assert_eq!(code(&err), "mods_duplicate");
        let err = mount(
            &mut world,
            &json!({ "id": "Bad", "path": base.display().to_string() }),
        )
        .expect_err("id");
        assert_eq!(code(&err), "mods_id_invalid");
        let err = mount(&mut world, &json!({ "id": "x", "path": "../../outside" })).expect_err("confined");
        assert_eq!(code(&err), "F0002");
        unmount(&mut world, &json!({ "id": "patch" })).expect("unmount");
        let err = unmount(&mut world, &json!({ "id": "patch" })).expect_err("gone");
        assert_eq!(code(&err), "mods_unknown_mod");
        let _ = std::fs::remove_dir_all(base);
        let _ = std::fs::remove_dir_all(patch);
    }
}
