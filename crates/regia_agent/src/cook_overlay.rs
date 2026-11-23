// SPDX-License-Identifier: MPL-2.0
//! `cook.overlay_roots`: extra asset directories a cook fills from (ADR-8155,
//! `book/agents/cook-overlay-roots.md`).
//!
//! Reports whether `[cook.overlay_roots]` or `REGIA_COOK_OVERLAY_ROOTS` is set, which of those
//! directories exist, and the last cook's `overlay_fill.json`. Read only: it never writes the
//! project. The pipeline is a desktop tool and not a dependency of this crate, so the table is
//! read here; the tests check it against the pipeline's own reader.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The error code of an invalid `[cook.overlay_roots]` table.
pub const CONFIG_INVALID: &str = "overlay_roots_config_invalid";

/// The error code of a `path` that is not a plain path relative to `assets/` (ADR-8845).
pub const PATH_INVALID: &str = "overlay_path_invalid";

/// Longest `path` accepted, in bytes.
const MAX_PATH_BYTES: usize = 1024;

/// The file the cook writes (`regia_asset_pipeline::overlay_roots::SUMMARY_FILE`).
const SUMMARY_FILE: &str = "overlay_fill.json";

/// The environment variable that replaces the configured path list
/// (`regia_asset_pipeline::overlay_roots::OVERLAY_ROOTS_ENV`).
const OVERLAY_ROOTS_ENV: &str = "REGIA_COOK_OVERLAY_ROOTS";

fn config_invalid(why: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        CONFIG_INVALID,
        why.to_string(),
        "fix [cook.overlay_roots] in Regia.toml: paths = [\"...\"]",
    )
}

#[derive(Debug)]
struct Resolved {
    paths: Vec<PathBuf>,
    missing: Vec<String>,
    configured: bool,
}

/// The pipeline's rules (`OverlayRoots::resolve`): unknown keys and bad values are errors,
/// a relative path is relative to the project, a non-directory is missing rather than an error,
/// and a non-empty environment value replaces the table's list.
fn resolve(root: &Path, manifest: Option<&str>, env: Option<&str>) -> Result<Resolved, String> {
    let mut listed = Vec::new();
    let mut configured = false;
    if let Some(text) = manifest {
        let table: toml::Table = text.parse().map_err(|error| format!("Regia.toml: {error}"))?;
        if let Some(overlay) = table
            .get("cook")
            .and_then(toml::Value::as_table)
            .and_then(|cook| cook.get("overlay_roots"))
        {
            configured = true;
            let overlay = overlay
                .as_table()
                .ok_or("Regia.toml: [cook.overlay_roots] must be a table")?;
            for key in overlay.keys() {
                if key != "paths" {
                    return Err(format!(
                        "Regia.toml: [cook.overlay_roots] has unknown key `{key}` (known: paths)"
                    ));
                }
            }
            listed = match overlay.get("paths") {
                None => Vec::new(),
                Some(toml::Value::Array(items)) => items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| match item {
                        toml::Value::String(path) if !path.trim().is_empty() => Ok(path.trim().to_string()),
                        _ => Err(format!(
                            "Regia.toml: [cook.overlay_roots] paths[{index}] must be a non-empty string"
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => {
                    return Err(
                        "Regia.toml: [cook.overlay_roots] paths must be an array of strings".to_string(),
                    );
                }
            };
        }
    }
    if let Some(raw) = env.map(str::trim).filter(|value| !value.is_empty()) {
        configured = true;
        listed = std::env::split_paths(raw)
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
    }
    if !configured {
        return Ok(Resolved {
            paths: Vec::new(),
            missing: Vec::new(),
            configured: false,
        });
    }
    let mut paths = Vec::new();
    let mut missing = Vec::new();
    for raw in listed {
        let path = PathBuf::from(&raw);
        let full = if path.is_absolute() { path } else { root.join(path) };
        if full.is_dir() {
            paths.push(full);
        } else {
            missing.push(raw);
        }
    }
    Ok(Resolved {
        paths,
        missing,
        configured: true,
    })
}

fn platform_tag(params: &Value) -> Result<String, RpcError> {
    let platform = match params.get("platform") {
        None | Some(Value::Null) => regia_asset::current_platform().to_string(),
        Some(Value::String(platform)) => platform.trim().to_ascii_lowercase(),
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
    Ok(platform)
}

/// The optional `path` (relative to `assets/`), checked by `regia_core::project_path::check_relative`.
fn source_path_param(params: &Value) -> Result<Option<String>, RpcError> {
    let path = match params.get("path") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(path)) => path,
        Some(_) => return Err(RpcError::invalid_params("`path` must be a string")),
    };
    let refuse = |why: String| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            PATH_INVALID,
            why,
            "give a path relative to assets/ without `..`, a drive or a leading `/` (e.g. tables/weapons.table.ron)",
        )
    };
    if path.len() > MAX_PATH_BYTES {
        return Err(refuse(format!("`path` is longer than {MAX_PATH_BYTES} bytes")));
    }
    regia_core::project_path::check_relative(path).map_err(|error| refuse(format!("`path`: {error}")))?;
    Ok(Some(path.replace('\\', "/")))
}

/// Where the runtime finds `path` (`regia_asset::SourceRoots::resolve`, ADR-8845): the project's
/// `assets/` first, then the existing overlay roots, earliest first.
fn resolved_json(root: &Path, overlays: &[PathBuf], path: &str) -> Value {
    let roots = regia_asset::SourceRoots::new(root.join("assets"), overlays.to_vec());
    match roots.resolve(path) {
        Some(found) => {
            let source = if found.overlay_index.is_some() {
                "overlay"
            } else {
                "project"
            };
            json!({
                "path": path,
                "found": true,
                "source": source,
                "root_index": found.overlay_index,
                "file": found.path.display().to_string(),
            })
        }
        None => json!({
            "path": path,
            "found": false,
            "source": Value::Null,
            "root_index": Value::Null,
            "file": Value::Null,
        }),
    }
}

/// `cook.overlay_roots { platform?, path? }`.
///
/// # Errors
/// `platform` is not a short tag; `overlay_roots_config_invalid` for a bad `Regia.toml` table;
/// `overlay_path_invalid` for a `path` that leaves `assets/`.
pub fn overlay_roots(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let platform = platform_tag(params)?;
    let source = source_path_param(params)?;
    let root = crate::req_path::project_root(world);
    let manifest = match std::fs::read_to_string(root.join("Regia.toml")) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(config_invalid(&format!("Regia.toml: {error}"))),
    };
    let env = std::env::var(OVERLAY_ROOTS_ENV).ok();
    let config = resolve(&root, manifest.as_deref(), env.as_deref()).map_err(|why| config_invalid(&why))?;
    let summary_path = regia_asset::cooked_cache_dir(&root, &platform).join(SUMMARY_FILE);
    let last = std::fs::read(&summary_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null);
    Ok(json!({
        "platform": platform,
        "configured": config.configured,
        "roots": config.paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "missing_roots": config.missing,
        "last_fill": last,
        "resolved": source.map_or(Value::Null, |path| resolved_json(&root, &config.paths, &path)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset_pipeline::overlay_roots as pipeline;

    fn world_at(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    #[test]
    fn the_table_and_the_names_agree_with_the_pipeline() {
        assert_eq!(SUMMARY_FILE, pipeline::SUMMARY_FILE);
        assert_eq!(OVERLAY_ROOTS_ENV, pipeline::OVERLAY_ROOTS_ENV);
        let root = Path::new("/proj");
        let texts = [
            "",
            "[cook]\nchunks = {}\n",
            "[cook.overlay_roots]\npaths = [\"cache\", \"/mnt/share\"]\n",
            "[cook]\noverlay_roots = { paths = [\"extra\"] }\n",
            "[cook.overlay_roots]\npaths = [\"\"]\n",
            "[cook.overlay_roots]\npaths = [1]\n",
            "[cook.overlay_roots]\npaths = \"cache\"\n",
            "[cook.overlay_roots]\nextra = 1\n",
            "[cook]\noverlay_roots = \"cache\"\n",
            "[\n",
        ];
        for text in texts {
            for env in [None, Some("from-env"), Some("   "), Some(""), Some("/abs/root")] {
                let ours = resolve(root, Some(text), env);
                let theirs = pipeline::OverlayRoots::resolve(root, Some(text), env);
                match (ours, theirs) {
                    (Ok(a), Ok(b)) => {
                        assert_eq!(a.configured, b.configured, "{text} env={env:?}");
                        assert_eq!(a.paths, b.paths, "{text} env={env:?}");
                        assert_eq!(a.missing, b.missing, "{text} env={env:?}");
                    }
                    (Err(_), Err(_)) => {}
                    (a, b) => panic!("{text} env={env:?}: {a:?} vs {b:?}"),
                }
            }
        }
        let live = regia_test_utils::TempDir::new("agent-overlay-agree");
        std::fs::create_dir_all(live.join("extra")).expect("extra");
        let text = "[cook.overlay_roots]\npaths = [\"extra\", \"missing\"]\n";
        let ours = resolve(&live, Some(text), None).expect("ours");
        let theirs = pipeline::OverlayRoots::resolve(&live, Some(text), None).expect("theirs");
        assert_eq!(ours.paths, theirs.paths);
        assert_eq!(ours.missing, theirs.missing);
    }

    #[test]
    fn stats_report_the_configuration_the_missing_roots_and_the_last_fill() {
        let root = regia_test_utils::TempDir::new("agent-overlay-stats");
        let mut world = world_at(&root);
        let off = overlay_roots(&mut world, &json!({ "platform": "linux" })).expect("stats");
        assert_eq!(off["configured"], json!(false));
        assert_eq!(off["last_fill"], Value::Null);
        assert_eq!(off["roots"], json!([]));
        assert_eq!(off["missing_roots"], json!([]));
        assert!(!root.join("assets").exists(), "stats never write the project");
        crate::contract::assert_result_named("cook.overlay_roots", &off);

        std::fs::write(
            root.join("Regia.toml"),
            "[cook.overlay_roots]\npaths = [\"missing-root\"]\n",
        )
        .expect("manifest");
        let missing = overlay_roots(&mut world, &json!({})).expect("stats");
        assert_eq!(missing["configured"], json!(true));
        assert_eq!(missing["missing_roots"], json!(["missing-root"]));
        assert_eq!(missing["roots"], json!([]));
        assert!(!root.join("missing-root").exists(), "stats never create a root");

        let summary = pipeline::OverlayFillSummary {
            configured: true,
            roots: vec!["/r1".to_string()],
            missing: vec![],
            files: 1,
            bytes: 4,
            filled: vec![pipeline::FilledPayload {
                relative_path: "props/crate.bin".to_string(),
                root_index: 1,
                bytes: 4,
            }],
            errors: vec![],
        };
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        pipeline::write_summary(&cache, &summary).expect("summary");
        let last = overlay_roots(&mut world, &json!({ "platform": "linux" })).expect("stats");
        assert_eq!(last["last_fill"]["files"], json!(1));
        assert_eq!(last["last_fill"]["filled"][0]["root_index"], json!(1));
        assert_eq!(
            last["last_fill"]["filled"][0]["relative_path"],
            json!("props/crate.bin")
        );
        crate::contract::assert_result_named("cook.overlay_roots", &last);

        std::fs::write(cache.join(pipeline::SUMMARY_FILE), b"not-json").expect("corrupt");
        let corrupt = overlay_roots(&mut world, &json!({ "platform": "linux" })).expect("stats");
        assert_eq!(corrupt["last_fill"], Value::Null);

        std::fs::write(root.join("Regia.toml"), "[cook.overlay_roots]\nextra = 1\n").expect("bad");
        let err = overlay_roots(&mut world, &json!({})).expect_err("bad table");
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(CONFIG_INVALID))
        );
        crate::contract::assert_listed("cook.overlay_roots", CONFIG_INVALID);
        assert!(overlay_roots(&mut world, &json!({ "platform": "../x" })).is_err());
        assert!(overlay_roots(&mut world, &json!({ "platform": 1 })).is_err());
    }

    #[test]
    fn path_reports_where_the_runtime_resolves_a_source() {
        let root = regia_test_utils::TempDir::new("agent-overlay-resolve");
        let mut world = world_at(&root);
        std::fs::write(
            root.join("Regia.toml"),
            "[cook.overlay_roots]\npaths = [\"layers/base\", \"layers/hd\"]\n",
        )
        .expect("manifest");
        for (rel, bytes) in [
            ("layers/base/tables/a.table.ron", "base"),
            ("layers/hd/tables/a.table.ron", "hd"),
            ("layers/hd/tables/b.table.ron", "hd"),
            ("assets/tables/c.table.ron", "project"),
            ("layers/base/tables/c.table.ron", "base"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, bytes).expect("write");
        }
        let no_path = overlay_roots(&mut world, &json!({})).expect("stats");
        assert_eq!(no_path["resolved"], Value::Null);
        let a = overlay_roots(&mut world, &json!({ "path": "tables/a.table.ron" })).expect("a");
        assert_eq!(a["resolved"]["source"], json!("overlay"));
        assert_eq!(a["resolved"]["root_index"], json!(0));
        crate::contract::assert_result_named("cook.overlay_roots", &a);
        let b = overlay_roots(&mut world, &json!({ "path": "tables\\b.table.ron" })).expect("b");
        assert_eq!(b["resolved"]["root_index"], json!(1));
        assert_eq!(b["resolved"]["path"], json!("tables/b.table.ron"));
        let c = overlay_roots(&mut world, &json!({ "path": "tables/c.table.ron" })).expect("c");
        assert_eq!(c["resolved"]["source"], json!("project"));
        assert_eq!(c["resolved"]["root_index"], Value::Null);
        let none = overlay_roots(&mut world, &json!({ "path": "tables/none.table.ron" })).expect("none");
        assert_eq!(none["resolved"]["found"], json!(false));
        for bad in [
            json!("../secret"),
            json!("/etc/passwd"),
            json!("c:x"),
            json!(""),
            json!(3),
        ] {
            assert!(
                overlay_roots(&mut world, &json!({ "path": bad })).is_err(),
                "{bad}"
            );
        }
        let err = overlay_roots(&mut world, &json!({ "path": "../x" })).expect_err("escape");
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(PATH_INVALID))
        );
        crate::contract::assert_listed("cook.overlay_roots", PATH_INVALID);
    }
}
