// SPDX-License-Identifier: MPL-2.0
//! `regia assets stream-report <level>` (`docs/specs/78-world-streaming.md` § 6.4): how a streamed
//! level partitions, without writing anything: cell count, entities and bytes per cell, the
//! always-loaded set and every cook warning with its code.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset_pipeline::stream_cook::{
    CacheAssets, StreamReport, cook_streamed_level, register_cook_types,
};
use regia_core::id::Guid;
use regia_scene::level::LevelId;

/// `regia assets stream-report` arguments.
#[derive(Debug, Args)]
pub struct StreamReportArgs {
    /// The level: a `.level.ron` file or `.level.d` directory, relative to the project.
    pub level: PathBuf,
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Cooked platform whose cache sizes the dependencies (defaults to host).
    #[arg(long)]
    pub platform: Option<String>,
    /// Print JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// The report of one level.
///
/// # Errors
///
/// The level does not read, is not streamed, or does not cook.
pub fn level_report(args: &StreamReportArgs) -> Result<StreamReport> {
    let path = args.project.join(&args.level);
    let scene = regia_scene::level_dir::read_level_source(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let platform = args
        .platform
        .clone()
        .unwrap_or_else(|| regia_asset_pipeline::host_platform().to_string());
    // Graph materials bake from their sources in the project (ADR-2713).
    let assets = CacheAssets::new(regia_asset::cooked_cache_dir(&args.project, &platform))
        .with_project_root(&args.project);
    let mut registry = regia_ecs::reflect::TypeRegistry::default();
    register_cook_types(&mut registry);
    let level = LevelId(Guid::from_u128(0));
    let rel = args.level.to_string_lossy();
    let Some(output) = cook_streamed_level(level, &scene, &registry, &assets, None, &rel)? else {
        bail!("{} is not a streamed level", args.level.display());
    };
    Ok(output.report)
}

/// Runs the command and returns what to print.
///
/// # Errors
///
/// See [`level_report`].
pub fn run(args: &StreamReportArgs) -> Result<String> {
    let report = level_report(args)?;
    if args.json {
        return Ok(serde_json::to_string_pretty(&report)?);
    }
    let mut out = format!(
        "cells: {}\nentities per cell: min {} median {} max {}\nbytes: {} in cells (largest {}), {} always loaded \
         ({} entities)",
        report.cells,
        report.entities_min,
        report.entities_median,
        report.entities_max,
        report.cell_bytes,
        report.cell_bytes_max,
        report.always_bytes,
        report.always_entities
    );
    for warning in &report.warnings {
        out.push_str(&format!("\nwarning {}: {}", warning.code, warning.message));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_level_reports_its_cells_and_a_plain_one_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("stream_report");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        std::fs::write(
            dir.join("assets/open.level.ron"),
            format!(
                "(format_version: 1, entities: [(guid: \"{}\", components: {{ \
                 \"regia_scene::transform::Transform\": (translation: (10.0, 0.0, 10.0)) }})], resources: {{ \
                 \"regia_scene::streaming::LevelStreaming\": (enabled: true, hlod: None) }})",
                Guid::from_u128(1)
            ),
        )
        .expect("write");
        std::fs::write(dir.join("assets/plain.level.ron"), "(format_version: 1)").expect("write");
        let args = |level: &str| StreamReportArgs {
            level: PathBuf::from(level),
            project: dir.clone(),
            platform: None,
            json: false,
        };
        let report = level_report(&args("assets/open.level.ron")).expect("report");
        assert_eq!((report.cells, report.entities_max), (1, 1));
        let error = level_report(&args("assets/plain.level.ron")).expect_err("plain");
        assert!(error.to_string().contains("not a streamed level"), "{error}");
    }
}
