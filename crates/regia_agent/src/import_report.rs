// SPDX-License-Identifier: MPL-2.0
//! `import.report` (`book/agents/import-report.md`, ADR-3358): what the last cook's importers
//! made of an Aseprite (`.aseprite`/`.ase`) or LDtk (`.ldtk`) source — frames, tags, layers,
//! levels, and every feature dropped with a warning. The cook writes one entry per source into
//! `import_reports.json` in the cooked cache (`regia_asset_pipeline::import_report`, the
//! `assets.font_subset` ↔ `font_subsets.json` pattern: the runtime does not link the pipeline,
//! a test ties the two). Read-only.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::assets::no_asset_server;
use crate::protocol::RpcError;

const METHOD: &str = "import.report";
/// The cook's reports file (`regia_asset_pipeline::import_report::IMPORT_REPORTS_FILE`; the
/// runtime does not link the pipeline, a test ties the two).
const IMPORT_REPORTS_FILE: &str = "import_reports.json";
/// The newest report document this build reads.
const IMPORT_REPORTS_VERSION: u64 = 1;

/// Source extensions this method reports on.
const REPORTED_EXTENSIONS: [&str; 3] = ["aseprite", "ase", "ldtk"];

fn bad_param(message: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "import_bad_path",
        format!("{METHOD}: {message}"),
        "pass a project-relative path such as assets/sprites/hero.aseprite or assets/world.ldtk",
    )
}

/// `import.report { path }`: the import report of one Aseprite/LDtk source from the last cook.
///
/// `path` is the project-relative source path (`assets/...`). The answer is `{ path, importer,
/// data, warnings }`: `data` lists frames, tags, layers, clips, levels and tilesets
/// (importer-specific), and `warnings` names every feature the importer dropped or adjusted.
/// Errors: `F0002` (`import_bad_path`: not a project-relative path of a reported extension),
/// `F0003` (`import_no_report`: the cooked cache has no report for the path — cook first),
/// `F0001` (`import_report_invalid`: the report file does not parse at the expected version).
pub fn import_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_param("`path` must be a non-empty string"))?;
    let normalised = path.replace('\\', "/");
    if normalised.starts_with('/')
        || normalised.split('/').any(|part| part == "..")
        || normalised.contains(':')
    {
        return Err(bad_param(format!("`{path}` must stay inside the project")));
    }
    let extension = normalised
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()))
        .unwrap_or_default();
    if !REPORTED_EXTENSIONS.contains(&extension.as_str()) {
        return Err(bad_param(format!(
            "`{path}` is not an Aseprite or LDtk source ({})",
            REPORTED_EXTENSIONS.join(", ")
        )));
    }
    let Some(server) = world.get_resource::<regia_asset::AssetServer>() else {
        return Err(no_asset_server());
    };
    let report_path = server.cache_dir().join(IMPORT_REPORTS_FILE);
    let bytes = std::fs::read(&report_path).map_err(|_| {
        RpcError::new(
            crate::protocol::NOT_FOUND,
            "import_no_report",
            format!("no {IMPORT_REPORTS_FILE} in the cooked cache"),
            "cook the project first (regia cook); a package does not carry this report",
        )
    })?;
    let document: Value = serde_json::from_slice(&bytes).map_err(|e| {
        RpcError::new(
            crate::protocol::INTERNAL_ERROR,
            "import_report_invalid",
            format!("{IMPORT_REPORTS_FILE} does not parse: {e}"),
            "cook the project again with this build",
        )
    })?;
    let version = document.get("format_version").and_then(Value::as_u64);
    if version.is_none_or(|v| v > IMPORT_REPORTS_VERSION) {
        return Err(RpcError::new(
            crate::protocol::INTERNAL_ERROR,
            "import_report_invalid",
            format!("unexpected format_version {version:?}"),
            "cook the project again with this build",
        ));
    }
    let entries = document
        .get("entries")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RpcError::new(
                crate::protocol::INTERNAL_ERROR,
                "import_report_invalid",
                format!("{IMPORT_REPORTS_FILE} has no `entries` object"),
                "cook the project again with this build",
            )
        })?;
    let found = entries.get_key_value(&normalised).or_else(|| {
        entries
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&normalised))
    });
    let Some((found_key, entry)) = found else {
        let known = entries.len();
        return Err(RpcError::new(
            crate::protocol::NOT_FOUND,
            "import_no_report",
            format!("the last cook has no import report for `{normalised}`"),
            if known == 0 {
                "cook the project first (regia cook); a package does not carry this report".to_string()
            } else {
                format!("the report holds {known} sources; pass one of their paths")
            },
        ));
    };
    let importer = entry
        .get("importer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(json!({
        "path": found_key,
        "importer": importer,
        "data": entry.get("data").cloned().unwrap_or(Value::Null),
        "warnings": entry.get("warnings").and_then(Value::as_array).cloned().unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{AssetPlugin, AssetServerSettings};

    /// An app with an AssetServer over a cooked cache carrying one report document.
    fn app_with_report(entries: serde_json::Value) -> (App, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir("import-report-agent");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        std::fs::write(
            cache.join(IMPORT_REPORTS_FILE),
            serde_json::to_vec(&serde_json::json!({
                "format_version": 1,
                "entries": entries,
            }))
            .expect("serialize"),
        )
        .expect("report file");
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

    fn code(error: &RpcError) -> String {
        serde_json::to_value(error)
            .ok()
            .and_then(|v| v.pointer("/data/code").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default()
    }

    #[test]
    fn reports_the_entry_and_warnings_of_a_source() {
        use regia_asset_pipeline::import_report as pipeline;
        // The constants are written down twice; the test ties them.
        assert_eq!(pipeline::IMPORT_REPORTS_FILE, IMPORT_REPORTS_FILE);
        assert_eq!(
            u64::from(pipeline::IMPORT_REPORTS_VERSION),
            IMPORT_REPORTS_VERSION
        );
        let (mut app, root) = app_with_report(serde_json::json!({
            "assets/hero.aseprite": {
                "importer": "aseprite",
                "data": { "frame_count": 2, "tags": [{ "name": "idle" }] },
                "warnings": ["layer `ref` is a reference layer"],
            },
        }));
        let answer = import_report(
            app.world_mut(),
            &serde_json::json!({ "path": "assets/hero.aseprite" }),
        )
        .expect("the report answers");
        assert_eq!(answer["importer"], "aseprite");
        assert_eq!(answer["data"]["frame_count"], 2);
        assert_eq!(answer["warnings"][0], "layer `ref` is a reference layer");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn windows_separators_and_extension_case_are_accepted() {
        let (mut app, root) = app_with_report(serde_json::json!({
            "assets/world.ldtk": {
                "importer": "ldtk",
                "data": { "levels": [] },
                "warnings": [],
            },
        }));
        let answer = import_report(
            app.world_mut(),
            &serde_json::json!({ "path": "assets\\WORLD.LDTK" }),
        )
        .expect("the lookup normalises separators and extension case");
        assert_eq!(answer["path"], "assets/world.ldtk");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn errors_carry_the_documented_codes() {
        let (mut app, root) = app_with_report(serde_json::json!({}));
        let world = app.world_mut();
        let err = import_report(world, &serde_json::json!({ "path": "assets/x.aseprite" }))
            .expect_err("no entry for the path");
        assert_eq!(code(&err), "import_no_report");
        let err = import_report(world, &serde_json::json!({ "path": "../x.aseprite" }))
            .expect_err("escapes the project");
        assert_eq!(code(&err), "import_bad_path");
        let err = import_report(world, &serde_json::json!({ "path": "assets/x.png" }))
            .expect_err("not a reported extension");
        assert_eq!(code(&err), "import_bad_path");
        let err = import_report(world, &serde_json::json!({})).expect_err("missing param");
        assert_eq!(code(&err), "import_bad_path");
        // A damaged report file is an internal error.
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::write(cache.join(IMPORT_REPORTS_FILE), b"{ nope").expect("damage");
        let err =
            import_report(world, &serde_json::json!({ "path": "assets/x.aseprite" })).expect_err("damaged");
        assert_eq!(code(&err), "import_report_invalid");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn without_an_asset_server_the_error_says_so() {
        let mut world = World::new();
        let err = import_report(&mut world, &serde_json::json!({ "path": "assets/x.aseprite" }))
            .expect_err("no AssetServer");
        assert_eq!(code(&err), "F0016");
    }
}
