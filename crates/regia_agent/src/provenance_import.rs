// SPDX-License-Identifier: MPL-2.0
//! `assets.provenance_import { records, under?, dry_run? }` (ADR-8834, `book/agents/provenance-import.md`,
//! guide topic `provenance-import`): moves pack provenance records (`provenance/<pack>.toml`) into
//! the provenance block of each source's `.meta`, through `regia_asset::provenance_import`. All or
//! nothing: any conflict writes nothing (`provenance_import_conflict`, `data.conflicts`), and a
//! second run writes nothing either. The agent surface of `regia assets provenance-import`.

use regia_app::prelude::*;
use serde_json::Value;

use regia_asset::provenance_import::{
    ImportOptions, ProvenanceImportError, import_provenance, read_pack_record, record_files,
};

use crate::protocol::{self, RpcError};

const METHOD: &str = "assets.provenance_import";

/// Most record paths one call names.
pub const MAX_RECORD_PATHS: usize = 256;

/// The protocol error for a module error: its lowercase code, with the conflicts in `data`.
fn rpc_error(error: &ProvenanceImportError) -> RpcError {
    let (status, hint) = match error {
        ProvenanceImportError::Conflict(_) => (
            protocol::INVALID_PARAMS,
            "nothing was written: fix the rows, the sources or the .meta files (data.conflicts), then run it again",
        ),
        ProvenanceImportError::Unreadable { .. } => (
            protocol::NOT_FOUND,
            "check the record paths (project-relative files, or folders of *.toml)",
        ),
        ProvenanceImportError::Invalid(_) => (
            protocol::INVALID_PARAMS,
            "fix the named record row; see book/agents/provenance-import.md for the rules",
        ),
        ProvenanceImportError::Write { .. } => (
            protocol::INTERNAL_ERROR,
            "check the folder is writable and the disk has space",
        ),
    };
    let mut err = RpcError::new(status, error.code(), format!("{METHOD}: {error}"), hint);
    if let (ProvenanceImportError::Conflict(report), Some(Value::Object(data))) = (error, err.data.as_mut()) {
        data.insert(
            "conflicts".to_string(),
            serde_json::to_value(&report.conflicts).unwrap_or(Value::Null),
        );
        data.insert(
            "duplicates".to_string(),
            serde_json::to_value(&report.duplicates).unwrap_or(Value::Null),
        );
    }
    err
}

/// `assets.provenance_import { records, under?, dry_run? }`: answers the import report
/// `{ applied, records, rows, written, kept, missing_source, missing_meta, duplicates, conflicts }`.
///
/// # Errors
/// - `F0002`: `records` is missing, empty, too long or not strings, or a parameter has the wrong
///   type; `path_outside_project` for a record path outside the project.
/// - `provenance_import_unreadable`: a record or folder does not read.
/// - `provenance_import_invalid`: a record does not parse or breaks a rule, or `under` is unsafe.
/// - `provenance_import_conflict`: rows conflict; nothing was written (`data.conflicts`).
/// - `provenance_import_write_failed`: a `.meta` could not be written.
pub fn provenance_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let list = crate::params::opt_array(params, "records")?
        .filter(|l| !l.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "{METHOD}: `records` (project-relative record files or folders) is required"
            ))
        })?;
    if list.len() > MAX_RECORD_PATHS {
        return Err(RpcError::invalid_params(format!(
            "{METHOD}: at most {MAX_RECORD_PATHS} record paths per call"
        )));
    }
    let under = crate::params::opt_str(params, "under")?
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let root = crate::req_path::project_root(world);
    let mut records = Vec::new();
    for item in list {
        let rel = item.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            RpcError::invalid_params(format!("{METHOD}: every `records` entry is a non-empty string"))
        })?;
        let path = crate::req_path::project_path_in(&root, "records", rel)?;
        for file in record_files(&path).map_err(|e| rpc_error(&e))? {
            let label = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            records.push(read_pack_record(&file, &label).map_err(|e| rpc_error(&e))?);
        }
    }
    let report = import_provenance(&root.join("assets"), &records, &ImportOptions { under, dry_run })
        .map_err(|e| rpc_error(&e))?;
    serde_json::to_value(&report).map_err(|e| RpcError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::provenance_import::file_sha256_hex;
    use regia_asset::{AssetId, MetaSidecar, meta_path_for};
    use serde_json::json;

    fn world_at(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    #[test]
    fn provenance_import_writes_reports_and_refuses() {
        let root = regia_test_utils::TempDir::new("agent-prov-import");
        let source = root.join("assets/voice/intro.ogg");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dir");
        std::fs::write(&source, b"OggS intro").expect("source");
        std::fs::write(
            meta_path_for(&source),
            MetaSidecar::new("ogg", AssetId::new_random())
                .to_ron_string()
                .expect("meta"),
        )
        .expect("meta");
        let sha = file_sha256_hex(&source).expect("hash");
        std::fs::create_dir_all(root.join("provenance")).expect("dir");
        let record = |sha: &str| {
            format!(
                "schema = 3\npack_id = \"vo\"\nstatus = \"approved\"\n[[files]]\npath = \"voice/intro.ogg\"\n\
                 sha256 = \"{sha}\"\nbyte_len = 10\norigin = \"original\"\ntool = \"Narrator voice\"\n\
                 generator = \"none\"\nline_id = \"intro\"\ncreated_utc = \"2026-09-28T00:00:00Z\"\n\
                 reviewer = \"owner\"\nforeign_game_file = false\nthird_party_mod_file = false\n"
            )
        };
        std::fs::write(root.join("provenance/vo.toml"), record(&sha)).expect("record");
        let mut world = world_at(&root);
        let dry = provenance_import(&mut world, &json!({ "records": ["provenance"], "dry_run": true }))
            .expect("dry run");
        assert_eq!(dry["written"], json!(["voice/intro.ogg"]));
        assert_eq!(dry["applied"], false);
        let done =
            provenance_import(&mut world, &json!({ "records": ["provenance/vo.toml"] })).expect("import");
        assert_eq!(done["applied"], true);
        let again =
            provenance_import(&mut world, &json!({ "records": ["provenance/vo.toml"] })).expect("again");
        assert_eq!(again["kept"], json!(["voice/intro.ogg"]));
        assert_eq!(again["written"], json!([]));

        // A record whose hash does not match: the code leads, data lists the conflict.
        std::fs::write(root.join("provenance/vo.toml"), record(&"ab".repeat(32))).expect("record");
        let err = provenance_import(&mut world, &json!({ "records": ["provenance/vo.toml"] }))
            .expect_err("conflict");
        let data = err.data.clone().unwrap_or_default();
        assert_eq!(data["code"], "provenance_import_conflict", "{err:?}");
        assert_eq!(data["conflicts"][0]["kind"], "hash_mismatch", "{err:?}");
        // Parameter and path refusals.
        let err = provenance_import(&mut world, &json!({})).expect_err("missing");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let err =
            provenance_import(&mut world, &json!({ "records": ["../outside.toml"] })).expect_err("outside");
        assert_eq!(err.data.unwrap_or_default()["reason"], "path_outside_project");
        let err =
            provenance_import(&mut world, &json!({ "records": ["provenance/none.toml"] })).expect_err("gone");
        assert_eq!(
            err.data.unwrap_or_default()["code"],
            "provenance_import_unreadable"
        );
    }
}
