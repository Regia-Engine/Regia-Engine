// SPDX-License-Identifier: MPL-2.0
//! `assets.import_rules`: the effective import settings of one source path, with the source of
//! each field (`book/agents/import-rules.md`, ADR-3321).
//!
//! The desktop pipeline (`regia_asset_pipeline::import_rules`) resolves the same
//! `config/import_rules.ron` at cook time; this crate must not depend on it (it is desktop-only),
//! so the resolution is built here on the shared `regia_asset::import_rules` parser — the same
//! types, the same merge — and a dev-dependency test checks both sides agree on fixtures.

use std::path::{Component, Path};

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `path` is not a safe project-relative source path.
fn bad_path(path: &str) -> RpcError {
    RpcError::invalid_params(format!(
        "`path` must be a project-relative source path (no `..`, absolute or empty): {path:?}"
    ))
}

/// Whether `path` stays inside the project (no absolute prefix, no `..`).
fn is_safe_rel(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// The importer name a source extension cooks under: `audio` for sounds, `gltf` for `.glb`, the
/// extension itself otherwise. Mirrors `regia_asset_pipeline::import_rules::importer_for_ext`.
fn importer_for_ext(ext: &str) -> &str {
    if matches!(ext, "wav" | "ogg" | "flac" | "mp3") {
        "audio"
    } else if matches!(ext, "glb" | "gltf") {
        "gltf"
    } else {
        ext
    }
}

/// `assets.import_rules { path, importer? }`.
///
/// # Errors
/// - `invalid_params`: `path` is missing or unsafe, or `importer` is not a string.
/// - `invalid_config`: `config/import_rules.ron` exists but does not parse.
pub fn import_rules(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match params.get("path") {
        Some(Value::String(p)) if is_safe_rel(p) => p.clone(),
        Some(Value::String(p)) => return Err(bad_path(p)),
        Some(_) => return Err(RpcError::invalid_params("`path` must be a string")),
        None => return Err(RpcError::invalid_params("`path` is required")),
    };
    let importer_override = match params.get("importer") {
        None | Some(Value::Null) => None,
        Some(Value::String(i)) if !i.is_empty() => Some(i.clone()),
        Some(_) => return Err(RpcError::invalid_params("`importer` must be a non-empty string")),
    };
    let root = crate::req_path::project_root(world);
    let rules = regia_asset::import_rules::ImportRulesFile::for_project(&root).map_err(|e| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            e.code(),
            format!("{}: {e}", regia_asset::import_rules::IMPORT_RULES_FILE),
            "fix config/import_rules.ron; `regia assets import-rules check` lists every problem",
        )
    })?;

    // The source's .meta is authoritative for the importer name and carries the explicit
    // settings; a source without a meta uses its extension's importer and no explicit settings.
    // Resolved by the shared rule (ADR-5597): a plain `root.join` followed an in-project folder
    // link out of the project and answered a `.meta` from there. The meta is read capped and
    // regular-file only, so a pipe in its place cannot block the world thread.
    let source = crate::req_path::project_path(world, "path", &path)?;
    let meta_file = regia_asset::meta_path_for(&source);
    let meta = crate::req_path::check_derived(&root, "path", &meta_file)
        .ok()
        .and_then(|()| crate::req_path::read_file_capped(&meta_file, crate::req_path::MAX_READ_BYTES).ok())
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok());
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let importer = importer_override
        .or_else(|| meta.as_ref().map(|m| m.importer.clone()))
        .unwrap_or_else(|| importer_for_ext(&ext).to_string());
    let explicit = meta.as_ref().and_then(|m| m.settings.clone());

    let matched: Vec<(usize, &regia_asset::import_rules::ImportRule)> = match &rules {
        Some(rules) => rules.matched_rules(&path, &importer),
        None => Vec::new(),
    };
    let merged = matched.iter().fold(
        regia_asset::import_rules::SettingsObject::default(),
        |mut acc, (_, rule)| {
            if let Ok(settings) = regia_asset::import_rules::SettingsObject::parse(&rule.settings) {
                acc.merge(&settings);
            }
            acc
        },
    );
    let mut merged = merged;
    if let Some(text) = explicit.as_deref().filter(|s| !s.trim().is_empty()) {
        match regia_asset::import_rules::SettingsObject::parse(text) {
            Ok(obj) => merged.merge(&obj),
            Err(e) => {
                return Err(RpcError::new(
                    protocol::INVALID_PARAMS,
                    "meta_settings_invalid",
                    format!("{path}: .meta settings do not parse: {e}"),
                    "fix the settings string in the asset's .meta",
                ));
            }
        }
    }
    let fields: Vec<Value> = merged
        .fields
        .iter()
        .map(|(name, value)| {
            // Provenance: the explicit .meta field wins; else the last matching rule that sets it.
            let from_meta = explicit
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .and_then(|text| regia_asset::import_rules::SettingsObject::parse(text).ok())
                .is_some_and(|obj| obj.get(name).is_some());
            let source = if from_meta {
                json!("meta")
            } else {
                matched
                    .iter()
                    .rev()
                    .find(|(_, rule)| {
                        regia_asset::import_rules::SettingsObject::parse(&rule.settings)
                            .ok()
                            .is_some_and(|obj| obj.get(name).is_some())
                    })
                    .map(|(index, rule)| json!({ "rule": index, "glob": rule.glob }))
                    .unwrap_or(json!("meta"))
            };
            json!({ "field": name, "value": value, "source": source })
        })
        .collect();
    Ok(json!({
        "path": path,
        "importer": importer,
        "settings": merged.to_ron(),
        "matched_rules": matched.iter().map(|(index, rule)| json!({
            "index": index,
            "glob": rule.glob,
            "importer": rule.importer,
        })).collect::<Vec<_>>(),
        "fields": fields,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with(dir: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.to_path_buf()),
        });
        world
    }

    fn project() -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir("agent-import-rules");
        std::fs::create_dir_all(root.join("assets/ui")).expect("assets");
        std::fs::create_dir_all(root.join("config")).expect("config");
        std::fs::write(root.join("assets/ui/icon.png"), b"x").expect("source");
        std::fs::write(
            root.join(regia_asset::import_rules::IMPORT_RULES_FILE),
            "(rules: [(glob: \"assets/ui/**\", importer: \"png\", settings: \"(compress: Some(false), max_size: Some(256))\")])",
        )
        .expect("rules");
        root
    }

    fn meta_for(root: &Path, settings: Option<&str>) {
        let source = root.join("assets/ui/icon.png");
        let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
        meta.settings = settings.map(str::to_string);
        std::fs::write(
            regia_asset::meta_path_for(&source),
            meta.to_ron_string().expect("ron"),
        )
        .expect("meta");
    }

    #[test]
    fn reports_effective_settings_with_the_source_of_each_field() {
        let root = project();
        meta_for(&root, Some("(max_size: Some(1024))"));
        let mut world = world_with(&root);
        let out = import_rules(&mut world, &json!({ "path": "assets/ui/icon.png" })).expect("call");
        assert_eq!(out["importer"], "png");
        assert_eq!(out["settings"], "(compress: Some(false), max_size: Some(1024))");
        assert_eq!(out["matched_rules"][0]["index"], 0);
        let field = |name: &str| {
            out["fields"]
                .as_array()
                .expect("fields")
                .iter()
                .find(|f| f["field"] == name)
                .expect("field")
                .clone()
        };
        assert_eq!(
            field("compress")["source"],
            json!({ "rule": 0, "glob": "assets/ui/**" })
        );
        assert_eq!(field("max_size")["value"], "Some(1024)");
        assert_eq!(field("max_size")["source"], "meta");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_unsafe_paths_and_bad_configs_with_stable_codes() {
        let root = project();
        let mut world = world_with(&root);
        for path in ["../x.png", "/etc/passwd", "", "a/../../b"] {
            let err = import_rules(&mut world, &json!({ "path": path })).expect_err("unsafe");
            assert_eq!(
                err.data.as_ref().and_then(|d| d.get("code")),
                Some(&json!("F0002")),
                "{path}"
            );
        }
        std::fs::write(
            root.join(regia_asset::import_rules::IMPORT_RULES_FILE),
            "(rules: [(glob: \"\", settings: \"()\")])",
        )
        .expect("rules");
        let err = import_rules(&mut world, &json!({ "path": "assets/ui/icon.png" })).expect_err("bad config");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("invalid_config"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// corr-agent (ADR-5597): `root.join(path)` followed a folder link inside the project to a
    /// `.meta` outside it, and its importer came back to a read-only caller.
    #[cfg(unix)]
    #[test]
    fn a_folder_link_out_of_the_project_is_refused() {
        let root = project();
        let outside = regia_test_utils::unique_temp_dir("agent-import-rules-outside");
        let source = outside.join("x.png");
        std::fs::write(&source, b"x").expect("source");
        let meta = regia_asset::MetaSidecar::new("secret_importer", regia_asset::AssetId::new_random());
        std::fs::write(
            regia_asset::meta_path_for(&source),
            meta.to_ron_string().expect("ron"),
        )
        .expect("meta");
        std::os::unix::fs::symlink(&outside, root.join("assets/link")).expect("link");
        let mut world = world_with(&root);
        let err = import_rules(&mut world, &json!({ "path": "assets/link/x.png" })).expect_err("outside");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("reason")),
            Some(&json!("path_outside_project")),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn agrees_with_the_pipeline_resolution() {
        // The pipeline is a dev-dependency: the agent's own resolution must equal
        // regia_asset_pipeline::import_rules::effective_settings on the same fixture.
        let root = project();
        meta_for(&root, Some("(max_size: Some(1024))"));
        let mut world = world_with(&root);
        let out = import_rules(&mut world, &json!({ "path": "assets/ui/icon.png" })).expect("call");
        let eff = regia_asset_pipeline::import_rules::effective_settings(
            &root,
            "assets/ui/icon.png",
            "png",
            Some("(max_size: Some(1024))"),
        )
        .expect("pipeline resolution");
        assert_eq!(out["settings"], eff.settings);
        let agent_fields: Vec<(String, String)> = out["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .map(|f| {
                (
                    f["field"].as_str().expect("field").to_string(),
                    f["value"].as_str().expect("value").to_string(),
                )
            })
            .collect();
        let pipe_fields: Vec<(String, String)> = eff
            .fields
            .iter()
            .map(|f| (f.field.clone(), f.value.clone()))
            .collect();
        assert_eq!(agent_fields, pipe_fields);
        let _ = std::fs::remove_dir_all(&root);
    }
}
