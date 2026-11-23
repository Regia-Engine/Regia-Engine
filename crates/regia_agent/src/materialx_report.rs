// SPDX-License-Identifier: MPL-2.0
//! `import.materialx_report`: what the MaterialX (`.mtlx`) importer maps, approximates and cannot
//! map in one project document (ADR-3341, `book/agents/materialx-import.md`).
//!
//! The importer lives in `regia_asset_pipeline`, a desktop tool this runtime-safe crate does not
//! depend on. The editor installs it as the [`MaterialXReporter`] resource
//! (`regia_asset_pipeline::materialx_import::agent_report`); without it the method answers
//! `materialx_importer_unavailable`. Read only: the report imports in memory and writes nothing,
//! not even a missing `.meta`.

use std::path::Path;

use regia_app::prelude::*;
use serde_json::Value;

use crate::protocol::{self, RpcError};

/// The importer behind the method: `(project root, project-relative path)` → the report body,
/// or `(error code, message)`.
pub type MaterialXReportFn = fn(&Path, &str) -> Result<Value, (&'static str, String)>;

/// The MaterialX importer the editor installs for `import.materialx_report`.
#[derive(Resource, Clone, Copy)]
pub struct MaterialXReporter(pub MaterialXReportFn);

/// Error code: no importer is installed (a game build without the editor tools).
pub const UNAVAILABLE: &str = "materialx_importer_unavailable";

/// Error code: `path` does not name a `.mtlx` file.
pub const NOT_MTLX: &str = "mtlx_wrong_extension";

/// `import.materialx_report {path}`.
///
/// # Errors
///
/// `F0002` for a missing or out-of-project `path`; [`NOT_MTLX`]; [`UNAVAILABLE`]; and the
/// importer's codes (`mtlx_io`, `mtlx_parse`, `mtlx_not_materialx`, `mtlx_version`,
/// `mtlx_structure`, `mtlx_no_material`, `mtlx_unsupported_shader`, `mtlx_cycle`, `mtlx_limit`,
/// `mtlx_settings`, `mtlx_too_large`, `mtlx_not_utf8`).
pub fn materialx_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match params.get("path") {
        Some(Value::String(p)) if !p.trim().is_empty() => p.trim().replace('\\', "/"),
        _ => {
            return Err(RpcError::invalid_params(
                "`path` (a project-relative .mtlx file) is required",
            ));
        }
    };
    let root = crate::req_path::project_root(world);
    crate::req_path::project_path_in(&root, "path", &path)?;
    if !path.to_ascii_lowercase().ends_with(".mtlx") {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            NOT_MTLX,
            format!("`{path}` is not a .mtlx file"),
            "pass a MaterialX document such as assets/materials/wood.mtlx",
        ));
    }
    let Some(reporter) = world.get_resource::<MaterialXReporter>().copied() else {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            UNAVAILABLE,
            "the MaterialX importer is not installed in this app".to_string(),
            "call it on the editor (regia editor or the headless editor), which installs the importer",
        ));
    };
    (reporter.0)(&root, &path).map_err(|(code, message)| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            code,
            message,
            "see book/agents/materialx-import.md for the error codes",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake(root: &Path, path: &str) -> Result<Value, (&'static str, String)> {
        if path.ends_with("bad.mtlx") {
            return Err(("mtlx_parse", "unclosed element".into()));
        }
        Ok(json!({ "path": path, "root_is_dir": root.as_os_str().is_empty() || root.is_dir() }))
    }

    fn code(e: &RpcError) -> Option<&str> {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn the_report_needs_an_importer_a_project_path_and_a_mtlx_file() {
        let mut world = World::new();
        let missing = materialx_report(&mut world, &json!({})).expect_err("no path");
        assert_eq!(missing.code, protocol::INVALID_PARAMS);
        let outside = materialx_report(&mut world, &json!({ "path": "../x.mtlx" })).expect_err("outside");
        assert_eq!(code(&outside), Some("F0002"));
        let not_mtlx = materialx_report(&mut world, &json!({ "path": "assets/a.png" })).expect_err("png");
        assert_eq!(code(&not_mtlx), Some(NOT_MTLX));
        let none =
            materialx_report(&mut world, &json!({ "path": "assets/a.mtlx" })).expect_err("no importer");
        assert_eq!(code(&none), Some(UNAVAILABLE));

        world.insert_resource(MaterialXReporter(fake));
        let body = materialx_report(&mut world, &json!({ "path": "assets\\m\\a.MTLX" })).expect("report");
        assert_eq!(body["path"], "assets/m/a.MTLX");
        let bad = materialx_report(&mut world, &json!({ "path": "assets/bad.mtlx" })).expect_err("bad");
        assert_eq!(code(&bad), Some("mtlx_parse"));
        assert!(bad.message.contains("unclosed"));
    }
}
