// SPDX-License-Identifier: MPL-2.0
//! `cook.cache_stats`: the project's shared cook cache (ADR-2612, `book/agents/cook-cache.md`).
//!
//! Reports the configuration (`Regia.toml` `[cook.shared_cache]` or `REGIA_SHARED_COOK_CACHE`),
//! the last cook's summary (`shared_cache_summary.json`, which the cook writes beside the
//! platform's cook ledger) and, unless `scan: false`, the cache directory's entry count and bytes.
//! Read only: it never writes the cache. The pipeline is a desktop tool and not a dependency of
//! this crate, so the table and the layout are read here; the tests check them against the
//! pipeline's own reader and writer.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The error code of an invalid `[cook.shared_cache]` table.
pub const CONFIG_INVALID: &str = "shared_cache_config_invalid";

/// The file the cook writes the last summary to (`regia_asset_pipeline::cook_cache::SUMMARY_FILE`).
const SUMMARY_FILE: &str = "shared_cache_summary.json";

/// The environment variable that replaces the configured path
/// (`regia_asset_pipeline::cook_cache::SHARED_CACHE_ENV`).
const SHARED_CACHE_ENV: &str = "REGIA_SHARED_COOK_CACHE";

fn config_invalid(why: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        CONFIG_INVALID,
        why.to_string(),
        "fix [cook.shared_cache] in Regia.toml: path = \"...\", mode = \"read\" | \"read_write\"",
    )
}

/// The configured cache (path, mode) from the `Regia.toml` text and the environment value, with
/// the pipeline's rules (`SharedCacheConfig::resolve`): unknown keys and bad values are errors,
/// a relative path is relative to the project, a URL is kept as is.
fn resolve(
    root: &Path,
    manifest: Option<&str>,
    env: Option<String>,
) -> Result<Option<(String, &'static str)>, String> {
    let (mut path, mut mode) = (None, "read");
    if let Some(text) = manifest {
        let table: toml::Table = text.parse().map_err(|e| format!("Regia.toml: {e}"))?;
        if let Some(shared) = table
            .get("cook")
            .and_then(toml::Value::as_table)
            .and_then(|cook| cook.get("shared_cache"))
        {
            let shared = shared
                .as_table()
                .ok_or("Regia.toml: [cook.shared_cache] must be a table")?;
            for key in shared.keys() {
                if key != "path" && key != "mode" {
                    return Err(format!(
                        "Regia.toml: [cook.shared_cache] has unknown key `{key}` (known: path, mode)"
                    ));
                }
            }
            match shared.get("path") {
                None => {}
                Some(toml::Value::String(p)) if !p.trim().is_empty() => path = Some(p.trim().to_string()),
                Some(_) => {
                    return Err("Regia.toml: [cook.shared_cache] path must be a non-empty string".into());
                }
            }
            match shared.get("mode").map(toml::Value::as_str) {
                None => {}
                Some(Some("read")) => mode = "read",
                Some(Some("read_write")) => mode = "read_write",
                Some(Some(other)) => {
                    return Err(format!(
                        "Regia.toml: [cook.shared_cache] mode `{other}` is not `read` or `read_write`"
                    ));
                }
                Some(None) => return Err("Regia.toml: [cook.shared_cache] mode must be a string".into()),
            }
        }
    }
    if let Some(env) = env.filter(|v| !v.is_empty()) {
        path = Some(env);
    }
    Ok(path.map(|p| {
        let url = p.starts_with("http://") || p.starts_with("https://");
        let full = if url || Path::new(&p).is_absolute() {
            p
        } else {
            root.join(&p).display().to_string()
        };
        (full, mode)
    }))
}

/// Entry count and bytes of a cache directory (`<root>/v1/<aa>/<key>.rcc`).
fn scan_dir(root: &Path) -> std::io::Result<(u64, u64)> {
    if !std::fs::metadata(root)?.is_dir() {
        return Err(std::io::Error::other("not a directory"));
    }
    let (mut entries, mut bytes) = (0u64, 0u64);
    let Ok(shards) = std::fs::read_dir(root.join("v1")) else {
        return Ok((0, 0));
    };
    for shard in shards.flatten() {
        let Ok(files) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            let is_entry = name
                .strip_suffix(".rcc")
                .is_some_and(|k| k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit()));
            if is_entry && let Ok(meta) = file.metadata() {
                entries += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((entries, bytes))
}

/// `cook.cache_stats { platform?, scan? }`.
///
/// # Errors
/// `platform` is not a short tag; `shared_cache_config_invalid` for a bad `Regia.toml` table.
pub fn cache_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let platform = match params.get("platform") {
        None | Some(Value::Null) => regia_asset::current_platform().to_string(),
        Some(Value::String(p)) => p.trim().to_ascii_lowercase(),
        Some(_) => return Err(RpcError::invalid_params("`platform` must be a string")),
    };
    if platform.is_empty()
        || platform.len() > 32
        || !platform
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(RpcError::invalid_params(
            "`platform` must be a short tag of ascii letters, digits, `-` and `_` (e.g. `linux`)",
        ));
    }
    let scan = match params.get("scan") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`scan` must be a boolean")),
    };
    let root = crate::req_path::project_root(world);
    let manifest = match std::fs::read_to_string(root.join("Regia.toml")) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(config_invalid(&format!("Regia.toml: {e}"))),
    };
    let env = std::env::var(SHARED_CACHE_ENV).ok();
    let config = resolve(&root, manifest.as_deref(), env).map_err(|why| config_invalid(&why))?;
    let summary_path = regia_asset::cooked_cache_dir(&root, &platform).join(SUMMARY_FILE);
    let last = std::fs::read(&summary_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null);
    let mut out = json!({
        "platform": platform,
        "configured": config.is_some(),
        "last_cook": last,
    });
    if let Some((location, mode)) = config {
        if scan {
            let found = if location.starts_with("http://") || location.starts_with("https://") {
                Err("an http:// cache cannot be listed; run `regia cook cache stats` on the server's directory"
                    .to_string())
            } else {
                scan_dir(&PathBuf::from(&location)).map_err(|e| e.to_string())
            };
            match found {
                Ok((entries, bytes)) => {
                    out["reachable"] = json!(true);
                    out["entries"] = json!(entries);
                    out["bytes"] = json!(bytes);
                }
                Err(why) => {
                    out["reachable"] = json!(false);
                    out["unreachable_reason"] = json!(why);
                }
            }
        }
        out["location"] = json!(location);
        out["mode"] = json!(mode);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset_pipeline::cook_cache as pipeline;

    fn world_at(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    /// The names and the table rules here match the pipeline's (it is not a dependency).
    #[test]
    fn the_table_and_the_names_agree_with_the_pipeline() {
        assert_eq!(SUMMARY_FILE, pipeline::SUMMARY_FILE);
        assert_eq!(SHARED_CACHE_ENV, pipeline::SHARED_CACHE_ENV);
        assert_eq!(
            regia_asset::current_platform(),
            regia_asset_pipeline::host_platform()
        );
        let root = Path::new("/proj");
        for text in [
            "",
            "[cook]\nchunks = {}\n",
            "[cook.shared_cache]\npath = \"cache\"\nmode = \"read_write\"\n",
            "[cook]\nshared_cache = { path = \"/mnt/share\" }\n",
            "[cook.shared_cache]\npath = \"http://cache.lan/c\"\n",
            "[cook.shared_cache]\npath = \"c\"\nmode = \"write\"\n",
            "[cook.shared_cache]\npath = 5\n",
            "[cook.shared_cache]\npath = \"c\"\nsize = 1\n",
            "[cook]\nshared_cache = \"c\"\n",
            "[cook.shared_cache]\nmode = 3\n",
        ] {
            for env in [None, Some("/env/cache")] {
                let ours = resolve(root, Some(text), env.map(str::to_string));
                let theirs = pipeline::SharedCacheConfig::resolve(root, Some(text), env.map(Into::into));
                match (ours, theirs) {
                    (Ok(a), Ok(b)) => assert_eq!(
                        a.map(|(p, m)| (PathBuf::from(p), m)),
                        b.map(|c| (c.path, c.mode.label())),
                        "{text}"
                    ),
                    (Err(_), Err(_)) => {}
                    (a, b) => panic!("{text}: {a:?} vs {b:?}"),
                }
            }
        }
    }

    #[test]
    fn stats_report_the_configuration_the_entries_and_the_last_cook() {
        let root = regia_test_utils::unique_temp_dir("agent-cook-cache");
        let mut world = world_at(&root);
        // Off by default.
        let off = cache_stats(&mut world, &json!({ "platform": "linux" })).expect("stats");
        assert_eq!(off["configured"], json!(false));
        assert_eq!(off["last_cook"], Value::Null);

        let share = root.join("share");
        std::fs::write(
            root.join("Regia.toml"),
            "[cook.shared_cache]\npath = \"share\"\nmode = \"read_write\"\n",
        )
        .expect("manifest");
        // Configured but not created yet: unreachable, not an error, and nothing created.
        let missing = cache_stats(&mut world, &json!({})).expect("stats");
        assert_eq!(missing["configured"], json!(true));
        assert_eq!(missing["mode"], json!("read_write"));
        assert_eq!(missing["reachable"], json!(false));
        assert!(!share.exists(), "stats never write the cache");

        // Entries the pipeline's backend wrote are counted.
        use pipeline::CookCacheBackend;
        std::fs::create_dir_all(&share).expect("share");
        let backend = pipeline::DirBackend::open(&share, pipeline::SharedCacheMode::ReadWrite).expect("open");
        backend.put(&[1; 32], &[0; 100]).expect("put");
        backend.put(&[2; 32], &[0; 50]).expect("put");
        let on = cache_stats(&mut world, &json!({ "scan": true })).expect("stats");
        assert_eq!(
            (
                on["reachable"].clone(),
                on["entries"].clone(),
                on["bytes"].clone()
            ),
            (json!(true), json!(2), json!(150))
        );
        let quick = cache_stats(&mut world, &json!({ "scan": false })).expect("stats");
        assert!(quick.get("entries").is_none());

        // The last cook's summary, as the cook writes it.
        let summary = pipeline::SharedCacheSummary {
            location: share.display().to_string(),
            mode: "read_write".into(),
            hits: 7,
            bytes_read: 1234,
            ..Default::default()
        };
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::write(
            cache.join(pipeline::SUMMARY_FILE),
            serde_json::to_vec(&summary).expect("json"),
        )
        .expect("summary");
        let last = cache_stats(&mut world, &json!({ "platform": "linux", "scan": false })).expect("stats");
        assert_eq!(last["last_cook"]["hits"], json!(7));
        assert_eq!(last["last_cook"]["bytes_read"], json!(1234));

        std::fs::write(root.join("Regia.toml"), "[cook.shared_cache]\nmode = \"often\"\n").expect("manifest");
        let err = cache_stats(&mut world, &json!({})).expect_err("bad table");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!(CONFIG_INVALID))
        );
        assert!(cache_stats(&mut world, &json!({ "platform": "../x" })).is_err());
        assert!(cache_stats(&mut world, &json!({ "scan": 1 })).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
