// SPDX-License-Identifier: MPL-2.0
//! `regia assets provenance-import RECORD... [--project DIR] [--under DIR] [--dry-run] [--json]`
//! (ADR-8834, `book/agents/provenance-import.md`): moves pack provenance records
//! (`provenance/<pack>.toml`, a file or a folder of them) into the provenance block of each
//! source's `.meta`, through `regia_asset::provenance_import`. All or nothing: any conflict (hash,
//! existing block, unreadable `.meta`, differing duplicates) writes nothing; a second run writes
//! nothing either.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use regia_asset::provenance_import::{
    ImportOptions, ImportReport, PackRecord, ProvenanceImportError, import_provenance, read_pack_record,
    record_files,
};

/// `regia assets provenance-import` arguments.
#[derive(Debug, Args)]
pub struct AssetsProvenanceImportArgs {
    /// Pack provenance records (`provenance/<pack>.toml`), or folders of them (every `*.toml`
    /// directly inside, sorted).
    #[arg(value_name = "RECORD", required = true)]
    pub records: Vec<PathBuf>,
    /// Project directory (contains `assets/` and `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Folder under `assets/` holding the packs' content root (record paths are relative to it;
    /// default: `assets/` itself).
    #[arg(long, value_name = "DIR")]
    pub under: Option<String>,
    /// Report what would be written without writing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Print the report as JSON on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `anyhow` error carrying the stable code first (`provenance_import_conflict: ...`), with each
/// conflict listed.
fn coded(error: &ProvenanceImportError) -> anyhow::Error {
    let mut text = format!("{}: {error}", error.code());
    if let ProvenanceImportError::Conflict(report) = error {
        for c in &report.conflicts {
            text.push_str(&format!(
                "\n  {} ({}): {}: {}",
                c.path, c.record, c.kind, c.detail
            ));
        }
        for d in report.duplicates.iter().filter(|d| !d.identical) {
            text.push_str(&format!("\n  duplicate {}: {}", d.path, d.records.join(", ")));
        }
        text.push_str(
            "\n  nothing was written; fix the rows, the sources or the .meta files and run it again",
        );
    }
    anyhow::anyhow!(text)
}

/// A record's label in reports: the path as given, `/`-separated.
fn label(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Reads every record `paths` names (files, or folders of `*.toml`), in order.
///
/// # Errors
///
/// `provenance_import_unreadable` or `provenance_import_invalid`, the code first.
pub fn read_records(paths: &[PathBuf]) -> Result<Vec<PackRecord>> {
    let mut out = Vec::new();
    for path in paths {
        for file in record_files(path).map_err(|e| coded(&e))? {
            out.push(read_pack_record(&file, &label(&file)).map_err(|e| coded(&e))?);
        }
    }
    Ok(out)
}

/// One line summarizing an import.
#[must_use]
pub fn summary(report: &ImportReport) -> String {
    format!(
        "provenance: {} record(s), {} row(s): {} {}, {} kept, {} without a source, {} without a .meta, {} duplicate(s)",
        report.records,
        report.rows,
        report.written.len(),
        if report.applied {
            "written"
        } else {
            "to write (dry run)"
        },
        report.kept.len(),
        report.missing_source.len(),
        report.missing_meta.len(),
        report.duplicates.len(),
    )
}

/// `regia assets provenance-import`.
///
/// # Errors
///
/// A record does not read or parse, a row conflicts (nothing written), or a `.meta` cannot be
/// written; the message starts with the code.
pub fn run(args: &AssetsProvenanceImportArgs) -> Result<String> {
    let records = read_records(&args.records)?;
    let options = ImportOptions {
        under: args.under.clone(),
        dry_run: args.dry_run,
    };
    let report =
        import_provenance(&args.project.join("assets"), &records, &options).map_err(|e| coded(&e))?;
    if args.json {
        return Ok(serde_json::to_string_pretty(&report)?);
    }
    let mut text = summary(&report);
    for path in &report.written {
        text.push_str(&format!("\n  write {path}.meta"));
    }
    for path in &report.missing_meta {
        text.push_str(&format!(
            "\n  no .meta for {path}: import or cook it first (or `regia assets ids import`)"
        ));
    }
    for d in &report.duplicates {
        text.push_str(&format!("\n  duplicate {}: {}", d.path, d.records.join(", ")));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::provenance_import::file_sha256_hex;
    use regia_asset::{AssetId, MetaSidecar, meta_path_for};

    fn row(path: &str, sha: &str, len: u64) -> String {
        format!(
            "[[files]]\npath = \"{path}\"\nsha256 = \"{sha}\"\nbyte_len = {len}\norigin = \"original\"\n\
             tool = \"Narrator voice\"\ngenerator = \"tools/gen/voice.py\"\nseed = 3\nline_id = \"intro\"\n\
             created_utc = \"2026-09-28T00:00:00Z\"\nreviewer = \"owner\"\nforeign_game_file = false\n\
             third_party_mod_file = false\n"
        )
    }

    #[test]
    fn cli_imports_a_record_folder_and_reports_conflicts_with_the_code() {
        let root = regia_test_utils::TempDir::new("cli-prov-import");
        let source = root.join("assets/packs/vo/voice/intro.ogg");
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
        let records = root.join("provenance");
        std::fs::create_dir_all(&records).expect("records");
        std::fs::write(
            records.join("vo.toml"),
            format!(
                "schema = 3\npack_id = \"vo\"\nstatus = \"approved\"\n{}",
                row("voice/intro.ogg", &sha, 10)
            ),
        )
        .expect("record");
        let args = |dry_run, json| AssetsProvenanceImportArgs {
            records: vec![records.clone()],
            project: root.to_path_buf(),
            under: Some("packs/vo".into()),
            dry_run,
            json,
        };
        let dry = run(&args(true, false)).expect("dry run");
        assert!(dry.contains("1 to write (dry run)"), "{dry}");
        assert!(dry.contains("write packs/vo/voice/intro.ogg.meta"), "{dry}");
        let out = run(&args(false, true)).expect("import");
        let json: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(json["written"][0], "packs/vo/voice/intro.ogg");
        assert_eq!(json["applied"], true);
        let meta =
            MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("read")).expect("parse");
        let p = meta.provenance.expect("block");
        assert_eq!(p.extensions.get("line_id").map(String::as_str), Some("intro"));
        let again = run(&args(false, false)).expect("again");
        assert!(again.contains("0 written, 1 kept"), "{again}");

        // A changed source: the hash no longer matches, nothing is written, the code leads.
        std::fs::write(&source, b"OggS INTRO").expect("changed");
        let err = run(&args(false, false)).expect_err("conflict");
        let text = format!("{err:#}");
        assert!(text.starts_with("provenance_import_conflict:"), "{text}");
        assert!(text.contains("hash_mismatch"), "{text}");
        let err = run(&AssetsProvenanceImportArgs {
            records: vec![root.join("nope.toml")],
            ..args(true, false)
        })
        .expect_err("missing");
        assert!(format!("{err:#}").starts_with("provenance_import_unreadable:"));
    }
}
