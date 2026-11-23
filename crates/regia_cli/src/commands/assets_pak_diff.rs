// SPDX-License-Identifier: MPL-2.0
//! `regia assets pak-diff <old> <new> [--json] [--chunk-size BYTES]`: what shipping the new
//! build's paks over the old one's costs a player (spec 22 T2.14, ADR-3322).
//!
//! Each side is a `.fpak` file or a packaged build directory (paks are found recursively, paired
//! by relative path). The report lists, per pak and summed: per-asset changes by [`AssetId`]
//! (added, removed, changed, unchanged, moved), the fixed-size chunk comparison, and an estimated
//! patch download under SteamPipe's public fixed-chunk model (default 1 MiB chunks — an estimate,
//! not a measurement of a real Steam build).

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use regia_asset::pak_diff::{DEFAULT_CHUNK_SIZE, PakSetDiff};

/// `regia assets pak-diff` arguments.
#[derive(Debug, Args)]
pub struct AssetsPakDiffArgs {
    /// The old pak file or packaged build directory.
    #[arg(value_name = "OLD")]
    pub old: PathBuf,
    /// The new pak file or packaged build directory.
    #[arg(value_name = "NEW")]
    pub new: PathBuf,
    /// Print JSON on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Chunk size for the patch model: bytes, or KiB/MiB/GiB suffixes (default 1 MiB, the size
    /// Steam's public SteamPipe documentation describes).
    #[arg(long, value_name = "BYTES")]
    pub chunk_size: Option<String>,
}

/// Runs the diff and prints the report.
///
/// # Errors
///
/// A side does not exist or does not read (bad magic, truncated pak), a directory side holds no
/// `.fpak` files, or `--chunk-size` does not parse.
pub fn run(args: &AssetsPakDiffArgs) -> Result<String> {
    let chunk_size = match &args.chunk_size {
        Some(text) => regia_asset_pipeline::cook_cache::parse_size(text)
            .map_err(|e| anyhow::anyhow!("--chunk-size: {e}"))?,
        None => DEFAULT_CHUNK_SIZE,
    };
    let diff = regia_asset::pak_diff::diff_sides(&args.old, &args.new, chunk_size).map_err(|e| {
        let code = match &e {
            regia_asset::pak_diff::PakDiffError::InvalidChunkSize(_) => "pak_diff_invalid_chunk_size",
            regia_asset::pak_diff::PakDiffError::NoPaks { .. } => "pak_diff_no_paks",
            regia_asset::pak_diff::PakDiffError::Open { .. }
            | regia_asset::pak_diff::PakDiffError::Io { .. } => "pak_diff_unreadable",
        };
        anyhow::anyhow!("{code}: {e}")
    })?;
    if args.json {
        Ok(serde_json::to_string_pretty(&diff)?)
    } else {
        Ok(to_text(&diff))
    }
}

fn bytes(n: u64) -> String {
    if n >= 1 << 30 && n.is_multiple_of(1 << 30) {
        format!("{} GiB", n / (1 << 30))
    } else if n >= 1 << 20 && n.is_multiple_of(1 << 20) {
        format!("{} MiB", n / (1 << 20))
    } else if n >= 1 << 10 && n.is_multiple_of(1 << 10) {
        format!("{} KiB", n / (1 << 10))
    } else {
        format!("{n} B")
    }
}

fn asset_line(totals: &regia_asset::pak_diff::AssetTotals) -> String {
    let parts = [
        (totals.added, "added"),
        (totals.removed, "removed"),
        (totals.changed, "changed"),
        (totals.moved, "moved"),
        (totals.unchanged, "unchanged"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, label)| format!("{n} {label}"))
    .collect::<Vec<_>>();
    if parts.is_empty() {
        "no assets".to_string()
    } else {
        parts.join(", ")
    }
}

fn pak_line(pak: &regia_asset::pak_diff::PakDiff) -> String {
    let len = |n: Option<u64>| n.map_or_else(|| "-".to_string(), bytes);
    let mut out = format!("pak  {}\n", pak.pak);
    out.push_str(&format!(
        "  assets: {} (old {} -> new {})\n",
        asset_line(&pak.totals),
        len(pak.old_len),
        len(pak.new_len)
    ));
    out.push_str(&format!(
        "  chunks ({}): {} unchanged, {} changed, {} added, {} removed (old {}, new {})\n",
        bytes(pak.chunks.chunk_size),
        pak.chunks.unchanged,
        pak.chunks.changed,
        pak.chunks.added,
        pak.chunks.removed,
        pak.chunks.old_chunks,
        pak.chunks.new_chunks
    ));
    out.push_str(&format!(
        "  estimated patch download: {} of {} (estimate; see note)\n",
        bytes(pak.estimate.download_bytes),
        len(pak.new_len),
    ));
    out
}

/// The text report: one block per pak pair, the sums first.
#[must_use]
pub fn to_text(diff: &PakSetDiff) -> String {
    let mut out = String::new();
    out.push_str(&format!("patch estimate ({} chunks)\n", bytes(diff.chunk_size)));
    out.push_str(&format!("  assets: {}\n", asset_line(&diff.totals)));
    out.push_str(&format!(
        "  chunks: {} unchanged, {} changed, {} added, {} removed\n",
        diff.chunks.unchanged, diff.chunks.changed, diff.chunks.added, diff.chunks.removed
    ));
    out.push_str(&format!(
        "  estimated patch download: {} of {} new bytes\n",
        bytes(diff.estimate.download_bytes),
        bytes(diff.paks.iter().filter_map(|p| p.new_len).sum::<u64>()),
    ));
    for pak in &diff.paks {
        if diff.paks.len() > 1 || diff.totals.added + diff.totals.removed > 0 {
            out.push_str(&pak_line(pak));
        }
    }
    out.push_str(&format!("note: {}\n", diff.estimate.note));
    out
}

/// The JSON report's shape, for tests that do not want to re-parse the CLI's text.
#[cfg(test)]
fn json_summary(old: &std::path::Path, new: &std::path::Path, chunk_size: u64) -> Result<serde_json::Value> {
    let diff = regia_asset::pak_diff::diff_sides(old, new, chunk_size)?;
    Ok(serde_json::to_value(&diff)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::pak_diff::{AssetChange, PakDiffError};

    /// Writes an uncompressed pak through the real writer (id-sorted, 4096-aligned). The manifest
    /// is stored sorted by asset id so a reordered `entries` list is the only variable.
    fn cooked_pak(tmp: &std::path::Path, name: &str, payloads: &[(regia_asset::AssetId, Vec<u8>)]) {
        let cooked = tmp.join(format!("cooked-{name}"));
        std::fs::create_dir_all(&cooked).expect("mkdir");
        let mut manifest = regia_asset::AssetManifest { entries: Vec::new() };
        let mut entries = Vec::new();
        for (id, payload) in payloads {
            std::fs::write(cooked.join(regia_asset::cooked_file_name(*id)), payload).expect("write");
            entries.push((*id, regia_core::id::TypePathHash::of("regia_asset::CookedImage")));
            manifest.entries.push(regia_asset::ManifestEntry {
                path: format!("assets/{id}.bin"),
                id: *id,
                type_path: "regia_asset::CookedImage".to_string(),
            });
        }
        manifest.entries.sort_by_key(|e| e.id);
        manifest.write_to_cache(&cooked).expect("manifest");
        write_pak_uncompressed(&tmp.join(name), &cooked, &entries, &manifest);
    }

    fn write_pak_uncompressed(
        out: &std::path::Path,
        cooked: &std::path::Path,
        entries: &[(regia_asset::AssetId, regia_core::id::TypePathHash)],
        manifest: &regia_asset::AssetManifest,
    ) {
        regia_asset_pipeline::pak_writer::write_pak(
            out,
            cooked,
            entries,
            manifest,
            &regia_asset_pipeline::pak_writer::PakWriteOptions {
                compress: false,
                ..regia_asset_pipeline::pak_writer::PakWriteOptions::default()
            },
        )
        .expect("write pak");
    }

    #[test]
    fn end_to_end_changed_asset_estimate() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-cli");
        let ids: Vec<_> = (0..2).map(|_| regia_asset::AssetId::new_random()).collect();
        let mut payloads: Vec<_> = ids.iter().map(|id| (*id, vec![11u8; 8000])).collect();
        cooked_pak(&tmp, "old.fpak", &payloads);
        payloads[0].1 = vec![42u8; 8000];
        cooked_pak(&tmp, "new.fpak", &payloads);

        let value = json_summary(&tmp.join("old.fpak"), &tmp.join("new.fpak"), 4096).expect("diff");
        assert_eq!(value["totals"]["changed"], serde_json::json!(1), "{value}");
        assert_eq!(value["totals"]["unchanged"], serde_json::json!(1));
        let assets = value["paks"][0]["assets"].as_array().expect("assets");
        assert_eq!(
            assets.iter().filter(|a| a["change"] == "changed").count(),
            1,
            "{value}"
        );
        // Same-length edit: the estimate covers the header, the edited asset's two chunks and
        // the unchanged TOC region — the untouched asset's chunks are reused.
        assert_eq!(value["chunks"]["unchanged"], serde_json::json!(3), "{value}");
        assert_eq!(value["chunks"]["changed"], serde_json::json!(3), "{value}");
        assert_eq!(
            value["estimate"]["download_bytes"].as_u64().unwrap_or(0) % 4096,
            0,
            "whole chunks: {value}"
        );
        assert_eq!(
            value["estimate"]["download_bytes"],
            serde_json::json!(3 * 4096),
            "header + edited asset's two chunks: {value}"
        );
        assert!(
            value["estimate"]["note"]
                .as_str()
                .is_some_and(|n| n.contains("estimate"))
        );
    }

    #[test]
    fn end_to_end_added_asset_and_reordered_sources() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-cli-add");
        let ids: Vec<_> = (0..3).map(|_| regia_asset::AssetId::new_random()).collect();
        let mut payloads: Vec<_> = ids.iter().map(|id| (*id, vec![7u8; 3000])).collect();
        cooked_pak(&tmp, "old.fpak", &payloads);
        // A reordered entry list must not change the pak bytes (the writer sorts by id; the
        // manifest is the same bytes in both writes).
        let manifest = regia_asset::AssetManifest {
            entries: {
                let mut entries: Vec<_> = ids
                    .iter()
                    .map(|id| regia_asset::ManifestEntry {
                        path: format!("assets/{id}.bin"),
                        id: *id,
                        type_path: "regia_asset::CookedImage".to_string(),
                    })
                    .collect();
                entries.sort_by_key(|e| e.id);
                entries
            },
        };
        let tmp2 = regia_test_utils::unique_temp_dir("pak-diff-cli-reorder");
        let cooked2 = tmp2.join("cooked");
        std::fs::create_dir_all(&cooked2).expect("mkdir");
        for (id, payload) in &payloads {
            std::fs::write(cooked2.join(regia_asset::cooked_file_name(*id)), payload).expect("write");
        }
        let mut shuffled: Vec<_> = ids
            .iter()
            .map(|id| (*id, regia_core::id::TypePathHash::of("regia_asset::CookedImage")))
            .collect();
        shuffled.swap(0, 2);
        shuffled.swap(1, 2);
        write_pak_uncompressed(&tmp2.join("old.fpak"), &cooked2, &shuffled, &manifest);
        assert_eq!(
            std::fs::read(tmp.join("old.fpak")).expect("old"),
            std::fs::read(tmp2.join("old.fpak")).expect("reordered"),
            "a reordered source list must give identical pak bytes"
        );
        // Add an asset whose id sorts after every existing one (the rest keep their offsets).
        let fourth = regia_asset::AssetId(regia_core::id::Guid(
            ids.iter().map(|id| id.0.0).max().expect("an id") + 1,
        ));
        payloads.push((fourth, vec![9u8; 9000]));
        cooked_pak(&tmp, "new.fpak", &payloads);

        let value = json_summary(&tmp.join("old.fpak"), &tmp.join("new.fpak"), 4096).expect("diff");
        assert_eq!(value["totals"]["added"], serde_json::json!(1), "{value}");
        assert_eq!(value["totals"]["unchanged"], serde_json::json!(3));
        assert_eq!(value["totals"]["moved"], serde_json::json!(0));
    }

    #[test]
    fn truncated_pak_is_an_error_with_a_code() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-cli-trunc");
        let ids: Vec<_> = (0..1).map(|_| regia_asset::AssetId::new_random()).collect();
        let payloads: Vec<_> = ids.iter().map(|id| (*id, vec![5u8; 2000])).collect();
        cooked_pak(&tmp, "old.fpak", &payloads);
        cooked_pak(&tmp, "new.fpak", &payloads);
        std::fs::write(
            tmp.join("cut.fpak"),
            &std::fs::read(tmp.join("new.fpak")).expect("read")[..100],
        )
        .expect("cut");
        let err = json_summary(&tmp.join("old.fpak"), &tmp.join("cut.fpak"), 4096).expect_err("cut pak");
        let reported = err.to_string();
        assert!(
            reported.contains("pak_diff_unreadable")
                || matches!(
                    err.downcast_ref::<PakDiffError>(),
                    Some(PakDiffError::Open { .. })
                ),
            "{reported}"
        );
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(4096), "4 KiB");
        assert_eq!(bytes(1 << 20), "1 MiB");
        assert_eq!(bytes((1 << 20) + 1), format!("{} B", (1 << 20) + 1));
    }

    #[test]
    fn change_kinds_serialize_stably() {
        assert_eq!(
            serde_json::to_value(AssetChange::Moved).expect("json"),
            serde_json::json!("moved")
        );
        assert_eq!(
            serde_json::to_value(AssetChange::Added).expect("json"),
            serde_json::json!("added")
        );
    }
}
