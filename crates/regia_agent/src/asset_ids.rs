// SPDX-License-Identifier: MPL-2.0
//! `assets.ids_export { out?, dry_run? }` and `assets.ids_import { file, dry_run? }` (ADR-8460,
//! `book/agents/asset-ids.md`, guide topic `asset-ids`): the asset id manifest `.assetids.ron` v1
//! of `regia_asset::asset_ids`. Export records the id of every `.meta` sidecar under the
//! project's `assets/`; import writes the sidecars a manifest records that are missing and never
//! replaces one that names another id (`asset_id_conflict`, nothing written). The agent surface of
//! `regia assets ids export | import`.

use regia_app::prelude::*;
use serde_json::{Value, json};

use regia_asset::asset_ids::{
    AssetIdsError, AssetIdsManifest, DEFAULT_ASSET_IDS_FILE, export_asset_ids, import_asset_ids,
    write_asset_ids,
};

use crate::protocol::{self, RpcError};

const EXPORT: &str = "assets.ids_export";
const IMPORT: &str = "assets.ids_import";

/// The protocol error for a module error: its lowercase code, with the conflicts in `data`.
fn rpc_error(method: &str, error: &AssetIdsError) -> RpcError {
    let (status, hint) = match error {
        AssetIdsError::Conflict(_) => (
            protocol::INVALID_PARAMS,
            "nothing was written: fix or remove the listed sidecars, or the manifest rows (data.conflicts)",
        ),
        AssetIdsError::Unreadable { .. } => (
            protocol::NOT_FOUND,
            "check the manifest path (project-relative); `assets.ids_export` writes one",
        ),
        AssetIdsError::Invalid(_) | AssetIdsError::UnsupportedVersion { .. } => (
            protocol::INVALID_PARAMS,
            "the manifest must be .assetids.ron v1: safe /-separated paths under assets/, each once, non-nil ids",
        ),
        AssetIdsError::BadMeta { .. } => (
            protocol::INVALID_PARAMS,
            "fix or delete the named .meta (a cook writes a new one), then export again",
        ),
        AssetIdsError::Write { .. } => (
            protocol::INTERNAL_ERROR,
            "check the folder is writable and the disk has space",
        ),
    };
    let mut err = RpcError::new(status, error.code(), format!("{method}: {error}"), hint);
    if let (AssetIdsError::Conflict(report), Some(Value::Object(data))) = (error, err.data.as_mut()) {
        data.insert(
            "conflicts".to_string(),
            serde_json::to_value(&report.conflicts).unwrap_or(Value::Null),
        );
    }
    err
}

/// `assets.ids_export { out?, dry_run? }`: writes the manifest (default
/// `asset_ids.assetids.ron` at the project root) and answers `{ file, written, entries, rows }`;
/// `rows` lists `{ path, id, importer }` with `dry_run`, which writes nothing.
///
/// # Errors
/// - `F0002`: a parameter has the wrong type; `path_outside_project` for an `out` outside the
///   project.
/// - `asset_ids_bad_meta`: a sidecar under `assets/` does not parse.
/// - `asset_ids_write_failed`: the manifest could not be written.
pub fn ids_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let out_rel = crate::params::opt_str(params, "out")?
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_ASSET_IDS_FILE)
        .to_string();
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let root = crate::req_path::project_root(world);
    let out = crate::req_path::project_path_in(&root, "out", &out_rel)?;
    let manifest = export_asset_ids(&root.join("assets")).map_err(|e| rpc_error(EXPORT, &e))?;
    if !dry_run {
        write_asset_ids(&out, &manifest).map_err(|e| rpc_error(EXPORT, &e))?;
    }
    Ok(json!({
        "file": out_rel,
        "written": !dry_run,
        "entries": manifest.entries.len(),
        "rows": if dry_run {
            manifest
                .entries
                .iter()
                .map(|e| json!({ "path": e.path, "id": e.id.to_string(), "importer": e.importer }))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        },
    }))
}

/// `assets.ids_import { file, dry_run? }`: writes the manifest's missing sidecars and answers
/// `{ file, applied, written, kept, missing_source, conflicts }`.
///
/// # Errors
/// - `F0002`: `file` is missing or a parameter has the wrong type; `path_outside_project` for a
///   `file` outside the project.
/// - `asset_ids_unreadable`: the manifest does not read.
/// - `asset_ids_invalid`: the manifest does not parse, is newer, or breaks a rule.
/// - `asset_id_conflict`: a source already has a sidecar with another id or an unreadable one;
///   nothing was written, `data.conflicts` lists them.
/// - `asset_ids_write_failed`: a sidecar could not be written.
pub fn ids_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let file_rel = crate::params::opt_str(params, "file")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "{IMPORT}: `file` (a project-relative .assetids.ron) is required"
            ))
        })?
        .to_string();
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let root = crate::req_path::project_root(world);
    let file = crate::req_path::project_path_in(&root, "file", &file_rel)?;
    let bytes =
        crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES).map_err(|source| {
            rpc_error(
                IMPORT,
                &AssetIdsError::Unreadable {
                    path: file_rel.clone().into(),
                    source,
                },
            )
        })?;
    let manifest = std::str::from_utf8(&bytes)
        .map_err(|e| AssetIdsError::Invalid(format!("not UTF-8: {e}")))
        .and_then(AssetIdsManifest::from_ron)
        .map_err(|e| rpc_error(IMPORT, &e))?;
    let report =
        import_asset_ids(&root.join("assets"), &manifest, dry_run).map_err(|e| rpc_error(IMPORT, &e))?;
    Ok(json!({
        "file": file_rel,
        "applied": report.applied,
        "written": report.written,
        "kept": report.kept,
        "missing_source": report.missing_source,
        "conflicts": report.conflicts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{AgentInbox, process_requests};
    use crate::protocol::{Request, Response};
    use crate::safety::{AgentConfig, AgentPermissions};
    use crate::server::Inbound;
    use regia_asset::{AssetId, MetaSidecar, meta_path_for};

    /// One request through the real dispatcher.
    fn call(world: &mut World, method: &str, params: Value) -> Response {
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: reply_tx,
            })
            .expect("queue request");
        world.insert_resource(AgentInbox(inbox_rx));
        process_requests(world);
        // Dispatch is synchronous; no clock or sleeping is needed to observe its reply.
        reply_rx.try_recv().expect("dispatcher answered")
    }

    fn code(response: &Response) -> Option<String> {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d["code"].as_str())
            .map(str::to_string)
    }

    fn app(project: &std::path::Path) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .insert_resource(crate::AgentSettings {
                project_dir: Some(project.to_path_buf()),
            })
            .init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>();
        app
    }

    fn source(project: &std::path::Path, rel: &str, importer: &str) -> AssetId {
        let path = project.join("assets").join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"source").expect("source");
        let id = AssetId::new_random();
        std::fs::write(
            meta_path_for(&path),
            MetaSidecar::new(importer, id).to_ron_string().expect("meta"),
        )
        .expect("meta");
        id
    }

    #[test]
    fn ids_export_and_import_through_the_dispatcher() {
        let project = regia_test_utils::unique_temp_dir("agent-asset-ids");
        let rock = source(&project, "textures/rock.png", "png");
        let crate_id = source(&project, "models/crate.glb", "gltf");
        let mut app = app(&project);
        let world = app.world_mut();

        // Export: a dry run lists the rows and writes nothing; then the file.
        let dry = call(world, EXPORT, json!({ "dry_run": true }));
        let dry = dry.result.expect("dry run");
        assert_eq!(dry["entries"], 2);
        assert_eq!(dry["rows"][0]["path"], "models/crate.glb");
        assert_eq!(dry["rows"][0]["id"], crate_id.to_string());
        assert!(!project.join(DEFAULT_ASSET_IDS_FILE).exists());
        let done = call(world, EXPORT, json!({ "out": "ids/game.assetids.ron" }));
        assert!(done.error.is_none(), "{:?}", done.error);
        let file = project.join("ids/game.assetids.ron");
        let first = std::fs::read(&file).expect("manifest");
        let again = call(world, EXPORT, json!({ "out": "ids/game.assetids.ron" }));
        assert!(again.error.is_none());
        assert_eq!(
            std::fs::read(&file).expect("manifest"),
            first,
            "export is byte-stable"
        );

        // Import after the sidecars are lost: both come back with their ids.
        for rel in ["textures/rock.png", "models/crate.glb"] {
            std::fs::remove_file(meta_path_for(&project.join("assets").join(rel))).expect("rm");
        }
        let imported = call(world, IMPORT, json!({ "file": "ids/game.assetids.ron" }));
        let imported = imported.result.expect("import");
        assert_eq!(imported["applied"], true);
        assert_eq!(
            imported["written"],
            json!(["models/crate.glb", "textures/rock.png"])
        );
        let rock_meta =
            std::fs::read(meta_path_for(&project.join("assets/textures/rock.png"))).expect("meta");
        assert_eq!(MetaSidecar::from_ron(&rock_meta).expect("meta").id, rock);

        // asset_id_conflict: a sidecar with another id is never replaced.
        std::fs::write(
            meta_path_for(&project.join("assets/textures/rock.png")),
            MetaSidecar::new("png", AssetId::new_random())
                .to_ron_string()
                .expect("meta"),
        )
        .expect("meta");
        let conflict = call(world, IMPORT, json!({ "file": "ids/game.assetids.ron" }));
        assert_eq!(
            code(&conflict).as_deref(),
            Some("asset_id_conflict"),
            "{conflict:?}"
        );
        let data = conflict
            .error
            .as_ref()
            .and_then(|e| e.data.clone())
            .expect("data");
        assert_eq!(data["conflicts"][0]["path"], "textures/rock.png");
        assert_eq!(data["conflicts"][0]["recorded"], rock.to_string());

        // asset_ids_unreadable, asset_ids_invalid, asset_ids_bad_meta and the parameter checks.
        let missing = call(world, IMPORT, json!({ "file": "ids/none.assetids.ron" }));
        assert_eq!(code(&missing).as_deref(), Some("asset_ids_unreadable"));
        std::fs::write(
            project.join("bad.assetids.ron"),
            "(format_version: 7, entries: [])",
        )
        .expect("write");
        let newer = call(world, IMPORT, json!({ "file": "bad.assetids.ron" }));
        assert_eq!(code(&newer).as_deref(), Some("asset_ids_invalid"));
        let no_file = call(world, IMPORT, json!({}));
        assert_eq!(
            no_file.error.as_ref().map(|e| e.code),
            Some(protocol::INVALID_PARAMS)
        );
        let outside = call(world, EXPORT, json!({ "out": "../escape.assetids.ron" }));
        assert!(
            outside.error.is_some(),
            "an export outside the project was written"
        );
        assert!(!project.join("../escape.assetids.ron").exists());
        std::fs::write(meta_path_for(&project.join("assets/models/crate.glb")), b"(oops").expect("garbage");
        let bad_meta = call(world, EXPORT, json!({}));
        assert_eq!(code(&bad_meta).as_deref(), Some("asset_ids_bad_meta"));
        let _ = std::fs::remove_dir_all(&project);
    }
}
