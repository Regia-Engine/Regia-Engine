// SPDX-License-Identifier: MPL-2.0
//! `assets.pak_diff { old, new, chunk_size? }` (ADR-3322, `book/agents/content-chunks.md` §
//! Shipping updates, guide topic `content-chunks`): what shipping the new build's paks over the
//! old one's costs a player. Each side is a pak file or a packaged build directory; the summary
//! reports per-asset changes (added, removed, changed, moved, unchanged by asset id), the
//! fixed-size chunk comparison and an estimated patch download under SteamPipe's public
//! fixed-chunk model (default 1 MiB). Desktop tools permission class (`tools` in
//! `.regia/agent.toml`): the method reads arbitrary build paths, which a game run must opt into.

use std::path::PathBuf;

use regia_app::prelude::*;
use serde_json::Value;

use crate::protocol::RpcError;

const METHOD: &str = "assets.pak_diff";
const DEFAULT_CHUNK_SIZE: u64 = regia_asset::pak_diff::DEFAULT_CHUNK_SIZE;
/// Largest chunk buffer `hash_chunks` may allocate. A larger count aborts the process.
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// `assets.pak_diff { old, new, chunk_size? }`: the update-cost report for two pak files or
/// packaged build directories.
pub fn pak_diff(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = |name: &str| -> Result<PathBuf, RpcError> {
        crate::params::opt_str(params, name)?
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "{METHOD}: `{name}` (a pak file or build directory) is required"
                ))
            })
    };
    let old = path("old")?;
    let new = path("new")?;
    let chunk_size = match params.get("chunk_size") {
        None | Some(Value::Null) => DEFAULT_CHUNK_SIZE,
        Some(value) => {
            let n = value.as_u64().ok_or_else(|| {
                RpcError::new(
                    crate::protocol::INVALID_PARAMS,
                    "pak_diff_invalid_chunk_size",
                    format!("{METHOD}: `chunk_size` is a byte count (integer), got {value}"),
                    "pass an integer number of bytes (default 1048576: SteamPipe's ~1 MiB chunks)",
                )
            })?;
            if n == 0 || n > MAX_CHUNK_SIZE {
                return Err(RpcError::new(
                    crate::protocol::INVALID_PARAMS,
                    "pak_diff_invalid_chunk_size",
                    format!("{METHOD}: `chunk_size` must be an integer from 1 to {MAX_CHUNK_SIZE} bytes"),
                    "pass an integer number of bytes (default 1048576: SteamPipe's ~1 MiB chunks)",
                ));
            }
            n
        }
    };
    let diff = regia_asset::pak_diff::diff_sides(&old, &new, chunk_size).map_err(|e| match e {
        regia_asset::pak_diff::PakDiffError::NoPaks { side, path } => RpcError::new(
            crate::protocol::NOT_FOUND,
            "pak_diff_no_paks",
            format!(
                "{METHOD}: the {side} directory {} contains no .fpak files",
                path.display()
            ),
            "point the side at a pak file or a packaged build's content directory",
        ),
        other => RpcError::new(
            crate::protocol::NOT_FOUND,
            "pak_diff_unreadable",
            format!("{METHOD}: {other}"),
            "check the path exists and is a complete .fpak (a truncated pak is an error, not a diff)",
        ),
    })?;
    serde_json::to_value(&diff).map_err(|e| {
        RpcError::new(
            crate::protocol::INTERNAL_ERROR,
            "pak_diff_report",
            format!("{METHOD}: cannot serialize the report: {e}"),
            "retry; the diff itself succeeded",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    /// Two small paks through the real writer: `old` with one asset, `new` with a second whose id
    /// sorts after the first (so the first keeps its offset and reads as unchanged).
    fn fixture_paks(prefix: &str) -> (std::path::PathBuf, std::path::PathBuf, regia_asset::AssetId) {
        let tmp = regia_test_utils::unique_temp_dir(prefix);
        let first = regia_asset::AssetId::new_random();
        let second = regia_asset::AssetId(regia_core::id::Guid(first.0.0 + 1));
        for (name, ids) in [("old", vec![first]), ("new", vec![first, second])] {
            let cooked = tmp.join(format!("cooked-{name}"));
            std::fs::create_dir_all(&cooked).expect("mkdir");
            let mut manifest = regia_asset::AssetManifest { entries: Vec::new() };
            let mut entries = Vec::new();
            for id in &ids {
                std::fs::write(cooked.join(regia_asset::cooked_file_name(*id)), vec![8u8; 4096])
                    .expect("write");
                entries.push((*id, regia_core::id::TypePathHash::of("regia_asset::CookedImage")));
                manifest.entries.push(regia_asset::ManifestEntry {
                    path: format!("assets/{id}.bin"),
                    id: *id,
                    type_path: "regia_asset::CookedImage".to_string(),
                });
            }
            manifest.write_to_cache(&cooked).expect("manifest");
            regia_asset_pipeline::pak_writer::write_pak(
                &tmp.join(format!("{name}.fpak")),
                &cooked,
                &entries,
                &manifest,
                &regia_asset_pipeline::pak_writer::PakWriteOptions::default(),
            )
            .expect("write pak");
        }
        (tmp.join("old.fpak"), tmp.join("new.fpak"), second)
    }

    #[test]
    fn reports_the_added_asset_and_the_estimate() {
        let (old, new, added) = fixture_paks("pak-diff-agent");
        let mut world = World::new();
        let report = pak_diff(
            &mut world,
            &json!({ "old": old.to_string_lossy(), "new": new.to_string_lossy() }),
        )
        .expect("report");
        assert_eq!(report["totals"]["added"], json!(1), "{report}");
        assert_eq!(report["totals"]["unchanged"], json!(1));
        assert_eq!(
            report["chunk_size"],
            json!(regia_asset::pak_diff::DEFAULT_CHUNK_SIZE)
        );
        let added_entry = report["paks"][0]["assets"]
            .as_array()
            .expect("assets")
            .iter()
            .find(|a| a["change"] == "added")
            .expect("the added asset");
        assert_eq!(added_entry["id"], json!(added.to_string()));
        assert!(report["estimate"]["download_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(
            report["estimate"]["note"]
                .as_str()
                .is_some_and(|n| n.contains("estimate"))
        );
    }

    #[test]
    fn missing_paths_and_bad_chunk_size_are_structured_errors() {
        let mut world = World::new();
        let err = pak_diff(&mut world, &json!({ "old": "a.fpak" })).expect_err("new missing");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{err:?}");
        let err = pak_diff(
            &mut world,
            &json!({ "old": "a.fpak", "new": "b.fpak", "chunk_size": 0 }),
        )
        .expect_err("zero chunk size");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{err:?}");
        let data = err.data.as_ref().expect("data");
        assert_eq!(data["code"], json!("pak_diff_invalid_chunk_size"), "{err:?}");
    }

    #[test]
    fn an_unreadable_side_reports_pak_diff_unreadable() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-agent-missing");
        let mut world = World::new();
        let err = pak_diff(
            &mut world,
            &json!({ "old": tmp.join("none.fpak").to_string_lossy(), "new": tmp.join("none.fpak").to_string_lossy() }),
        )
        .expect_err("no such pak");
        assert_eq!(err.code, crate::protocol::NOT_FOUND, "{err:?}");
        let data = err.data.as_ref().expect("data");
        assert_eq!(data["code"], json!("pak_diff_unreadable"), "{err:?}");
    }

    /// corr-agent finding 27 (ADR-5895): every code `assets.pak_diff` can answer is documented, so
    /// `errors.explain` knows it; `pak_diff_report` (the report did not serialize) was missing.
    #[test]
    fn every_pak_diff_error_code_is_explained() {
        for code in [
            "pak_diff_no_paks",
            "pak_diff_unreadable",
            "pak_diff_report",
            "pak_diff_invalid_chunk_size",
        ] {
            let explained = crate::discovery::errors_explain(&json!({ "code": code }));
            assert!(explained.is_ok(), "{code} has no explanation: {explained:?}");
        }
    }

    /// Spec 22 § T2.14b: a side that is a directory with no `.fpak` in it answers
    /// `pak_diff_no_paks` naming that side (ADR-5628).
    #[test]
    fn a_build_directory_without_paks_reports_pak_diff_no_paks() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-agent-no-paks");
        let (old_dir, new_dir) = (tmp.join("old"), tmp.join("new"));
        std::fs::create_dir_all(&old_dir).expect("old dir");
        std::fs::create_dir_all(&new_dir).expect("new dir");
        std::fs::write(new_dir.join("readme.txt"), "not a pak").expect("file");
        let mut world = World::new();
        let err = pak_diff(
            &mut world,
            &json!({ "old": old_dir.to_string_lossy(), "new": new_dir.to_string_lossy() }),
        )
        .expect_err("no paks");
        assert_eq!(err.code, crate::protocol::NOT_FOUND, "{err:?}");
        let data = err.data.as_ref().expect("data");
        assert_eq!(data["code"], json!("pak_diff_no_paks"), "{err:?}");
        assert!(err.message.contains("old"), "names the side: {}", err.message);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_huge_chunk_size_is_refused_before_any_read() {
        let mut world = World::new();
        let err = pak_diff(
            &mut world,
            &json!({ "old": "a.fpak", "new": "b.fpak", "chunk_size": u64::MAX }),
        )
        .expect_err("huge chunk");
        let data = err.data.as_ref().expect("data");
        assert_eq!(data["code"], json!("pak_diff_invalid_chunk_size"), "{err:?}");
    }
}
