// SPDX-License-Identifier: MPL-2.0
//! `regia gen list | run <name>... | verify [<name>...]`: generators (`docs/specs/23` § 6,
//! `docs/formats/generator-toml.md`, ADR-2412).
//!
//! `run` runs each named generator (or `--all`) and installs its outputs with provenance
//! `generator + seed` in each `.meta`. `verify` re-runs generators into staging folders and fails
//! (exit 1) with the list of every file whose bytes differ, is missing or is extra. `--json`
//! prints the report as one JSON document on stdout for CI.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_asset_pipeline::generators::{self, DiffKind, GenDiff};

/// `regia gen` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum GenCommand {
    /// List the generators declared in `generators/*.toml`.
    List(GenListArgs),
    /// Run generators and install their outputs (provenance generator + seed in each `.meta`).
    Run(GenRunArgs),
    /// Re-run generators into staging folders; exit 1 with a diff list when any byte differs.
    Verify(GenVerifyArgs),
}

/// Arguments of `regia gen list`.
#[derive(Debug, Clone, Args)]
pub struct GenListArgs {
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `regia gen run`.
#[derive(Debug, Clone, Args)]
pub struct GenRunArgs {
    /// Generators to run (`generators/<name>.toml`).
    #[arg(value_name = "NAME", required_unless_present = "all")]
    pub names: Vec<String>,
    /// Run every declared generator, in name order.
    #[arg(long, conflicts_with = "names")]
    pub all: bool,
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `regia gen verify`.
#[derive(Debug, Clone, Args)]
pub struct GenVerifyArgs {
    /// Generators to verify (default: every declared generator).
    #[arg(value_name = "NAME")]
    pub names: Vec<String>,
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Runs a `regia gen` subcommand.
///
/// # Errors
///
/// A generator failed to load or run (its stable code leads the message), or `verify` found a
/// difference.
pub fn run(command: &GenCommand) -> Result<()> {
    match command {
        GenCommand::List(args) => list(args),
        GenCommand::Run(args) => run_generators(args),
        GenCommand::Verify(args) => verify(args),
    }
}

fn project_dir(raw: &std::path::Path) -> Result<PathBuf> {
    raw.canonicalize()
        .with_context(|| format!("project directory {} not found", raw.display()))
}

fn list(args: &GenListArgs) -> Result<()> {
    let project = project_dir(&args.project)?;
    let listed = generators::list(&project);
    if args.json {
        let rows: Vec<serde_json::Value> = listed
            .iter()
            .map(|(name, loaded)| match loaded {
                Ok(def) => serde_json::json!({
                    "name": name, "ok": true, "kind": def.kind.name(), "seed": def.seed,
                    "description": def.description, "inputs": def.inputs, "outputs": def.outputs,
                }),
                Err(err) => serde_json::json!({ "name": name, "ok": false, "error": format!("{}: {err}", err.code()) }),
            })
            .collect();
        println!("{}", serde_json::json!({ "generators": rows }));
        return Ok(());
    }
    if listed.is_empty() {
        println!(
            "no generators in {}",
            project.join(generators::GENERATORS_DIR).display()
        );
    }
    for (name, loaded) in &listed {
        match loaded {
            Ok(def) => println!(
                "{name}  {}  seed {}  {} output(s){}",
                def.kind.name(),
                def.seed,
                def.outputs.len(),
                if def.description.is_empty() {
                    String::new()
                } else {
                    format!("  {}", def.description)
                }
            ),
            Err(err) => println!("{name}  error {}: {err}", err.code()),
        }
    }
    Ok(())
}

fn run_generators(args: &GenRunArgs) -> Result<()> {
    let project = project_dir(&args.project)?;
    let names: Vec<String> = if args.all {
        generators::list(&project)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    } else {
        args.names.clone()
    };
    let mut reports = Vec::new();
    for name in &names {
        let report = generators::run(&project, name).map_err(|e| anyhow::anyhow!("{}: {e}", e.code()))?;
        if !args.json {
            let written = report.outputs.iter().filter(|o| o.written).count();
            println!(
                "{name}: {} output(s), {written} written, {} unchanged, {} removed (seed {})",
                report.outputs.len(),
                report.outputs.len() - written,
                report.removed.len(),
                report.seed
            );
        }
        reports.push(report);
    }
    if args.json {
        println!("{}", serde_json::json!({ "runs": reports }));
    }
    Ok(())
}

/// One line of the human diff list.
#[must_use]
pub fn diff_line(diff: &GenDiff) -> String {
    match diff.kind {
        DiffKind::Changed => format!(
            "{}: {} differs (first differing byte {})",
            diff.generator,
            diff.path,
            diff.first_difference.unwrap_or(0)
        ),
        DiffKind::Missing => format!("{}: {} is missing from the project", diff.generator, diff.path),
        DiffKind::Extra => format!(
            "{}: {} is not written by the generator",
            diff.generator, diff.path
        ),
    }
}

fn verify(args: &GenVerifyArgs) -> Result<()> {
    let project = project_dir(&args.project)?;
    let report =
        generators::verify(&project, &args.names).map_err(|e| anyhow::anyhow!("{}: {e}", e.code()))?;
    let ok = report.ok();
    if args.json {
        let mut value = serde_json::to_value(&report)?;
        if let Some(map) = value.as_object_mut() {
            map.insert("ok".into(), serde_json::Value::Bool(ok));
        }
        println!("{value}");
    } else {
        for entry in &report.generators {
            match &entry.error {
                Some(error) => println!("{}: error {error}", entry.generator),
                None if entry.ok => println!("{}: ok ({} file(s))", entry.generator, entry.files),
                None => println!("{}: DIFFERS", entry.generator),
            }
        }
        for diff in &report.diffs {
            println!("  {}", diff_line(diff));
        }
    }
    if !ok {
        let failed = report.generators.iter().filter(|g| g.error.is_some()).count();
        bail!(
            "gen_verify_failed: {} file(s) differ, {failed} generator(s) failed",
            report.diffs.len()
        );
    }
    Ok(())
}
