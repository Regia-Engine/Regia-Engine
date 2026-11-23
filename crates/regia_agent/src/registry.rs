// SPDX-License-Identifier: MPL-2.0
//! `registry.*`: asset registry queries without loading assets (book `agents/asset-registry.md`,
//! `docs/specs/content-data.md` § 2).
//!
//! The registry is the cook's `asset_registry.bin` in the running server's cooked cache, plus the
//! `*.registry.bin` files next to mounted paks in a packaged build. It is read on the first call and
//! again when one of its files changes on disk (the live cook rewrites it after each re-cook) or the
//! call passes `refresh: true`.

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_asset::registry::{LoadedAssetRegistry, RegistryEntry, RegistryQuery, RegistryWalk, TagFilter};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Records returned when a call names no `limit`.
const DEFAULT_LIMIT: usize = 200;
/// Most records one call returns.
const MAX_LIMIT: usize = 5000;

fn not_loaded(message: impl Into<String>) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "registry_not_loaded",
        message,
        "cook the project (regia cook) so the cooked cache holds asset_registry.bin",
    )
}

fn bad_query(message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "registry_bad_query",
        message,
        "see agents/asset-registry in the book for the parameters",
    )
}

/// The loaded registry, (re)read when absent, stale or `refresh` is set.
fn loaded<'w>(world: &'w mut World, params: &Value) -> Result<&'w LoadedAssetRegistry, RpcError> {
    let refresh = crate::params::opt_bool(params, "refresh")?.unwrap_or(false);
    let current = world
        .get_resource::<LoadedAssetRegistry>()
        .is_some_and(|r| !refresh && !r.is_stale());
    if !current {
        let Some(server) = world.get_resource::<AssetServer>() else {
            return Err(not_loaded("the app has no asset server"));
        };
        let cache = server.cache_dir().to_path_buf();
        let content = server.settings_content_dir().map(std::path::Path::to_path_buf);
        let loads = world.get_resource::<LoadedAssetRegistry>().map_or(0, |r| r.loads);
        match LoadedAssetRegistry::load(&cache, content.as_deref()) {
            Ok(Some(mut fresh)) => {
                fresh.loads += loads;
                world.insert_resource(fresh);
            }
            Ok(None) => {
                world.remove_resource::<LoadedAssetRegistry>();
                return Err(not_loaded(format!(
                    "no asset registry in {} or next to the mounted paks",
                    cache.display()
                )));
            }
            Err(e) => {
                world.remove_resource::<LoadedAssetRegistry>();
                return Err(RpcError::new(
                    NOT_FOUND,
                    "registry_unreadable",
                    format!("the asset registry does not decode: {e}"),
                    "cook the project again to rewrite asset_registry.bin",
                ));
            }
        }
    }
    world
        .get_resource::<LoadedAssetRegistry>()
        .ok_or_else(|| not_loaded("the asset registry is not loaded"))
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| bad_query(format!("`{key}` must be a string"))),
    }
}

fn limit(params: &Value) -> Result<usize, RpcError> {
    match params.get("limit").filter(|v| !v.is_null()) {
        None => Ok(DEFAULT_LIMIT),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| bad_query(format!("`limit` must be a whole number in 1..={MAX_LIMIT}"))),
    }
}

fn summary(entry: &RegistryEntry) -> Value {
    json!({
        "id": entry.id.to_string(),
        "path": entry.path,
        "type_path": entry.type_path,
        "tags": entry.tags,
        "size": entry.size,
        "dependencies": entry.dependencies.len(),
    })
}

fn brief(entry: &RegistryEntry) -> Value {
    json!({ "id": entry.id.to_string(), "path": entry.path, "type_path": entry.type_path })
}

fn source_json(loaded: &LoadedAssetRegistry) -> Value {
    json!({
        "records": loaded.registry.len(),
        "generation": loaded.registry.generation,
        "loads": loaded.loads,
        "sources": loaded.sources.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
    })
}

/// The record named by the `asset` parameter (an id or a path).
fn asset<'r>(loaded: &'r LoadedAssetRegistry, params: &Value) -> Result<&'r RegistryEntry, RpcError> {
    let name = opt_str(params, "asset")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_query("`asset` (an asset id or a project-relative path) is required"))?;
    loaded.registry.resolve(name).ok_or_else(|| {
        RpcError::new(
            NOT_FOUND,
            "registry_unknown_asset",
            format!("the asset registry has no record for {name}"),
            "find paths and ids with registry.query",
        )
    })
}

/// `registry.query { path?, path_contains?, type?, tags?, limit?, refresh? }`.
pub(crate) fn query(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut query = RegistryQuery {
        path: opt_str(params, "path")?.map(str::to_string),
        path_contains: opt_str(params, "path_contains")?.map(str::to_string),
        type_path: opt_str(params, "type")?.map(str::to_string),
        tags: Vec::new(),
        limit: Some(limit(params)?),
    };
    match params.get("tags").filter(|v| !v.is_null()) {
        None => {}
        Some(Value::Array(items)) => {
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| bad_query("`tags` must be a list of \"key\" or \"key=value\" strings"))?;
                let filter = TagFilter::parse(text).map_err(|e| {
                    RpcError::new(
                        INVALID_PARAMS,
                        "registry_bad_tag",
                        e.to_string(),
                        "a tag filter is \"key\" or \"key=value\"; keys have no spaces or `=`",
                    )
                })?;
                query.tags.push(filter);
            }
        }
        Some(_) => return Err(bad_query("`tags` must be a list of strings")),
    }
    let loaded = loaded(world, params)?;
    let matches = loaded.registry.query(&query);
    Ok(json!({
        "total": matches.total,
        "returned": matches.entries.len(),
        "entries": matches.entries.iter().map(|e| summary(e)).collect::<Vec<_>>(),
        "registry": source_json(loaded),
    }))
}

/// `registry.describe { asset, refresh? }`: the full record, its dependencies and direct referencers.
pub(crate) fn describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let loaded = loaded(world, params)?;
    let entry = asset(loaded, params)?;
    let deps = loaded.registry.dependencies(entry.id, false);
    let refs = loaded.registry.referencers(entry.id, false);
    Ok(json!({
        "id": entry.id.to_string(),
        "path": entry.path,
        "type_path": entry.type_path,
        "tags": entry.tags,
        "size": entry.size,
        "dependencies": deps.entries.iter().map(|e| brief(e)).collect::<Vec<_>>(),
        "missing_dependencies": deps.missing.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "referencers": refs.entries.iter().map(|e| brief(e)).collect::<Vec<_>>(),
        "registry": source_json(loaded),
    }))
}

fn walk_json(walk: &RegistryWalk<'_>, limit: usize) -> Value {
    json!({
        "total": walk.entries.len(),
        "entries": walk.entries.iter().take(limit).map(|e| brief(e)).collect::<Vec<_>>(),
        "missing": walk.missing.iter().map(ToString::to_string).collect::<Vec<_>>(),
    })
}

/// `registry.dependencies { asset, transitive?, limit?, refresh? }`.
pub(crate) fn dependencies(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let transitive = crate::params::opt_bool(params, "transitive")?.unwrap_or(false);
    let limit = limit(params)?;
    let loaded = loaded(world, params)?;
    let entry = asset(loaded, params)?;
    let walk = loaded.registry.dependencies(entry.id, transitive);
    let mut out = walk_json(&walk, limit);
    out["asset"] = brief(entry);
    out["transitive"] = json!(transitive);
    Ok(out)
}

/// `registry.referencers { asset, transitive?, limit?, refresh? }`.
pub(crate) fn referencers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let transitive = crate::params::opt_bool(params, "transitive")?.unwrap_or(false);
    let limit = limit(params)?;
    let loaded = loaded(world, params)?;
    let entry = asset(loaded, params)?;
    let walk = loaded.registry.referencers(entry.id, transitive);
    let mut out = walk_json(&walk, limit);
    out["asset"] = brief(entry);
    out["transitive"] = json!(transitive);
    Ok(out)
}

/// `registry.tags { key?, refresh? }`: tag keys with record counts, or one key's values.
pub(crate) fn tags(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = opt_str(params, "key")?.map(str::to_string);
    let loaded = loaded(world, params)?;
    Ok(match key {
        None => json!({ "keys": loaded.registry.tag_keys(), "registry": source_json(loaded) }),
        Some(key) => json!({
            "key": key,
            "values": loaded.registry.tag_values(&key),
            "registry": source_json(loaded),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::registry::{AssetRegistry, automatic_tags, cache_registry_path};
    use regia_asset::{AssetId, AssetPlugin, AssetServerSettings, cooked_cache_dir};
    use regia_core::id::Guid;

    fn id(n: u128) -> AssetId {
        AssetId(Guid::from_u128(n))
    }

    fn rec(n: u128, path: &str, ty: &str, deps: &[u128]) -> RegistryEntry {
        let mut tags = automatic_tags(path);
        if path.contains("forest") {
            tags.insert("biome".into(), "forest".into());
        }
        RegistryEntry {
            id: id(n),
            path: path.into(),
            type_path: ty.into(),
            dependencies: deps.iter().map(|d| id(*d)).collect(),
            tags,
            size: 64,
        }
    }

    fn registry() -> AssetRegistry {
        AssetRegistry::from_entries([
            rec(1, "assets/forest/tree.glb", "regia_asset::CookedMesh", &[2]),
            rec(
                2,
                "assets/forest/tree.glb#material/Bark",
                "regia_asset::CookedStandardMaterial",
                &[3, 9],
            ),
            rec(3, "assets/textures/bark.png", "regia_asset::CookedTexture", &[]),
        ])
    }

    fn app(tag: &str, with_registry: bool) -> (App, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(&format!("agent-registry-{tag}"));
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        if with_registry {
            registry()
                .write_to(&cache_registry_path(&cache))
                .expect("registry");
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        (app, root)
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn query_filters_by_tag_and_type() {
        let (mut app, root) = app("query", true);
        let out = query(app.world_mut(), &json!({ "tags": ["biome=forest"] })).expect("query");
        assert_eq!(out["total"], 2);
        assert_eq!(out["entries"][0]["path"], "assets/forest/tree.glb");
        let out = query(app.world_mut(), &json!({ "type": "CookedTexture", "limit": 1 })).expect("query");
        assert_eq!(out["total"], 1);
        assert_eq!(out["entries"][0]["tags"]["ext"], "png");
        assert_eq!(out["registry"]["loads"], 1, "loaded once, then reused");
        let err = query(app.world_mut(), &json!({ "tags": ["bad key"] })).expect_err("bad tag");
        assert_eq!(code(&err), "registry_bad_tag");
        let err = query(app.world_mut(), &json!({ "limit": 0 })).expect_err("bad limit");
        assert_eq!(code(&err), "registry_bad_query");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walks_and_describe() {
        let (mut app, root) = app("walks", true);
        let deps = dependencies(
            app.world_mut(),
            &json!({ "asset": "forest/tree.glb", "transitive": true }),
        )
        .expect("deps");
        assert_eq!(deps["total"], 2);
        assert_eq!(deps["missing"][0], id(9).to_string());
        let refs = referencers(
            app.world_mut(),
            &json!({ "asset": id(3).to_string(), "transitive": true }),
        )
        .expect("refs");
        assert_eq!(refs["total"], 2);
        let described = describe(
            app.world_mut(),
            &json!({ "asset": "assets/forest/tree.glb#material/Bark" }),
        )
        .expect("describe");
        assert_eq!(described["referencers"][0]["path"], "assets/forest/tree.glb");
        assert_eq!(described["dependencies"][0]["path"], "assets/textures/bark.png");
        let err = describe(app.world_mut(), &json!({ "asset": "nope.png" })).expect_err("unknown");
        assert_eq!(code(&err), "registry_unknown_asset");
        let keys = tags(app.world_mut(), &json!({})).expect("keys");
        assert_eq!(keys["keys"]["biome"], 2);
        let values = tags(app.world_mut(), &json!({ "key": "ext" })).expect("values");
        assert_eq!(values["values"]["png"], 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rewritten_registry_is_read_again() {
        let (mut app, root) = app("reload", true);
        assert_eq!(query(app.world_mut(), &json!({})).expect("q")["total"], 3);
        let cache = cooked_cache_dir(&root, "linux");
        let mut more = registry();
        more.merge(AssetRegistry::from_entries([rec(
            4,
            "assets/ui/icon.png",
            "regia_asset::CookedTexture",
            &[],
        )]));
        more.write_to(&cache_registry_path(&cache)).expect("rewrite");
        let out = query(app.world_mut(), &json!({ "refresh": true })).expect("q");
        assert_eq!(out["total"], 4);
        assert_eq!(out["registry"]["loads"], 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_registry_is_a_structured_error() {
        let (mut app, root) = app("missing", false);
        let err = query(app.world_mut(), &json!({})).expect_err("none");
        assert_eq!(code(&err), "registry_not_loaded");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::write(cache_registry_path(&cache), b"garbage!garbage!").expect("bad file");
        let err = tags(app.world_mut(), &json!({})).expect_err("bad");
        assert_eq!(code(&err), "registry_unreadable");
        let mut bare = World::new();
        let err = query(&mut bare, &json!({})).expect_err("no server");
        assert_eq!(code(&err), "registry_not_loaded");
        let _ = std::fs::remove_dir_all(&root);
    }
}
