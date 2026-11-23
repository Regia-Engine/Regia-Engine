// SPDX-License-Identifier: MPL-2.0
//! `regia assets lint-geometry`: source geometry checks with the cook's evidence cache (ADR-8390).
//!
//! The command does not repair sources. `--fix` is not accepted. Cook and package call the same
//! scan with repair forced off.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset_pipeline::source_lint::{FindingSeverity, SourceLintReport};
use regia_asset_pipeline::source_lint_cache::{ScanCounters, ScanError, ScanRequest, scan};
use serde_json::json;

/// `regia assets lint-geometry` arguments.
#[derive(Debug, Args)]
pub struct GeometryLintArgs {
    /// Source `.gltf` or `.glb` files or folders. Empty scans `<project>/assets`.
    pub paths: Vec<PathBuf>,
    /// Project directory. The evidence cache is `build/source-lint/` here.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Source-lint target profile. `default` is the engine target.
    #[arg(long, default_value = "default")]
    pub target: String,
    /// File with one changed project-relative path per line.
    #[arg(long, value_name = "FILE")]
    pub changed: Option<PathBuf>,
    /// Write the evidence JSON here. Default: `<project>/build/source-lint/report.json`.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Stop the scan when this file exists. The process prints one JSON error and exits nonzero.
    #[arg(long, value_name = "FILE")]
    pub cancel_file: Option<PathBuf>,
    /// Skip files and folders whose names begin with an underscore.
    #[arg(long, default_value_t = false)]
    pub skip_staging: bool,
    /// Warnings fail the run. Geometry warnings fail even without this flag.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
    /// Print one JSON object.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Runs the geometry source lint.
///
/// # Errors
/// A policy, IO, or lint failure, or unsuppressed geometry warnings.
pub fn run(args: &GeometryLintArgs) -> Result<()> {
    let changed = read_changed(args.changed.as_deref())?;
    let request = ScanRequest {
        project: args.project.clone(),
        target: args.target.clone(),
        paths: args.paths.clone(),
        changed,
        strict: args.strict,
        fix: false,
        cancel_file: args.cancel_file.clone(),
        report_path: args.report.clone(),
        material_profile: None,
        skip_staging: args.skip_staging,
    };
    match scan(&request) {
        Ok(outcome) => {
            let failed = geometry_failed(&outcome.report) || (args.strict && any_warning(&outcome.report));
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "ok": !failed,
                        "target": args.target,
                        "report": outcome.report,
                        "report_path": outcome.report_path,
                        "counters": outcome.counters,
                        "rule_ids": outcome.rule_ids,
                        "evidence_key": outcome.report.evidence_key,
                    }))?
                );
            } else {
                print_text(&outcome.report, &outcome.counters, &outcome.report_path);
            }
            if failed {
                bail!("glTF geometry lint failed; see the report above");
            }
            Ok(())
        }
        Err(error) => {
            if args.json && error.code() == "content_lint_cancelled" {
                println!("{}", cancelled_json(&error));
            }
            bail!(error)
        }
    }
}

fn geometry_failed(report: &SourceLintReport) -> bool {
    report.findings.iter().any(|finding| {
        !finding.suppressed
            && finding.severity == FindingSeverity::Warning
            && finding.rule_id.starts_with("gltf_")
    }) || !report.complete
}

fn any_warning(report: &SourceLintReport) -> bool {
    report
        .findings
        .iter()
        .any(|finding| !finding.suppressed && finding.severity == FindingSeverity::Warning)
}

fn print_text(report: &SourceLintReport, counters: &ScanCounters, report_path: &std::path::Path) {
    for finding in &report.findings {
        println!(
            "{} {} {}: {}",
            match finding.severity {
                FindingSeverity::Warning => "warning",
                FindingSeverity::Info => "info",
            },
            finding.rule_id,
            finding.path,
            finding.message
        );
    }
    println!(
        "source lint {}: {} finding(s), parses {}, decoded {}, cache hits {}, report {}",
        report.evidence_key,
        report.findings.len(),
        counters.source_parses,
        counters.decoded_bytes,
        counters.cache_hits,
        report_path.display()
    );
}

fn read_changed(path: Option<&std::path::Path>) -> Result<Option<Vec<String>>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

fn cancelled_json(error: &ScanError) -> String {
    json!({
        "ok": false,
        "error": {"code": error.code(), "message": error.to_string()}
    })
    .to_string()
}
