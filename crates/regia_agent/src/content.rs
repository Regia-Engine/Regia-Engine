// SPDX-License-Identifier: MPL-2.0
//! Agent methods for content chunks (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 5,
//! book/agents/content-chunks.md): list chunks and their states, request, pause, resume, cancel,
//! approve metered transfers, read the network cost, mount, unmount, release, choose a source and
//! arm deterministic faults. Errors are `F0002` with `data.reason`.

use regia_app::prelude::*;
use regia_asset::content::{
    ChunkEntry, ChunkState, ContentError, ContentFault, ContentManager, DirSource, RequestId, RequestOptions,
    Tier,
};
use regia_asset::{AssetServer, LoadSets};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most asset paths `content.chunk` lists.
const MAX_PATHS: usize = 100;

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn content_error(e: &ContentError) -> RpcError {
    refused(
        e.reason(),
        e.to_string(),
        "see book/agents/content-chunks.md for the reason codes",
    )
}

fn no_manifest() -> RpcError {
    refused(
        "manifest_missing",
        "this app has no content manifest (content-manifest.ron) or no ContentPlugin",
        "cook with `regia cook` and run the packaged game, or add ContentPlugin with a content directory",
    )
}

fn str_param<'a>(params: &'a Value, field: &str) -> Result<&'a str, RpcError> {
    crate::params::opt_str(params, field)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            refused(
                "bad_params",
                format!("`{field}` (a string) is required"),
                "see agent.schema",
            )
        })
}

fn names_param(params: &Value, field: &str) -> Result<Vec<String>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    refused(
                        "bad_params",
                        format!("`{field}` must be an array of chunk names"),
                        "",
                    )
                })
            })
            .collect(),
        Some(Value::String(one)) => Ok(vec![one.clone()]),
        Some(_) => Err(refused(
            "bad_params",
            format!("`{field}` must be an array of chunk names"),
            "",
        )),
    }
}

fn bool_param(params: &Value, field: &str) -> Result<bool, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(refused(
            "bad_params",
            format!("`{field}` must be true or false"),
            "",
        )),
    }
}

fn request_param(params: &Value) -> Result<RequestId, RpcError> {
    crate::params::opt_u64(params, "request")?
        .map(RequestId)
        .ok_or_else(|| {
            refused(
                "bad_params",
                "`request` (an integer from content.request) is required",
                "",
            )
        })
}

fn state_json(state: &ChunkState) -> Value {
    match state {
        ChunkState::Downloading { done, total } => {
            json!({ "state": "Downloading", "done": done, "total": total })
        }
        other => json!({ "state": other.label() }),
    }
}

fn with_manager<T>(
    world: &mut World,
    f: impl FnOnce(&mut ContentManager) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    let mut manager = world
        .get_resource_mut::<ContentManager>()
        .ok_or_else(no_manifest)?;
    f(&mut manager)
}

/// `content.chunks {}`: every chunk with delivery, size, assets, state, requires and mount order.
/// Without a manifest the list is empty (`manifest: false`).
pub fn chunks(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(manager) = world.get_resource::<ContentManager>() else {
        return Ok(json!({ "build_id": null, "manifest": false, "chunks": [] }));
    };
    let m = manager.manifest();
    let chunks: Vec<Value> = m
        .chunks
        .iter()
        .map(|c| {
            let state = manager.state(&c.name).unwrap_or(ChunkState::NotInstalled);
            json!({
                "name": c.name,
                "delivery": c.delivery.name(),
                "size": c.size,
                "assets": c.assets,
                "state": state.label(),
                "requires": c.requires,
                "mount_order": c.mount_order,
            })
        })
        .collect();
    Ok(json!({
        "build_id": m.build_id,
        "platform": m.platform,
        "manifest": true,
        "sources": manager.source_names(),
        "free_bytes": manager.free_space(),
        "chunks": chunks,
    }))
}

/// `content.network {}`: whether the active network is metered. `platform` is the platform's live
/// answer (Android `ConnectivityManager`; `null` when it cannot tell, as on desktop), with
/// `platform_error` when the query failed; `metered`, `probed` and `forced` are what the content
/// manager uses for transfers (`null`/`false` without a manifest). Read-only: it asks the platform but
/// does not change the manager (its probe re-reads on the next request or poll).
pub fn network(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let (platform, platform_error) = match regia_platform::network::active_network_metered() {
        Ok(answer) => (answer, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let manager = world
        .get_resource::<ContentManager>()
        .map(ContentManager::network);
    Ok(json!({
        "platform": platform,
        "platform_error": platform_error,
        "manifest": manager.is_some(),
        "metered": manager.map(|c| c.metered),
        "probed": manager.is_some_and(|c| c.probed),
        "forced": manager.is_some_and(|c| c.forced),
    }))
}

/// `content.chunk { name }`: one chunk, its first 100 asset paths and its size by asset type
/// (from the chunk's `<chunk>.registry.bin` when it is on the device).
pub fn chunk(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "name")?.to_owned();
    let manager = world.get_resource::<ContentManager>().ok_or_else(no_manifest)?;
    let entry = manager
        .manifest()
        .chunk(&name)
        .cloned()
        .ok_or_else(|| content_error(&ContentError::ChunkNotFound(name.clone())))?;
    let state = manager.state(&name).unwrap_or(ChunkState::NotInstalled);
    let ids = manager.chunk_map().ids_in(&name);
    let registry = manager.chunk_path(&name).and_then(|pak| {
        regia_asset::registry::AssetRegistry::read_from(&regia_asset::registry::pak_registry_path(pak))
            .ok()
            .flatten()
    });
    let server = world.get_resource::<AssetServer>();
    let mut by_type: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut paths = Vec::new();
    for id in &ids {
        let record = registry.as_ref().and_then(|r| r.by_id(*id));
        if let Some(rec) = record {
            let ty = rec
                .type_path
                .rsplit("::")
                .next()
                .unwrap_or(&rec.type_path)
                .to_owned();
            *by_type.entry(ty).or_default() += rec.size;
        }
        if paths.len() < MAX_PATHS {
            let path = record
                .map(|r| r.path.clone())
                .or_else(|| server.and_then(|s| s.path_for_id(*id)).map(str::to_owned))
                .unwrap_or_else(|| id.to_string());
            paths.push(path);
        }
    }
    Ok(json!({
        "name": entry.name,
        "file": entry.file,
        "delivery": entry.delivery.name(),
        "size": entry.size,
        "hash": entry.hash,
        "assets": entry.assets,
        "requires": entry.requires,
        "overrides": entry.overrides,
        "mount_order": entry.mount_order,
        "state": state_json(&state),
        "asset_paths": paths,
        "asset_paths_total": ids.len(),
        "size_by_type": by_type,
    }))
}

/// `content.why { asset }`: the chunk an asset (path or id) lives in and the cook's reason chain
/// (`rule` / `follows` / `shared` / `pinned` / `default`, the rule's selector, and the asset path
/// chain back to a direct asset) from `chunk-why.bin`. A build cooked without that file answers
/// the chunk from the chunk map with `reason: null`.
pub fn why(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let asset = str_param(params, "asset")?.to_owned();
    if !world.contains_resource::<ContentManager>() {
        return Err(no_manifest());
    }
    world.resource_scope(|world, mut manager: Mut<ContentManager>| {
        if let Some(table) = manager.why_table()
            && let Some(answer) = table.resolve(&asset).and_then(|id| table.explain(id))
        {
            let mut value = json!({
                "asset": answer.path,
                "id": answer.id.to_string(),
                "chunk": answer.chunk,
                "reason": answer.reason,
                "via": answer.via,
            });
            if let Some((chunk, index, selector)) = answer.rule {
                value["rule"] = json!({ "chunk": chunk, "index": index, "selector": selector });
            }
            if !answer.duplicated_in.is_empty() {
                value["duplicated_in"] = json!(answer.duplicated_in);
            }
            return Ok(value);
        }
        let server = world.get_resource::<AssetServer>();
        let id = asset.parse::<regia_asset::AssetId>().ok().or_else(|| {
            let path = asset.replace('\\', "/");
            let full = if path.starts_with("assets/") {
                path
            } else {
                format!("assets/{path}")
            };
            server.and_then(|s| s.id_for_path(&full))
        });
        let Some(id) = id else {
            return Err(content_error(&ContentError::AssetNotFound(asset)));
        };
        let Some(chunk) = manager.chunk_of(id) else {
            return Err(content_error(&ContentError::AssetNotFound(asset)));
        };
        Ok(json!({
            "asset": server.and_then(|s| s.path_for_id(id)).unwrap_or(asset.as_str()),
            "id": id.to_string(),
            "chunk": chunk,
            "reason": null,
            "via": [],
            "hint": "this build has no chunk-why.bin (cooked before ADR-1876); `regia assets why <asset>` prints the chain",
        }))
    })
}

/// `content.request { chunks, priority?, allow_metered?, mount_when_done? }`.
pub fn request(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let names = names_param(params, "chunks")?;
    let priority = match crate::params::opt_str(params, "priority")? {
        None => Tier::Normal,
        Some(text) => Tier::parse(text)
            .ok_or_else(|| refused("bad_params", "`priority` must be low, normal or high", ""))?,
    };
    let opts = RequestOptions {
        priority,
        allow_metered: bool_param(params, "allow_metered")?,
        mount_when_done: bool_param(params, "mount_when_done")?,
    };
    with_manager(world, |m| {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let id = m.request(&refs, opts).map_err(|e| content_error(&e))?;
        Ok(json!({ "request": id.0, "total_bytes": m.total_bytes(id) }))
    })
}

/// `content.progress { request }`.
pub fn progress(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = request_param(params)?;
    with_manager(world, |m| {
        let p = m
            .progress(id)
            .ok_or_else(|| content_error(&ContentError::RequestNotFound(id.0)))?;
        let chunks: Vec<Value> = p
            .chunks
            .iter()
            .map(|(name, state, done, total)| {
                json!({ "name": name, "state": state.label(), "done": done, "total": total })
            })
            .collect();
        Ok(json!({ "request": id.0, "state": p.state, "done": p.done, "total": p.total, "chunks": chunks }))
    })
}

fn request_op(
    world: &mut World,
    params: &Value,
    op: impl FnOnce(&mut ContentManager, RequestId) -> Result<String, ContentError>,
) -> Result<Value, RpcError> {
    let id = request_param(params)?;
    with_manager(world, |m| {
        let state = op(m, id).map_err(|e| content_error(&e))?;
        Ok(json!({ "request": id.0, "state": state }))
    })
}

/// `content.pause { request }`.
pub fn pause(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    request_op(world, params, ContentManager::pause)
}

/// `content.resume { request }`.
pub fn resume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    request_op(world, params, ContentManager::resume)
}

/// `content.cancel { request }`.
pub fn cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    request_op(world, params, ContentManager::cancel)
}

/// `content.approve_metered { request }`.
pub fn approve_metered(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    request_op(world, params, ContentManager::approve_metered)
}

fn mount_op(world: &mut World, params: &Value, mount: bool) -> Result<Value, RpcError> {
    let name = str_param(params, "chunk")?.to_owned();
    let force = bool_param(params, "force")?;
    let sets = world.get_resource::<LoadSets>().cloned();
    if !world.contains_resource::<ContentManager>() {
        return Err(no_manifest());
    }
    if !world.contains_resource::<AssetServer>() {
        return Err(refused(
            "source_not_supported",
            "this app has no asset server to mount into",
            "add AssetPlugin",
        ));
    }
    world.resource_scope(|world, mut manager: Mut<ContentManager>| {
        let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
            return Err(refused(
                "source_not_supported",
                "no asset server",
                "add AssetPlugin",
            ));
        };
        let result = if mount {
            manager.mount(&mut server, &name)
        } else {
            manager.unmount(&mut server, sets.as_ref(), &name, force)
        };
        result.map_err(|e| content_error(&e))?;
        let state = manager.state(&name).unwrap_or(ChunkState::NotInstalled);
        Ok(json!({ "chunk": name, "state": state.label() }))
    })
}

/// `content.mount { chunk }`.
pub fn mount(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    mount_op(world, params, true)
}

/// `content.unmount { chunk, force? }`.
pub fn unmount(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    mount_op(world, params, false)
}

/// `content.release { chunks, keep? }`.
pub fn release(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let names = names_param(params, "chunks")?;
    let keep = names_param(params, "keep")?;
    with_manager(world, |m| {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
        let released = m.release(&refs, &keep).map_err(|e| content_error(&e))?;
        Ok(json!({ "released": released }))
    })
}

/// `content.set_source { kind: "local" | "dir" | "http", path?, base_url? }`: `local` keeps only
/// the content directory; `dir` adds a directory laid out like a static host (tests, a mounted
/// share); `http` fetches `<base_url>/<build_id>/<file>` (`https://` when the game is built with
/// the `content-http` feature; `http://` to this machine always, to another machine only with
/// `REGIA_CONTENT_ALLOW_HTTP=1`; otherwise `source_not_supported`).
pub fn set_source(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let kind = str_param(params, "kind")?.to_owned();
    let path = crate::params::opt_str(params, "path")?.map(str::to_owned);
    // A content source directory under the allowed roots only (ADR-1816).
    let dir = match (kind.as_str(), path.as_deref()) {
        ("dir", Some(p)) if !p.is_empty() && !p.contains('\0') => {
            Some(crate::req_path::allowed_path(world, "path", p)?)
        }
        _ => None,
    };
    with_manager(world, |m| match kind.as_str() {
        "local" => {
            m.clear_sources();
            Ok(json!({ "source": "local", "sources": m.source_names() }))
        }
        "dir" => {
            let Some(dir) = dir else {
                return Err(refused(
                    "bad_params",
                    "`path` (a directory) is required for kind dir",
                    "",
                ));
            };
            m.clear_sources();
            m.add_source(Box::new(DirSource::new(dir)));
            Ok(json!({ "source": "dir", "sources": m.source_names() }))
        }
        "http" => {
            let Some(url) = crate::params::opt_str(params, "base_url")? else {
                return Err(refused("bad_params", "`base_url` is required for kind http", ""));
            };
            let source =
                regia_asset::content::HttpSource::new(url, &m.manifest().build_id).map_err(|why| {
                    refused(
                        "source_not_supported",
                        why,
                        "use https:// (content-http feature), an http:// loopback URL, or set REGIA_CONTENT_ALLOW_HTTP=1 for a LAN test host",
                    )
                })?;
            m.clear_sources();
            m.add_source(Box::new(source));
            Ok(json!({ "source": "http", "sources": m.source_names() }))
        }
        other => Err(refused(
            "bad_params",
            format!("unknown source kind {other:?}"),
            "kind is local, dir or http",
        )),
    })
}

/// `content.check_host { base_url }`: what the host offers (`<base_url>/latest.ron`, then that
/// build's manifest) against the running manifest, changing nothing. Connect and each socket read
/// time out at 30 s. On plain `http://` each response header block is also capped at 60 s. The
/// call fetches `latest.ron` and then the manifest (and a signature when the game trusts a key),
/// so use it from tools and deploy checks, not from a game loop.
pub fn check_host(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let url = str_param(params, "base_url")?.to_owned();
    let (running, trust, trust_error) = with_manager(world, |m| {
        Ok((
            m.manifest().clone(),
            m.trust().cloned(),
            m.trust_error().map(str::to_owned),
        ))
    })?;
    if let Some(why) = trust_error {
        return Err(content_error(
            &regia_asset::content::ContentError::SignatureInvalid(format!(
                "content-trust.ron did not read ({why})"
            )),
        ));
    }
    // With trusted keys the signature is checked before the offered manifest is read (ADR-2710).
    let fetched = regia_asset::content::fetch_manifest_trusted(
        &regia_asset::content::HttpClient::new(),
        &url,
        &running,
        trust.as_ref(),
    )
    .map_err(|e| content_error(&e))?;
    let offered = fetched.manifest;
    let names = |want: &dyn Fn(&ChunkEntry) -> bool| -> Vec<String> {
        offered
            .chunks
            .iter()
            .filter(|c| want(c))
            .map(|c| c.name.clone())
            .collect()
    };
    let added = names(&|c| running.chunk(&c.name).is_none());
    let changed = names(&|c| running.chunk(&c.name).is_some_and(|r| r.hash != c.hash));
    let removed: Vec<&str> = running
        .chunks
        .iter()
        .filter(|c| offered.chunk(&c.name).is_none())
        .map(|c| c.name.as_str())
        .collect();
    Ok(json!({
        "running": running.build_id,
        "offered": offered.build_id,
        "added": added,
        "changed": changed,
        "removed": removed,
        "signature": signature_value(&fetched.signature, trust.as_ref()),
    }))
}

/// `{ state, key_id?, policy?, trusted_keys }` of a signature check (ADR-2710).
fn signature_value(
    check: &regia_asset::content::SignatureCheck,
    trust: Option<&regia_asset::content::ContentTrust>,
) -> Value {
    let key_id = match check {
        regia_asset::content::SignatureCheck::Verified { key_id } => Some(key_id.as_str()),
        _ => None,
    };
    json!({
        "state": check.label(),
        "key_id": key_id,
        "policy": trust.map(|t| t.policy.label()),
        "trusted_keys": trust.map(regia_asset::content::ContentTrust::key_ids).unwrap_or_default(),
    })
}

/// `content.update { base_url? }`: fetch the manifest the host offers, check its signature against
/// the game's trusted keys (`content-trust.ron`, ADR-2710) before reading it, and adopt it; the
/// `http` sources move to the new build. `base_url` defaults to the game's content base URL. On
/// any error the running manifest is kept. It waits for the host, like `content.check_host`.
pub fn update(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let url = match crate::params::opt_str(params, "base_url")? {
        Some(url) => url.to_owned(),
        None => world
            .get_resource::<regia_asset::content::ContentSettings>()
            .and_then(|s| s.base_url.clone())
            .ok_or_else(|| {
                refused(
                    "bad_params",
                    "`base_url` is required: the game has no content base URL",
                    "pass base_url, or set [content] base_url in Regia.toml and cook",
                )
            })?,
    };
    with_manager(world, |m| {
        let update = m
            .update_from_host(&regia_asset::content::HttpClient::new(), &url)
            .map_err(|e| content_error(&e))?;
        Ok(json!({
            "previous": update.previous_build_id,
            "build_id": update.build_id,
            "changed": update.changed,
            "removed": update.removed,
            "signature": signature_value(&update.signature, m.trust()),
        }))
    })
}

/// `content.inject { chunk, fault, after_bytes? }` (debug builds): arms a deterministic fault.
pub fn inject(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !cfg!(debug_assertions) {
        return Err(refused(
            "bad_params",
            "content.inject is available in debug builds only",
            "",
        ));
    }
    let name = str_param(params, "chunk")?.to_owned();
    let fault = str_param(params, "fault")?;
    let fault = ContentFault::parse(fault).ok_or_else(|| {
        refused(
            "bad_params",
            "`fault` must be fail, stall, metered, corrupt or no_space",
            "",
        )
    })?;
    let after = crate::params::opt_u64(params, "after_bytes")?.unwrap_or(0);
    with_manager(world, |m| {
        m.inject(&name, fault, after).map_err(|e| content_error(&e))?;
        Ok(json!({ "armed": true, "chunk": name, "after_bytes": after }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::content::{ContentManifest, Delivery};

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    /// A world with a manager over a manifest whose `arena` chunk sits in a `dir` source.
    fn world_with_manager(tag: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        let content = root.join("content");
        let cdn = root.join("cdn");
        std::fs::create_dir_all(&content).expect("mkdir");
        std::fs::create_dir_all(&cdn).expect("mkdir");
        std::fs::write(content.join("base.fpak"), b"base").expect("base");
        std::fs::write(cdn.join("arena.fpak"), vec![3u8; 10_000]).expect("arena");
        let entry = |name: &str, dir: &std::path::Path, delivery| {
            let file = format!("{name}.fpak");
            ChunkEntry {
                name: name.into(),
                size: std::fs::metadata(dir.join(&file)).expect("file").len(),
                hash: regia_asset::content::hash_file(&dir.join(&file)).expect("hash"),
                file,
                delivery,
                requires: Vec::new(),
                overrides: false,
                mount_order: 0,
                assets: 0,
                blocks: Vec::new(),
            }
        };
        let mut manifest = ContentManifest::new("1-1-x", "linux");
        manifest.chunks.push(entry("base", &content, Delivery::Install));
        manifest.chunks.push(entry("arena", &cdn, Delivery::OnDemand));
        let manager = ContentManager::from_manifest(manifest, &content, &root.join("install"));
        let mut world = World::new();
        world.insert_resource(manager);
        (world, root)
    }

    /// `content.network` answers without a manifest (the platform query alone) and reports the
    /// manager's view, including an injected metered fault, when there is one.
    #[test]
    fn network_reports_the_platform_and_the_manager_view() {
        let mut world = World::new();
        let alone = network(&mut world, &json!({})).expect("network");
        assert_eq!(alone["manifest"], json!(false));
        assert_eq!(alone["metered"], Value::Null);
        #[cfg(not(target_os = "android"))]
        assert_eq!(alone["platform"], Value::Null, "desktop cannot tell");

        let (mut world, root) = world_with_manager("agent-content-network");
        let seen = network(&mut world, &json!({})).expect("network");
        assert_eq!(
            (&seen["manifest"], &seen["metered"], &seen["forced"]),
            (&json!(true), &json!(false), &json!(false))
        );
        inject(&mut world, &json!({ "chunk": "arena", "fault": "metered" })).expect("inject");
        let forced = network(&mut world, &json!({})).expect("network");
        assert_eq!(
            (&forced["metered"], &forced["forced"]),
            (&json!(true), &json!(true))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// `content.network` through the dispatcher (envelope, permission and parameter gates): it
    /// answers with the documented fields.
    #[test]
    fn network_round_trips_through_the_protocol() {
        let (mut world, root) = world_with_manager("agent-content-network-dispatch");
        let got = crate::dispatch::call_in_process(&mut world, "content.network", json!({}))
            .expect("synchronous method")
            .expect("protocol call");
        assert_eq!((&got["manifest"], &got["metered"]), (&json!(true), &json!(false)));
        assert!(
            got.get("platform").is_some() && got.get("platform_error").is_some(),
            "{got}"
        );
        // Exactly the fields the schema documents.
        let mut keys: Vec<&str> = got
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "forced",
                "manifest",
                "metered",
                "platform",
                "platform_error",
                "probed"
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn without_a_manager_the_list_is_empty_and_the_rest_refuse() {
        let mut world = World::new();
        let listed = chunks(&mut world, &json!({})).expect("list");
        assert_eq!(listed["chunks"], json!([]));
        assert_eq!(listed["manifest"], json!(false));
        let err = request(&mut world, &json!({ "chunks": ["arena"] })).expect_err("no manifest");
        assert_eq!(reason(&err), "manifest_missing");
    }

    /// ADR-8810: `content.chunks` reports a packaged chunk whose file is not the manifest's length
    /// as `Failed(size_mismatch)`.
    #[test]
    fn a_packaged_chunk_of_the_wrong_length_lists_as_size_mismatch() {
        let (world, root) = world_with_manager("agent-content-size");
        drop(world);
        // A torn copy of the packaged base: one byte more than the manifest says.
        std::fs::write(root.join("content").join("base.fpak"), b"base!").expect("grow");
        let manifest = {
            let mut m = ContentManifest::new("1-1-x", "linux");
            m.chunks.push(ChunkEntry {
                name: "base".into(),
                file: "base.fpak".into(),
                size: 4,
                hash: "0".repeat(64),
                delivery: Delivery::Install,
                requires: Vec::new(),
                overrides: false,
                mount_order: 0,
                assets: 0,
                blocks: Vec::new(),
            });
            m
        };
        let mut world = World::new();
        world.insert_resource(ContentManager::from_manifest(
            manifest,
            &root.join("content"),
            &root.join("install"),
        ));
        let listed = chunks(&mut world, &json!({})).expect("list");
        assert_eq!(listed["chunks"][0]["state"], "Failed(size_mismatch)", "{listed}");
        assert_eq!(listed["chunks"][0]["name"], "base");
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-8860: after a content update that changes the packaged base chunk's size and hash
    /// (no fetched copy yet), `content.chunks` lists it `NeedsUpdate`, not as a torn install.
    #[test]
    fn an_update_that_resizes_a_packaged_chunk_lists_as_needs_update() {
        let (mut world, root) = world_with_manager("agent-content-adopt-resize");
        let listed = chunks(&mut world, &json!({})).expect("list");
        assert_eq!(listed["chunks"][0]["state"], "Installed", "{listed}");
        let mut next = world.resource::<ContentManager>().manifest().clone();
        next.build_id = "1-2-y".into();
        for chunk in &mut next.chunks {
            if chunk.name == "base" {
                chunk.size += 100;
                chunk.hash = "1".repeat(64);
            }
        }
        world
            .resource_mut::<ContentManager>()
            .adopt_manifest(next)
            .expect("adopt");
        let listed = chunks(&mut world, &json!({})).expect("list");
        assert_eq!(listed["chunks"][0]["name"], "base");
        assert_eq!(listed["chunks"][0]["state"], "NeedsUpdate", "{listed}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn request_progress_and_the_inject_recipe() {
        let (mut world, root) = world_with_manager("agent-content");
        let listed = chunks(&mut world, &json!({})).expect("list");
        assert_eq!(listed["chunks"][1]["state"], "NotInstalled");
        let err = request(&mut world, &json!({ "chunks": ["arena"] })).expect("queued without a source");
        let id = err["request"].as_u64().expect("id");
        assert_eq!(err["total_bytes"], 10_000);
        // No dir source yet: the next step fails with source_unreachable.
        world.resource_mut::<ContentManager>().advance(1 << 20, None);
        let p = progress(&mut world, &json!({ "request": id })).expect("progress");
        assert_eq!(p["state"], "Failed(source_unreachable)");

        let cdn = root.join("cdn");
        // https:// needs the content-http feature; without it the source is refused.
        let https = set_source(
            &mut world,
            &json!({ "kind": "http", "base_url": "https://cdn.example.com" }),
        );
        if regia_asset::content::HttpClient::supports_https() {
            assert_eq!(https.expect("https")["source"], "http");
        } else {
            assert_eq!(reason(&https.expect_err("https")), "source_not_supported");
        }
        assert_eq!(
            reason(
                &set_source(&mut world, &json!({ "kind": "http", "base_url": "ftp://x" })).expect_err("ftp")
            ),
            "source_not_supported"
        );
        assert_eq!(
            reason(&set_source(&mut world, &json!({ "kind": "http" })).expect_err("no url")),
            "bad_params"
        );
        assert_eq!(
            set_source(
                &mut world,
                &json!({ "kind": "http", "base_url": "http://127.0.0.1:9/game" })
            )
            .expect("http")["source"],
            "http"
        );
        // `check_host` compares what a host offers, and refuses another platform or build.
        let host = root.join("host");
        std::fs::create_dir_all(host.join("2")).expect("mkdir");
        std::fs::write(host.join("latest.ron"), "\"2\"\n").expect("latest");
        let mut offered = world.resource::<ContentManager>().manifest().clone();
        offered.build_id = "2".into();
        offered.compatible_with.clear();
        offered.chunks.retain(|c| c.name != "arena");
        let mut new_chunk = offered.chunks[0].clone();
        new_chunk.name = "forest".into();
        // Its own file: a manifest with two chunks on one file is refused as malformed.
        new_chunk.file = "forest.fpak".into();
        offered.chunks.push(new_chunk);
        offered.chunks[0].hash = "changed".into();
        offered.write_to_dir(&host.join("2")).expect("manifest");
        let (url, _) = regia_asset::content::serve_dir_loopback(host.clone()).expect("server");
        assert_eq!(
            reason(&check_host(&mut world, &json!({ "base_url": url })).expect_err("unaccepted build")),
            "chunk_base_mismatch"
        );
        world
            .resource_mut::<ContentManager>()
            .manifest_mut()
            .compatible_with
            .push("2".into());
        let out = check_host(&mut world, &json!({ "base_url": url })).expect("offered");
        assert_eq!(out["offered"], "2");
        assert_eq!(out["added"], json!(["forest"]));
        assert_eq!(out["changed"], json!(["base"]));
        assert_eq!(out["removed"], json!(["arena"]));
        assert_eq!(
            reason(&check_host(&mut world, &json!({})).expect_err("no url")),
            "bad_params"
        );

        // The last set_source wins: the dir source with the chunks.
        set_source(
            &mut world,
            &json!({ "kind": "dir", "path": cdn.to_string_lossy() }),
        )
        .expect("source");
        inject(&mut world, &json!({ "chunk": "arena", "fault": "corrupt" })).expect("armed");
        let id = request(&mut world, &json!({ "chunks": ["arena"] })).expect("again")["request"]
            .as_u64()
            .expect("id");
        world.resource_mut::<ContentManager>().advance(1 << 20, None);
        let p = progress(&mut world, &json!({ "request": id })).expect("progress");
        assert_eq!(p["state"], "Failed(hash_mismatch)");
        assert!(
            !root.join("install/arena.fpak.part").exists(),
            "partial file removed"
        );

        let id = request(&mut world, &json!({ "chunks": ["arena"], "priority": "high" })).expect("third")["request"]
            .as_u64()
            .expect("id");
        world.resource_mut::<ContentManager>().advance(1 << 20, None);
        assert_eq!(
            progress(&mut world, &json!({ "request": id })).expect("p")["state"],
            "Installed"
        );
        let one = chunk(&mut world, &json!({ "name": "arena" })).expect("chunk");
        assert_eq!(one["state"]["state"], "Installed");
        assert_eq!(
            reason(&pause(&mut world, &json!({ "request": id })).expect_err("finished")),
            "request_finished"
        );
        assert_eq!(
            reason(&mount(&mut world, &json!({ "chunk": "arena" })).expect_err("no server")),
            "source_not_supported"
        );
        assert_eq!(
            release(&mut world, &json!({ "chunks": ["arena"] })).expect("release")["released"],
            json!(["arena"])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Spec 40 § 5 `content.mount` / `content.unmount` against a real asset server: a fetched
    /// chunk mounts (`Mounted`) and unmounts (`Installed`), `force` parses, the always-installed
    /// base refuses with `chunk_in_use`, and a chunk not installed is `chunk_not_installed`
    /// (lane spec-17-39, ADR-5636).
    #[test]
    fn mount_and_unmount_through_the_agent() {
        use regia_asset::AssetServerSettings;
        let root = regia_test_utils::unique_temp_dir("agent-content-mount");
        let content = root.join("content");
        let cdn = root.join("cdn");
        let cooked = root.join("cooked");
        for dir in [&content, &cdn, &cooked] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        // An empty cooked cache packs to a valid `base.fpak` with no assets; the arena chunk is a
        // copy of it under its own name.
        regia_asset_pipeline::pak_writer::pack_cooked_directory(
            &cooked,
            &content,
            &std::collections::BTreeMap::new(),
            false,
        )
        .expect("pak");
        std::fs::copy(content.join("base.fpak"), cdn.join("arena.fpak")).expect("copy");
        let entry = |name: &str, dir: &std::path::Path, delivery, order| {
            let file = format!("{name}.fpak");
            ChunkEntry {
                name: name.into(),
                size: std::fs::metadata(dir.join(&file)).expect("file").len(),
                hash: regia_asset::content::hash_file(&dir.join(&file)).expect("hash"),
                file,
                delivery,
                requires: Vec::new(),
                overrides: false,
                mount_order: order,
                assets: 0,
                blocks: Vec::new(),
            }
        };
        let mut manifest = ContentManifest::new("1-1-x", "linux");
        manifest
            .chunks
            .push(entry("base", &content, Delivery::Install, 0));
        manifest.chunks.push(entry("arena", &cdn, Delivery::OnDemand, 1));
        let mut manager = ContentManager::from_manifest(manifest, &content, &root.join("install"));
        manager.add_source(Box::new(regia_asset::content::DirSource::new(&cdn)));
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: Some(content.clone()),
        })
        .expect("server");
        let mut world = World::new();
        world.insert_resource(manager);
        world.insert_resource(server);

        assert_eq!(
            reason(&mount(&mut world, &json!({ "chunk": "arena" })).expect_err("not yet")),
            "chunk_not_installed"
        );
        assert_eq!(
            reason(&mount(&mut world, &json!({})).expect_err("no chunk")),
            "bad_params"
        );
        let id = request(&mut world, &json!({ "chunks": ["arena"] })).expect("request")["request"]
            .as_u64()
            .expect("id");
        world.resource_mut::<ContentManager>().advance(1 << 20, None);
        assert_eq!(
            progress(&mut world, &json!({ "request": id })).expect("p")["state"],
            "Installed"
        );
        let mounted = mount(&mut world, &json!({ "chunk": "arena" })).expect("mounts");
        assert_eq!(mounted, json!({ "chunk": "arena", "state": "Mounted" }));
        assert!(
            world
                .resource::<AssetServer>()
                .mounted_pak_names()
                .contains(&"arena".to_owned())
        );
        let unmounted = unmount(&mut world, &json!({ "chunk": "arena" })).expect("unmounts");
        assert_eq!(unmounted["state"], "Installed");
        mount(&mut world, &json!({ "chunk": "arena" })).expect("mounts again");
        let forced = unmount(&mut world, &json!({ "chunk": "arena", "force": true })).expect("force");
        assert_eq!(forced["state"], "Installed");
        assert_eq!(
            reason(&unmount(&mut world, &json!({ "chunk": "base", "force": true })).expect_err("base")),
            "chunk_in_use"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-2710: `content.update` keeps the running manifest when the game trusts keys and the
    /// host's signature is missing or malformed (both refused before any key is used, so this runs
    /// without the content-signing feature), reports the signature on `check_host`, and adopts an
    /// unsigned manifest when the game trusts no key.
    #[test]
    fn update_keeps_the_running_manifest_on_a_signature_failure() {
        use regia_asset::content::{ContentTrust, SignaturePolicy, format_public_key};
        let (mut world, root) = world_with_manager("agent-content-update");
        let host = root.join("host");
        std::fs::create_dir_all(host.join("2")).expect("mkdir");
        std::fs::write(host.join("latest.ron"), "\"2\"\n").expect("latest");
        let mut offered = world.resource::<ContentManager>().manifest().clone();
        offered.build_id = "2".into();
        offered.chunks[1].hash = "0".repeat(64);
        offered.write_to_dir(&host.join("2")).expect("manifest");
        world
            .resource_mut::<ContentManager>()
            .manifest_mut()
            .compatible_with
            .push("2".into());
        let (url, _) = regia_asset::content::serve_dir_loopback(host.clone()).expect("server");
        let trust =
            ContentTrust::new(SignaturePolicy::Required, vec![format_public_key(&[4; 32])]).expect("trust");
        world.resource_mut::<ContentManager>().set_trust(Some(trust));
        let before = world.resource::<ContentManager>().manifest().clone();
        for (method, result) in [
            ("update", update(&mut world, &json!({ "base_url": url }))),
            ("check_host", check_host(&mut world, &json!({ "base_url": url }))),
        ] {
            assert_eq!(
                reason(&result.expect_err(method)),
                "content_signature_missing",
                "{method}"
            );
        }
        std::fs::write(
            host.join("2").join(regia_asset::content::CONTENT_SIGNATURE_FILE),
            "garbage",
        )
        .expect("sig");
        let err = update(&mut world, &json!({ "base_url": url })).expect_err("malformed");
        assert_eq!(reason(&err), "content_signature_invalid");
        assert_eq!(world.resource::<ContentManager>().manifest(), &before, "kept");
        assert_eq!(
            reason(&update(&mut world, &json!({})).expect_err("no base url")),
            "bad_params"
        );
        // Trusting no key: the manifest is adopted and reported not_configured.
        world.resource_mut::<ContentManager>().set_trust(None);
        let out = check_host(&mut world, &json!({ "base_url": url })).expect("check");
        assert_eq!(out["signature"]["state"], "not_configured");
        assert_eq!(out["changed"], json!(["arena"]));
        let out = update(&mut world, &json!({ "base_url": url })).expect("adopted");
        assert_eq!(
            (out["previous"].as_str(), out["build_id"].as_str()),
            (Some("1-1-x"), Some("2"))
        );
        assert_eq!(out["changed"], json!(["arena"]));
        assert_eq!(world.resource::<ContentManager>().manifest().build_id, "2");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn why_answers_the_cook_reason_chain() {
        use regia_asset::AssetId;
        use regia_asset::content::{ChunkWhy, WhyReason, WhyRecord};
        let (mut world, root) = world_with_manager("agent-content-why");
        let level = AssetId::new_random();
        let mesh = AssetId::new_random();
        let mut records = vec![
            WhyRecord {
                id: level,
                path: "assets/levels/arena.level.ron".into(),
                chunk: 0,
                reason: WhyReason::Rule(1),
                duplicated_in: Vec::new(),
            },
            WhyRecord {
                id: mesh,
                path: "assets/arena/rock.mesh".into(),
                chunk: 0,
                reason: WhyReason::Follows(level),
                duplicated_in: vec![0, 1],
            },
        ];
        records.sort_by_key(|r| r.id);
        let table = ChunkWhy {
            chunks: vec!["arena".into(), "base".into()],
            selectors: [(
                "arena".to_owned(),
                vec!["levels/other/**".to_owned(), "levels/arena*".to_owned()],
            )]
            .into(),
            records,
        };
        let manager = world.remove_resource::<ContentManager>().expect("manager");
        world.insert_resource(manager.with_why_table(table));
        let answer = why(&mut world, &json!({ "asset": "arena/rock.mesh" })).expect("why");
        assert_eq!(answer["chunk"], "arena");
        assert_eq!(answer["reason"], "follows");
        assert_eq!(
            answer["via"],
            json!(["assets/arena/rock.mesh", "assets/levels/arena.level.ron"])
        );
        assert_eq!(
            answer["rule"],
            json!({ "chunk": "arena", "index": 1, "selector": "levels/arena*" })
        );
        assert_eq!(answer["duplicated_in"], json!(["arena", "base"]));
        let by_id = why(&mut world, &json!({ "asset": level.to_string() })).expect("by id");
        assert_eq!(by_id["reason"], "rule");
        assert_eq!(
            reason(&why(&mut world, &json!({ "asset": "nope" })).expect_err("unknown")),
            "asset_not_found"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_params_carry_reasons() {
        let (mut world, root) = world_with_manager("agent-content-bad");
        for (result, want) in [
            (request(&mut world, &json!({ "chunks": [] })), "bad_params"),
            (
                request(&mut world, &json!({ "chunks": ["nope"] })),
                "chunk_not_found",
            ),
            (
                request(&mut world, &json!({ "chunks": ["arena"], "priority": "urgent" })),
                "bad_params",
            ),
            (
                progress(&mut world, &json!({ "request": 999 })),
                "request_not_found",
            ),
            (resume(&mut world, &json!({})), "bad_params"),
            (chunk(&mut world, &json!({ "name": "nope" })), "chunk_not_found"),
            (
                why(&mut world, &json!({ "asset": "nope.png" })),
                "asset_not_found",
            ),
            (
                inject(&mut world, &json!({ "chunk": "arena", "fault": "melt" })),
                "bad_params",
            ),
            (set_source(&mut world, &json!({ "kind": "ftp" })), "bad_params"),
        ] {
            assert_eq!(reason(&result.expect_err(want)), want);
        }
        let id = request(&mut world, &json!({ "chunks": ["arena"] })).expect("ok")["request"]
            .as_u64()
            .expect("id");
        assert_eq!(
            reason(&resume(&mut world, &json!({ "request": id })).expect_err("not paused")),
            "request_not_paused"
        );
        assert_eq!(
            approve_metered(&mut world, &json!({ "request": id })).expect("ok")["request"],
            id
        );
        assert_eq!(
            cancel(&mut world, &json!({ "request": id })).expect("ok")["state"],
            "Cancelled"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
