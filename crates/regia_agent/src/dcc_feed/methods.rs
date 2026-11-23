// SPDX-License-Identifier: MPL-2.0
//! The runtime-safe `dcc_feed.*` agent methods (`specs/60` § 5, `book/agents/dcc-feed.md`).
//! `dcc_feed.bake` lives in `regia_editor::dcc_feed`: a bake needs the asset pipeline.
//!
//! The editor panel calls the same functions. Refusals are `F0002` with a `data.reason` from the
//! § 4.10 vocabulary and the table of § 5.

use std::path::PathBuf;

use regia_animation::{FeedPoseTarget, SkinnedMesh};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::prelude::FeedOwned;
use serde_json::{Value, json};

use super::apply::despawn_item;
use super::core::{CoreError, ReplayOptions};
use super::net::lock;
use super::plugin::{DccFeedRegistry, DccFeedServer, FeedItemChange, FeedItemChanged};
use super::registry::{ItemRecord, ItemState};
use super::session::SessionId;
use super::settings::{self, DccFeedSettings};
use crate::protocol::{self, RpcError};

/// Most items `dcc_feed.items` lists by default.
const DEFAULT_ITEM_LIMIT: usize = 1000;

/// `F0002` with `data.reason`.
pub fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(
        protocol::INVALID_PARAMS,
        "F0002",
        message,
        "see book/agents/dcc-feed.md for this reason",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn from_core(error: CoreError) -> RpcError {
    refused(error.code, error.message)
}

fn server(world: &World) -> Result<&DccFeedServer, RpcError> {
    world
        .get_resource::<DccFeedServer>()
        .ok_or_else(|| refused("dcc_feed_disabled", "this app has no DccFeedPlugin"))
}

fn session_param(params: &Value) -> Result<SessionId, RpcError> {
    params
        .get("session")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`session` (a session id from dcc_feed.status) is required"))
}

/// Confines a request path; the feed's vocabulary calls a refusal `path_denied`.
fn project_path(world: &World, key: &str, rel: &str) -> Result<PathBuf, RpcError> {
    crate::req_path::project_path(world, key, rel).map_err(|e| refused("path_denied", e.message))
}

/// `dcc_feed.status`: listeners, settings and sessions.
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let settings = world.resource::<DccFeedSettings>().to_json();
    let Some(server) = world.get_resource::<DccFeedServer>() else {
        return Ok(json!({ "enabled": false, "listeners": [], "sessions": [], "settings": settings }));
    };
    let listeners: Vec<Value> = server
        .listeners()
        .iter()
        .map(|l| json!({ "transport": l.transport, "address": l.address }))
        .collect();
    let registry = world.resource::<DccFeedRegistry>();
    let core = lock(server.core());
    let sessions: Vec<Value> = core
        .sessions
        .iter()
        .map(|s| {
            let c = core.counters(s.id);
            json!({
                "id": s.id,
                "client": s.client,
                "client_version": s.client_version,
                "dcc": s.dcc,
                "state": s.phase.name(),
                "transport": s.transport.name(),
                "peer": s.peer,
                "protocol_version": s.protocol_version,
                "items": registry.table.session_items(s.id),
                "bytes_in": c.bytes_in,
                "frames_in": c.frames_in,
                "frames_applied": c.frames_drained,
                "frames_coalesced": c.frames_coalesced,
                "dropped": c.dropped_overload + s.dropped_not_accepted,
                "dropped_not_accepted": s.dropped_not_accepted,
                "last_error": s.last_error,
                "close_reason": s.close_reason,
            })
        })
        .collect();
    let replay = core
        .replay_progress()
        .map(|(done, total)| json!({ "done": done, "total": total }));
    let recording = core.recording.as_ref().map(|r| {
        json!({
            "path": r.path.display().to_string(),
            "records": r.recording.records.len(),
            "session": r.session,
        })
    });
    let warnings: Vec<Value> = core
        .warnings
        .iter()
        .map(|(session, code, text)| json!({ "session": session, "code": code, "text": text }))
        .collect();
    Ok(json!({
        "enabled": !listeners.is_empty(),
        "listeners": listeners,
        "settings": settings,
        "sessions": sessions,
        "replay": replay,
        "recording": recording,
        "warnings": warnings,
        "transient_textures": registry.textures.len(),
        "transient_clips": registry.clips.len(),
    }))
}

/// `*` and `?` wildcards over a whole key. Linear two-pointer matcher with single-star
/// backtracking: at most `pattern.len() * text.len()` steps, never exponential.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    glob_match_counted(pattern, text, &mut 0)
}

/// [`glob_match`] that also counts loop steps (for the bounded-work test).
fn glob_match_counted(pattern: &[u8], text: &[u8], steps: &mut u64) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // Pattern index after the last `*`, and the text index that star currently swallows up to.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        *steps += 1;
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p + 1, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    star = Some((sp, st + 1));
                    p = sp;
                    t = st + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

fn item_json(item: &ItemRecord) -> Value {
    json!({
        "key": item.key,
        "guid": item.guid.to_string(),
        "handle": item.handle,
        "entity": item.entity.map(|e| e.to_bits()),
        "kind": item.kind.name(),
        "state": item.state.name(),
        "owned": true,
        "session": item.session,
        "frames_applied": item.frames_applied,
        "last_frame_id": item.last_frame_id,
        "last_source_time_ns": item.last_source_time_ns,
        "timecode": item.timecode.map(|t| format!("{:02}:{:02}:{:02}:{:02}", t.hh, t.mm, t.ss, t.ff)),
        "dropped_out_of_order": item.dropped_out_of_order,
        "detail": {
            "vertices": item.detail.vertices,
            "triangles": item.detail.triangles,
            "joints": item.detail.joints,
            "channels": item.detail.channels,
        },
    })
}

/// `dcc_feed.items`: `{ session?, kind?, state?, key_glob?, limit? }`.
pub fn items(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let server = server(world)?;
    let session = crate::params::opt_u64(params, "session")?;
    if let Some(id) = session
        && lock(server.core()).sessions.get(id).is_none()
    {
        return Err(refused("session_not_found", format!("no session {id}")));
    }
    let kind = crate::params::opt_str(params, "kind")?;
    let state = match crate::params::opt_str(params, "state")? {
        None => None,
        Some(name) => Some(ItemState::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "`state` {name:?}: expected declared, active, stalled or detached"
            ))
        })?),
    };
    let glob = crate::params::opt_str(params, "key_glob")?;
    let limit = crate::params::opt_u64(params, "limit")?
        .map_or(DEFAULT_ITEM_LIMIT, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let registry = world.resource::<DccFeedRegistry>();
    let items: Vec<Value> = registry
        .table
        .iter()
        .filter(|i| session.is_none_or(|id| i.session == Some(id)))
        .filter(|i| kind.is_none_or(|k| i.kind.name() == k))
        .filter(|i| state.is_none_or(|s| i.state == s))
        .filter(|i| glob.is_none_or(|g| glob_match(g.as_bytes(), i.key.as_bytes())))
        .take(limit)
        .map(item_json)
        .collect();
    Ok(json!({ "items": items }))
}

/// `dcc_feed.accept`: `{ session, remember? }`.
pub fn accept(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = session_param(params)?;
    let remember = crate::params::opt_bool(params, "remember")?.unwrap_or(false);
    let server = server(world)?;
    let core = server.core().clone();
    let project = server.project_dir().map(std::path::Path::to_path_buf);
    let (resync, client) = {
        let mut c = lock(&core);
        let resync = c
            .accept(id)
            .map_err(|code| refused(code, format!("session {id}: {code}")))?;
        c.flush_replies(id);
        let client = c.sessions.get(id).map(|s| s.client.clone()).unwrap_or_default();
        if remember {
            c.allow_client(&client);
        }
        (resync, client)
    };
    let mut remembered = false;
    if remember {
        let Some(dir) = project else {
            return Err(refused("internal", "no project to remember the client in"));
        };
        settings::remember_client(&dir, &client).map_err(|e| refused("internal", e))?;
        remembered = true;
    }
    Ok(json!({ "accepted": true, "resync_sent": resync, "remembered": remembered }))
}

/// `dcc_feed.disconnect`: `{ session, despawn? }`. Always answers: the kill switch (spec § 9.1).
pub fn disconnect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = session_param(params)?;
    let despawn = crate::params::opt_bool(params, "despawn")?.unwrap_or(false);
    let core = server(world)?.core().clone();
    lock(&core)
        .disconnect(id)
        .map_err(|code| refused(code, format!("session {id}: {code}")))?;
    lock(&core).flush();
    let mut items_detached = 0;
    let mut despawned = 0;
    world.resource_scope(|world, mut registry: Mut<DccFeedRegistry>| {
        let guids = registry.table.detach_session(id);
        items_detached = guids.len();
        for guid in guids {
            let entity = registry.table.get(guid).and_then(|i| i.entity);
            if despawn {
                if let Some(item) = registry.table.purge(guid) {
                    despawn_item(world, &mut registry, &item);
                    despawned += 1;
                }
                world.write_message(FeedItemChanged {
                    guid,
                    entity: None,
                    change: FeedItemChange::Removed,
                });
                continue;
            }
            if let Some(mut feed) = entity.and_then(|e| world.get_mut::<super::plugin::FeedItem>(e)) {
                feed.state = ItemState::Detached;
                feed.session = None;
            }
            world.write_message(FeedItemChanged {
                guid,
                entity,
                change: FeedItemChange::Detached,
            });
        }
    });
    Ok(json!({ "closed": true, "items_detached": items_detached, "despawned": despawned }))
}

/// Makes an item ordinary content: it leaves the table, its parts go, and the entity stays without
/// `FeedItem` and `FeedOwned` (spec § 2.4 rule 6). Returns the entity. The bake adds the rest.
pub fn mark_baked(world: &mut World, guid: Guid) -> Option<Entity> {
    let mut entity = None;
    world.resource_scope(|world, mut registry: Mut<DccFeedRegistry>| {
        let Some(item) = registry.table.mark_baked(guid) else {
            return;
        };
        let mut live = item.clone();
        live.entity = None;
        despawn_item(world, &mut registry, &live);
        if let Some(e) = item.entity.filter(|e| world.get_entity(*e).is_ok()) {
            world.entity_mut(e).remove::<(
                super::plugin::FeedItem,
                FeedOwned,
                SkinnedMesh,
                FeedPoseTarget,
                regia_render::SkinJoints,
            )>();
            entity = Some(e);
        }
        world.write_message(FeedItemChanged {
            guid,
            entity,
            change: FeedItemChange::Baked,
        });
    });
    entity
}

/// `dcc_feed.record`: `{ action: "start" | "stop", path?, session? }`.
pub fn record(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`action` (\"start\" or \"stop\") is required"))?;
    match action {
        "start" => {
            let rel = params
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`path` is required to start"))?;
            let path = project_path(world, "path", rel)?;
            let session = crate::params::opt_u64(params, "session")?;
            let core = server(world)?.core().clone();
            let mut c = lock(&core);
            c.start_recording(path.clone(), session).map_err(from_core)?;
            c.flush();
            Ok(json!({ "recording": true, "path": path.display().to_string(), "records": 0 }))
        }
        "stop" => {
            let core = server(world)?.core().clone();
            let (path, records) = lock(&core).stop_recording().map_err(from_core)?;
            Ok(json!({ "recording": false, "path": path.display().to_string(), "records": records }))
        }
        other => Err(RpcError::invalid_params(format!(
            "`action` {other:?}: expected \"start\" or \"stop\""
        ))),
    }
}

/// `dcc_feed.replay`: `{ path, ticks_per_record?, loop?, as_client? }`.
pub fn replay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let rel = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let path = project_path(world, "path", rel)?;
    let options = ReplayOptions {
        ticks_per_record: crate::params::opt_u64(params, "ticks_per_record")?
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        looping: crate::params::opt_bool(params, "loop")?.unwrap_or(false),
        as_client: crate::params::opt_str(params, "as_client")?.map(str::to_owned),
    };
    let core = server(world)?.core().clone();
    let (session, total) = lock(&core).start_replay(&path, &options).map_err(from_core)?;
    Ok(json!({ "session": session, "records_total": total }))
}

/// `dcc_feed.listen`: `{ enabled }` opens or closes the listeners (the panel's toggle).
pub fn listen(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`enabled` (a boolean) is required"))?;
    let settings = world.resource::<DccFeedSettings>().clone();
    let Some(mut server) = world.get_resource_mut::<DccFeedServer>() else {
        return Err(refused("dcc_feed_disabled", "this app has no DccFeedPlugin"));
    };
    if enabled {
        server.start(&settings).map_err(|e| refused("internal", e))?;
    } else {
        server.stop();
    }
    let listeners: Vec<Value> = server
        .listeners()
        .iter()
        .map(|l| json!({ "transport": l.transport, "address": l.address }))
        .collect();
    Ok(json!({ "enabled": enabled, "listeners": listeners }))
}

/// `dcc_feed.rotate_token`: a new token; open sessions keep running.
pub fn rotate_token(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(mut server) = world.get_resource_mut::<DccFeedServer>() else {
        return Err(refused("dcc_feed_disabled", "this app has no DccFeedPlugin"));
    };
    server.rotate_token().map_err(from_core)?;
    Ok(json!({ "rotated": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_whole_keys() {
        for (p, t, want) in [
            ("*", "Cube", true),
            ("Cu*", "Cube", true),
            ("Cu*", "Scu", false),
            ("?ube", "Cube", true),
            ("*.001", "Cube.001", true),
            ("Cube", "Cube.001", false),
            ("", "", true),
        ] {
            assert_eq!(glob_match(p.as_bytes(), t.as_bytes()), want, "{p} {t}");
        }
    }

    #[test]
    fn glob_work_is_bounded_by_pattern_times_text() {
        // Exponential for a recursive backtracker; here at most pattern * text steps.
        let pattern = format!("{}b", "*a".repeat(30));
        let text = "a".repeat(40);
        let mut steps = 0;
        assert!(!glob_match_counted(
            pattern.as_bytes(),
            text.as_bytes(),
            &mut steps
        ));
        assert!(steps <= (pattern.len() * text.len()) as u64, "{steps} steps");
        let stars = format!("{}x", "*".repeat(1000));
        let mut steps = 0;
        assert!(!glob_match_counted(stars.as_bytes(), b"Cube", &mut steps));
        assert!(steps <= (stars.len() * 4) as u64);
        assert!(glob_match(b"*a*b*", b"xaxxbx"));
        assert!(glob_match(b"a*", b"a"));
        assert!(!glob_match(b"a?", b"a"));
    }
}
