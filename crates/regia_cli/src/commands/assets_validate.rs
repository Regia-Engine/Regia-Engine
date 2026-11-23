// SPDX-License-Identifier: MPL-2.0
//! `regia assets validate|graph|size-map|audit`: asset validation, the reference graph, the size map and
//! the audit CSV over the cooked asset registry (`docs/specs/32-asset-validation-and-audit.md` § 7,
//! book `agents/asset-validators.md`). Nothing loads an asset; the registry is read through the same
//! [`RegistrySource`] flags as `regia assets query`.
//!
//! `validate` exits 0 when clean, 1 on failure (with `--strict` also on warnings) and 2 when the
//! configuration cannot be read (a [`UsageError`]).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset_pipeline::asset_audit::{
    AuditError, Combine, GraphQuery, GroupBy, Side, SizeMapQuery, audit_csv, graph_query, reference_tree,
    size_map,
};
use regia_asset_pipeline::asset_graph::{AssetSet, DependencyGraph};
use regia_asset_pipeline::source_lint::{FindingSeverity, SourceLintReport};
use regia_asset_pipeline::source_lint_cache::{self, ScanOutcome, ScanRequest};
use regia_asset_pipeline::validation_rules::{CookGate, ValidationFile};
use regia_asset_pipeline::validation_triggers::{CookGateOutcome, cook_gate};
use regia_asset_pipeline::validators::{RunSettings, Scope, Trigger, ValidationReport, ValidatorRegistry};
use serde_json::json;

use super::assets_query::{RegistrySource, load};

/// An error that makes the process exit 2 (unreadable or invalid configuration).
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// A source-lint failure that the cook or package JSON result must carry.
#[derive(Debug)]
pub struct LintStop {
    /// Stable lowercase code.
    pub code: String,
    /// What stopped the step.
    pub message: String,
    /// `failed` or the gate status.
    pub status: String,
    /// Report path, when one was written.
    pub report: Option<String>,
}

impl std::fmt::Display for LintStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for LintStop {}

/// Runs source diagnostics before cooked output is published (ADR-8390).
///
/// # Errors
/// Invalid policy, a lint that cannot finish, or a geometry warning under a failing gate.
/// The error is a [`LintStop`] so `--json` can attach `lint`.
pub fn enforce_publish_lint(project: &Path, package: bool) -> Result<serde_json::Value> {
    match source_lint_cache::diagnose_before_publish(&source_lint_cache::PublishRequest {
        project: project.to_path_buf(),
        target: "default".to_owned(),
        package,
    }) {
        Ok(status) if status.blocking => Err(LintStop {
            code: "content_lint_failed".to_owned(),
            message: format!("source lint blocked publication ({})", status.status),
            status: status.status,
            report: status.report,
        }
        .into()),
        Ok(status) => Ok(serde_json::json!({
            "status": status.status,
            "report": status.report,
        })),
        Err(error) => Err(LintStop {
            code: error.code().to_owned(),
            message: error.to_string(),
            status: "failed".to_owned(),
            report: None,
        }
        .into()),
    }
}

/// `regia assets validate` arguments.
#[derive(Debug, Args)]
pub struct AssetsValidateArgs {
    /// Registry location and output form.
    #[command(flatten)]
    pub source: RegistrySource,
    /// Validate this asset (path or id); repeat for several.
    #[arg(long = "asset", value_name = "PATH")]
    pub assets: Vec<String>,
    /// Validate every asset under this folder.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["assets", "with_deps", "changed"])]
    pub folder: Option<String>,
    /// With `--folder`: include subfolders.
    #[arg(long, default_value_t = false)]
    pub recursive: bool,
    /// With `--asset`: also validate everything the assets depend on.
    #[arg(long, default_value_t = false)]
    pub with_deps: bool,
    /// File with one changed asset path per line (from your VCS diff).
    #[arg(long, value_name = "FILE", conflicts_with = "assets")]
    pub changed: Option<PathBuf>,
    /// Warnings fail the run too.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
    /// Stop gathering at this many assets.
    #[arg(long, value_name = "N")]
    pub max: Option<usize>,
    /// `validation.ron` to read (default: `<project>/config/validation.ron` when it exists).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Validate what is staged in git (the pre-commit check, `regia hooks install`): the staged
    /// text assets must parse, and with a cooked registry the validators run over the staged paths.
    #[arg(long, default_value_t = false, conflicts_with_all = ["assets", "folder", "changed"])]
    pub staged: bool,
    /// Source-lint target profile. `default` is the engine target.
    #[arg(long, default_value = "default")]
    pub target: String,
    /// Write the source-lint JSON report here.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Stop the source scan when this file exists.
    #[arg(long, value_name = "FILE")]
    pub cancel_file: Option<PathBuf>,
}

/// `regia assets graph` arguments.
#[derive(Debug, Args)]
pub struct AssetsGraphArgs {
    /// Registry location and output form.
    #[command(flatten)]
    pub source: RegistrySource,
    /// Assets (paths or ids) to start from.
    #[arg(required = true)]
    pub assets: Vec<String>,
    /// Hops of referrers (0..=8).
    #[arg(long, default_value_t = 1)]
    pub referrers: u32,
    /// Hops of dependencies (0..=8).
    #[arg(long, default_value_t = 1)]
    pub dependencies: u32,
    /// Most nodes per column (1..=500).
    #[arg(long, default_value_t = 50)]
    pub width: u32,
    /// With several assets: keep only what all of them share.
    #[arg(long, default_value_t = false)]
    pub intersect: bool,
    /// Print indented reference trees instead of columns.
    #[arg(long, default_value_t = false)]
    pub tree: bool,
}

/// `regia assets size-map` arguments.
#[derive(Debug, Args)]
pub struct AssetsSizeMapArgs {
    /// Registry location and output form.
    #[command(flatten)]
    pub source: RegistrySource,
    /// Assets (paths or ids) whose closure to measure.
    #[arg(required = true)]
    pub assets: Vec<String>,
    /// Grouping: `asset`, `kind`, `folder:N` or `tag:KEY`.
    #[arg(long, default_value = "asset")]
    pub group: String,
    /// Rows kept before folding the rest into `(other)`.
    #[arg(long, default_value_t = 200)]
    pub top: u32,
}

/// `regia assets audit` arguments.
#[derive(Debug, Args)]
pub struct AssetsAuditArgs {
    /// Registry location.
    #[command(flatten)]
    pub source: RegistrySource,
    /// Only assets under this folder.
    #[arg(long, value_name = "PATH")]
    pub folder: Option<String>,
    /// Only this asset (path or id); repeat for several.
    #[arg(long = "asset", value_name = "PATH")]
    pub assets: Vec<String>,
    /// Write the table here (atomically: temp file, then rename).
    #[arg(long, value_name = "FILE")]
    pub csv: PathBuf,
}

fn read_config(source: &RegistrySource, explicit: Option<&Path>) -> Result<ValidationFile> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let p = source.project.join("config").join("validation.ron");
            if !p.is_file() {
                return Ok(ValidationFile::default());
            }
            p
        }
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| UsageError(format!("cannot read {}: {e}", path.display())))?;
    ValidationFile::from_ron(&text).map_err(|e| UsageError(format!("{}: {e}", path.display())).into())
}

fn load_set(source: &RegistrySource, file: &ValidationFile) -> Result<AssetSet> {
    let registry = load(source)?;
    // Layouts are raw files: their component `use` edges and named pictures come from the files
    // (ADR-3147, ADR-3148).
    Ok(AssetSet::from_registry_with_roots(&registry, &file.roots)
        .with_ui_layouts(&source.project.join("assets")))
}

fn audit_error(e: &AuditError) -> anyhow::Error {
    anyhow::anyhow!("{} ({})", e, e.code())
}

/// The text (or JSON) of `regia assets validate` and whether the run failed.
///
/// # Errors
///
/// No registry, an unreadable `validation.ron` ([`UsageError`]) or an unreadable `--changed` file.
pub fn validate(args: &AssetsValidateArgs) -> Result<(String, bool)> {
    let file = read_config(&args.source, args.config.as_deref())?;
    if args.staged {
        return validate_staged(args, &file);
    }
    let mut set = load_set(&args.source, &file)?;
    let scope = if let Some(folder) = &args.folder {
        Scope::Folder {
            path: folder.clone(),
            recursive: args.recursive,
        }
    } else if let Some(changed) = &args.changed {
        let text =
            std::fs::read_to_string(changed).with_context(|| format!("reading {}", changed.display()))?;
        Scope::Changed(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    } else if !args.assets.is_empty() {
        if args.with_deps {
            Scope::WithDependencies(args.assets.clone())
        } else {
            Scope::Assets(args.assets.clone())
        }
    } else {
        Scope::Project
    };
    let source = source_lint_for_validate(args, &scope)?;
    let settings = RunSettings {
        scope,
        trigger: Trigger::Ci,
        max_assets: args.max,
        exclude: file.exclude.clone(),
        warnings_fail: false,
        only: None,
    };
    let mut registry = ValidatorRegistry::with_rules(&file);
    // `config/provenance.ron` (ADR-2428): the `provenance_policy` validator joins the run only when
    // the project has a policy, so a project without one validates as before.
    if let Some(validator) = provenance_policy_validator(&args.source.project)? {
        registry
            .register(Box::new(validator))
            .map_err(|e| UsageError(e.to_string()))?;
    }
    register_dialogue_assets(&mut registry, &mut set, &args.source.project)?;
    let report = registry.run_with(&set, &settings);
    let source_failed = source_blocks_validate(&source.report, args.strict);
    let failed = report.exit_failed(args.strict) || source_failed;
    let text = if args.source.json {
        let mut value = serde_json::to_value(&report)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("exit_failed".into(), json!(failed));
            object.insert("source_lint".into(), source_lint_json(&source, args.strict));
        }
        serde_json::to_string_pretty(&value)?
    } else {
        let mut text = report_text(&report);
        text.push_str(&source_lint_text(&source));
        text
    };
    Ok((text, failed))
}

/// What `regia assets validate --staged` found: the staged files' check and, with a cooked registry,
/// the validators' report over the staged paths (ADR-7789).
#[derive(Debug, Clone)]
pub struct StagedValidation {
    /// The staged files and their syntax findings.
    pub check: regia_asset_pipeline::precommit::StagedCheck,
    /// The validators' report over the staged paths (`None` without a registry or staged assets).
    pub report: Option<ValidationReport>,
    /// Why the registry part did not run, when it did not.
    pub registry_note: Option<String>,
}

impl StagedValidation {
    /// Whether the commit should stop: a staged file error, a failing report, or with `strict` a
    /// warning in either.
    #[must_use]
    pub fn failed(&self, strict: bool) -> bool {
        self.check.failed(strict) || self.report.as_ref().is_some_and(|r| r.exit_failed(strict))
    }
}

/// Runs the staged validation of `project` (the pre-commit check): [`regia_asset_pipeline::precommit::check_staged`],
/// then the validators of `file` over the staged paths when the registry `source` names loads.
///
/// # Errors
///
/// No git or not a git work tree ([`UsageError`], exit 2), or a failing git command.
pub fn staged_validation(source: &RegistrySource, file: &ValidationFile) -> Result<StagedValidation> {
    let check = regia_asset_pipeline::precommit::check_staged(&source.project)
        .map_err(|e| UsageError(format!("{}: {e}", e.code())))?;
    if check.changed_assets.is_empty() {
        return Ok(StagedValidation {
            check,
            report: None,
            registry_note: Some("nothing staged under the project".to_owned()),
        });
    }
    let set = match load_set(source, file) {
        Ok(set) => set,
        Err(e) => {
            return Ok(StagedValidation {
                check,
                report: None,
                registry_note: Some(format!(
                    "registry validators skipped ({e:#}); only the staged files were checked"
                )),
            });
        }
    };
    let settings = RunSettings {
        scope: Scope::Changed(check.changed_assets.clone()),
        trigger: Trigger::Ci,
        exclude: file.exclude.clone(),
        ..RunSettings::default()
    };
    let report = ValidatorRegistry::with_rules(file).run_with(&set, &settings);
    Ok(StagedValidation {
        check,
        report: Some(report),
        registry_note: None,
    })
}

fn validate_staged(args: &AssetsValidateArgs, file: &ValidationFile) -> Result<(String, bool)> {
    let staged = staged_validation(&args.source, file)?;
    let failed = staged.failed(args.strict);
    if args.source.json {
        let value = json!({
            "staged": staged.check,
            "report": staged.report,
            "registry_note": staged.registry_note,
            "exit_failed": failed,
        });
        return Ok((serde_json::to_string_pretty(&value)?, failed));
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "staged: {} file(s) under the project, {} parsed",
        staged.check.files.len(),
        staged.check.checked
    );
    for f in &staged.check.findings {
        let _ = writeln!(
            out,
            "{} {} {}: {}",
            f.finding.severity.label(),
            f.finding.code,
            f.path,
            f.finding.text
        );
    }
    if let Some(report) = &staged.report {
        out.push_str(&report_text(report));
    }
    if let Some(note) = &staged.registry_note {
        out.push_str(note);
    }
    if failed {
        out.push_str("\ncommit stopped: fix the findings above and stage again (git commit --no-verify skips the check)");
    }
    Ok((out.trim_end().to_owned(), failed))
}

/// Registers the `dialogue_assets` validator (spec 36 § 11, ADR-2755) when the project has dialogue
/// files, and adds the dialogue files it has findings on to `set` (they are not cooked assets).
fn register_dialogue_assets(
    registry: &mut ValidatorRegistry,
    set: &mut AssetSet,
    project: &Path,
) -> Result<()> {
    if let Some(validator) = regia_asset_pipeline::dialogue_validator::dialogue_assets_validator(project) {
        validator.augment_set(set);
        registry
            .register(Box::new(validator))
            .map_err(|e| UsageError(e.to_string()))?;
    }
    Ok(())
}

/// The `provenance_policy` validator over the project's `.meta` files, or `None` without a
/// `config/provenance.ron` (ADR-2428).
///
/// # Errors
///
/// An unreadable or invalid policy file ([`UsageError`], exit 2).
pub fn provenance_policy_validator(
    project: &Path,
) -> Result<Option<regia_asset_pipeline::provenance_policy::ProvenancePolicyValidator>> {
    use regia_asset_pipeline::provenance_policy::{ProvenancePolicyValidator, load_policy};
    let policy = load_policy(project).map_err(|e| UsageError(format!("{e} ({})", e.code())))?;
    Ok(policy.map(|policy| ProvenancePolicyValidator::for_project(project, &policy)))
}

fn source_lint_for_validate(args: &AssetsValidateArgs, scope: &Scope) -> Result<ScanOutcome> {
    let (paths, changed) = match scope {
        Scope::Folder { path, .. } => (vec![args.source.project.join(path)], None),
        Scope::Changed(list) => (Vec::new(), Some(list.clone())),
        Scope::Assets(list) | Scope::WithDependencies(list) => (
            list.iter().map(|path| args.source.project.join(path)).collect(),
            None,
        ),
        Scope::Project => (Vec::new(), None),
    };
    scan_or_cancel(
        &ScanRequest {
            project: args.source.project.clone(),
            target: args.target.clone(),
            paths,
            changed,
            strict: args.strict,
            fix: false,
            cancel_file: args.cancel_file.clone(),
            report_path: args.report.clone(),
            material_profile: None,
            skip_staging: false,
        },
        args.source.json,
    )
}

fn scan_or_cancel(request: &ScanRequest, json: bool) -> Result<ScanOutcome> {
    match source_lint_cache::scan(request) {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.code() == "content_lint_cancelled" => {
            if json {
                println!(
                    "{}",
                    json!({
                        "ok": false,
                        "error": {"code": error.code(), "message": error.to_string()}
                    })
                );
            }
            bail!(error)
        }
        Err(error) if error.code() == "content_lint_policy" || error.code() == "content_lint_target" => {
            Err(UsageError(format!("{} ({})", error, error.code())).into())
        }
        Err(error) => bail!(error),
    }
}

fn source_blocks_validate(report: &SourceLintReport, strict: bool) -> bool {
    !report.complete
        || (strict
            && report
                .findings
                .iter()
                .any(|finding| !finding.suppressed && finding.severity == FindingSeverity::Warning))
}

fn source_lint_json(outcome: &ScanOutcome, strict: bool) -> serde_json::Value {
    let warnings = outcome
        .report
        .findings
        .iter()
        .any(|finding| !finding.suppressed && finding.severity == FindingSeverity::Warning);
    json!({
        "status": if source_blocks_validate(&outcome.report, strict) {
            "failed"
        } else if warnings {
            "warnings"
        } else {
            "clean"
        },
        "report": outcome.report_path.display().to_string(),
        "evidence_key": outcome.report.evidence_key,
        "rule_ids": outcome.rule_ids,
        "findings": outcome.report.findings.len(),
        "counters": outcome.counters,
    })
}

fn source_lint_text(outcome: &ScanOutcome) -> String {
    let mut text = String::new();
    for finding in &outcome.report.findings {
        let _ = writeln!(
            text,
            "source {} {} {}: {}",
            match finding.severity {
                FindingSeverity::Warning => "warning",
                FindingSeverity::Info => "info",
            },
            finding.rule_id,
            finding.path,
            finding.message
        );
    }
    let _ = writeln!(
        text,
        "source lint {} finding(s), report {}",
        outcome.report.findings.len(),
        outcome.report_path.display()
    );
    text
}

/// Validate the cooked registry of `platform` after a cook (spec 32 § 4.5): `cook: Off | Warn | Fail` in
/// `config/validation.ron` (default `Warn`) decides what the findings mean. A missing registry or
/// `validation.ron` is not an error; under `Fail` a failing report stops the step (the cook output
/// already written stays). Used by `regia cook` and `regia package`.
///
/// # Errors
///
/// An unreadable or invalid `validation.ron` ([`UsageError`]), or `Fail` mode with a failing report.
pub fn cook_validation(project: &Path, platform: &str) -> Result<()> {
    cook_validation_for(project, platform, false)
}

/// Whether `config/validation.ron` of `project` names `cook` itself (spec 32 § 4.5: `regia package`
/// uses `Fail` only when the config says nothing).
fn config_sets_cook(project: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(project.join("config").join("validation.ron")) else {
        return false;
    };
    match ron::from_str::<ron::Value>(&text) {
        Ok(ron::Value::Map(map)) => map
            .iter()
            .any(|(key, _)| matches!(key, ron::Value::String(name) if name == "cook")),
        _ => false,
    }
}

/// [`cook_validation`] for a caller that may default the gate: with `package` set, a config that does
/// not name `cook` means `Fail` (spec 32 § 4.5), as `regia package` should run it. `regia cook` keeps
/// `Warn` as the default.
///
/// # Errors
///
/// As [`cook_validation`].
pub fn cook_validation_for(project: &Path, platform: &str, package: bool) -> Result<()> {
    let source = RegistrySource {
        project: project.to_path_buf(),
        platform: Some(platform.to_owned()),
        registry: None,
        json: false,
    };
    let mut file = read_config(&source, None)?;
    if package && !config_sets_cook(project) {
        file.cook = CookGate::Fail;
    }
    let cache = regia_asset::cooked_cache_dir(project, platform);
    // Registry integrity is mandatory even when advisory cook validation is Off or Warn. With no
    // registry file this is the legacy `asset_manifest.bin` fallback (empty when there is none).
    let fallback = super::content::project_registry(&cache)?;
    if !file.cook.runs() {
        return Ok(());
    }
    // One registry, both contracts: a required file that is missing fails (ADR-8390) even when
    // the fallback is empty; an optional missing file validates the legacy manifest when there is
    // one, and skips only when there is nothing to validate.
    let registry_path = regia_asset::registry::cache_registry_path(&cache);
    let registry = match regia_asset_pipeline::registry_bridge::open_cook_registry(&registry_path, package) {
        Ok(Some(registry)) => registry,
        Ok(None) if fallback.is_empty() => {
            eprintln!("validate: no asset registry for {platform}; skipped");
            return Ok(());
        }
        Ok(None) => fallback,
        Err(message) => bail!("{message}"),
    };
    let mut set =
        AssetSet::from_registry_with_roots(&registry, &file.roots).with_ui_layouts(&project.join("assets"));
    let settings = RunSettings {
        trigger: Trigger::Cook,
        exclude: file.exclude.clone(),
        ..RunSettings::default()
    };
    let mut validators = ValidatorRegistry::with_rules(&file);
    register_dialogue_assets(&mut validators, &mut set, project)?;
    let report = validators.run_with(&set, &settings);
    if report.summary.failed > 0 || report.summary.warnings > 0 {
        eprintln!("{}", report_text(&report));
    }
    match cook_gate(file.cook, Some(&report), false) {
        CookGateOutcome::Stop { failed } => {
            bail!("asset validation failed for {failed} asset(s) (cook: Fail in config/validation.ron)")
        }
        CookGateOutcome::Skipped | CookGateOutcome::Continue { .. } => Ok(()),
    }
}

/// One line per finding, `<severity> <code> <asset>: <text>`, then a summary line.
#[must_use]
pub fn report_text(report: &ValidationReport) -> String {
    let mut out = String::new();
    for (row, f) in report.all_findings() {
        let _ = writeln!(out, "{} {} {}: {}", f.severity.label(), f.code, row.asset, f.text);
    }
    let s = &report.summary;
    let _ = write!(
        out,
        "{} checked of {} requested: {} passed, {} failed, {} not validated, {} with warnings{}",
        s.checked,
        s.requested,
        s.passed,
        s.failed,
        s.not_validated,
        s.warnings,
        if s.cap_reached { " (cap reached)" } else { "" }
    );
    out
}

/// The text (or JSON) of `regia assets graph`.
///
/// # Errors
///
/// No registry or a bad query (the message names the stable code).
pub fn graph(args: &AssetsGraphArgs) -> Result<String> {
    let set = load_set(&args.source, &ValidationFile::default())?;
    let g = DependencyGraph::build(&set);
    let query = GraphQuery {
        roots: args.assets.clone(),
        combine: if args.intersect {
            Combine::Intersect
        } else {
            Combine::Union
        },
        referrer_depth: args.referrers,
        dependency_depth: args.dependencies,
        max_per_column: args.width,
        ..GraphQuery::default()
    };
    let result = graph_query(&set, &g, &query).map_err(|e| audit_error(&e))?;
    if args.tree {
        let mut out = String::new();
        for root in &result.roots {
            for (side, depth) in [
                (Side::Dependencies, args.dependencies),
                (Side::Referrers, args.referrers),
            ] {
                if depth == 0 {
                    continue;
                }
                let tree =
                    reference_tree(&set, &g, root, side, depth, 2_000, true).map_err(|e| audit_error(&e))?;
                let _ = writeln!(out, "{}:", side.label());
                out.push_str(&tree.text);
            }
        }
        return Ok(out.trim_end().to_owned());
    }
    if args.source.json {
        return Ok(serde_json::to_string_pretty(&result)?);
    }
    let mut out = String::new();
    let _ = writeln!(out, "roots: {}", result.roots.join(", "));
    for c in &result.columns {
        let _ = writeln!(
            out,
            "{} hop {} ({} of {}):",
            c.side.label(),
            c.hop,
            c.nodes.len(),
            c.total
        );
        for n in &c.nodes {
            let _ = writeln!(out, "  {n}");
        }
        if c.hidden > 0 {
            let _ = writeln!(out, "  ... {} more", c.hidden);
        }
    }
    for m in &result.missing {
        let _ = writeln!(out, "missing: {m}");
    }
    Ok(out.trim_end().to_owned())
}

/// The text (or JSON) of `regia assets size-map`.
///
/// # Errors
///
/// No registry, an unknown asset or a grouping that does not parse.
pub fn size_map_text(args: &AssetsSizeMapArgs) -> Result<String> {
    let set = load_set(&args.source, &ValidationFile::default())?;
    let g = DependencyGraph::build(&set);
    let group_by = GroupBy::parse(&args.group).map_err(|e| audit_error(&e))?;
    let map = size_map(
        &set,
        &g,
        &SizeMapQuery {
            roots: args.assets.clone(),
            group_by,
            top: args.top,
        },
    )
    .map_err(|e| audit_error(&e))?;
    if args.source.json {
        return Ok(serde_json::to_string_pretty(&map)?);
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} assets, {} bytes total ({} roots, {} exclusive, {} shared)",
        map.closure_assets, map.total_bytes, map.roots_bytes, map.exclusive_bytes, map.shared_bytes
    );
    for r in &map.rows {
        let _ = writeln!(
            out,
            "{:>12} {:>12} {:>12} {:>6}  {}",
            r.bytes, r.exclusive_bytes, r.shared_bytes, r.assets, r.name
        );
    }
    for m in &map.missing {
        let _ = writeln!(out, "missing: {m}");
    }
    Ok(out.trim_end().to_owned())
}

/// Write `regia assets audit`'s table; returns the number of data rows.
///
/// # Errors
///
/// No registry or the file cannot be written.
pub fn audit(args: &AssetsAuditArgs) -> Result<usize> {
    let set = load_set(&args.source, &ValidationFile::default())?;
    let g = DependencyGraph::build(&set);
    let mut selection = args.assets.clone();
    if let Some(folder) = &args.folder {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        selection.extend(
            set.assets
                .iter()
                .filter(|a| a.path.starts_with(&prefix))
                .map(|a| a.path.clone()),
        );
        if selection.is_empty() {
            bail!("no assets under {folder}");
        }
    }
    let csv = audit_csv(&set, &g, &selection);
    let rows = csv.lines().count().saturating_sub(1);
    let tmp = args.csv.with_extension("csv.tmp");
    std::fs::write(&tmp, &csv).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &args.csv).with_context(|| format!("renaming to {}", args.csv.display()))?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetId;
    use regia_asset::registry::{AssetRegistry, RegistryEntry};
    use regia_core::id::Guid;
    use std::collections::BTreeMap;

    fn rec(n: u128, path: &str, ty: &str, deps: &[u128], size: u64) -> RegistryEntry {
        RegistryEntry {
            id: AssetId(Guid::from_u128(n)),
            path: path.into(),
            type_path: ty.into(),
            dependencies: deps.iter().map(|d| AssetId(Guid::from_u128(*d))).collect(),
            tags: BTreeMap::new(),
            size,
        }
    }

    fn project(config: Option<&str>) -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir("cli-assets-validate");
        let file = dir.join("base.registry.bin");
        AssetRegistry::from_entries([
            rec(1, "assets/levels/a.scene", "x::SceneAsset", &[2, 3], 10),
            rec(2, "assets/tex/rock.png", "x::CookedTexture", &[], 1000),
            rec(3, "assets/tex/Bad Name.png", "x::CookedTexture", &[], 500),
            rec(4, "assets/tex/loose.png", "x::CookedTexture", &[], 7),
        ])
        .write_to(&file)
        .expect("write registry");
        if let Some(text) = config {
            std::fs::create_dir_all(dir.join("config")).expect("config dir");
            std::fs::write(dir.join("config/validation.ron"), text).expect("write config");
        }
        (dir, file)
    }

    fn source(dir: &Path, file: &Path, json: bool) -> RegistrySource {
        RegistrySource {
            project: dir.to_path_buf(),
            platform: None,
            registry: Some(file.to_path_buf()),
            json,
        }
    }

    fn vargs(dir: &Path, file: &Path, json: bool) -> AssetsValidateArgs {
        AssetsValidateArgs {
            source: source(dir, file, json),
            assets: Vec::new(),
            folder: None,
            recursive: false,
            with_deps: false,
            changed: None,
            strict: false,
            max: None,
            config: None,
            staged: false,
            target: "default".to_owned(),
            report: None,
            cancel_file: None,
        }
    }

    #[test]
    fn validate_runs_the_provenance_policy_only_with_a_policy_file() {
        let (dir, file) = project(None);
        std::fs::create_dir_all(dir.join("assets/tex")).expect("assets");
        std::fs::write(dir.join("assets/tex/rock.png"), b"png").expect("source");
        let meta = regia_asset::MetaSidecar::new("png", AssetId::new_random());
        std::fs::write(
            dir.join("assets/tex/rock.png.meta"),
            meta.to_ron_string().expect("ron"),
        )
        .expect("meta");
        let (text, _) = validate(&vargs(&dir, &file, false)).expect("validates");
        assert!(!text.contains("provenance_policy"), "no policy, no rows: {text}");
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(
            dir.join("config/provenance.ron"),
            "(fields: [(name: \"licence\")], kinds: { \"png\": (required: [\"licence\"]) })",
        )
        .expect("policy");
        let (text, failed) = validate(&vargs(&dir, &file, false)).expect("validates");
        assert!(failed);
        assert!(
            text.contains("error provenance_policy assets/tex/rock.png: missing_field"),
            "{text}"
        );
        std::fs::write(dir.join("config/provenance.ron"), "(kinds: 3)").expect("bad policy");
        let err = validate(&vargs(&dir, &file, false)).expect_err("bad policy");
        assert!(err.downcast_ref::<UsageError>().is_some(), "exit 2: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_reports_findings_and_fails() {
        let (dir, file) = project(None);
        let (text, failed) = validate(&vargs(&dir, &file, false)).expect("validates");
        assert!(failed);
        assert!(
            text.contains("error validate_name_invalid assets/tex/Bad Name.png"),
            "{text}"
        );
        assert!(
            text.contains("error validate_asset_orphaned assets/tex/loose.png"),
            "{text}"
        );
        assert!(text.contains("checked of 4 requested"), "{text}");
        let (json, _) = validate(&vargs(&dir, &file, true)).expect("json");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(v["exit_failed"], true);
        // Deterministic.
        assert_eq!(json, validate(&vargs(&dir, &file, true)).expect("again").0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_runs_and_rule_config_apply() {
        let config = r#"(exclude: ["assets/tex/loose.png"], rules: [SizeBudget(id: "tex", kind: Texture, max_bytes: 600, severity: Warning)])"#;
        let (dir, file) = project(Some(config));
        let mut a = vargs(&dir, &file, false);
        a.assets = vec!["assets/tex/rock.png".into()];
        let (text, failed) = validate(&a).expect("validates");
        assert!(!failed, "{text}");
        assert!(
            text.contains("warning validate_size_over_budget assets/tex/rock.png"),
            "{text}"
        );
        assert!(text.contains("1 checked of 1 requested"), "{text}");
        // --strict fails on the warning.
        a.strict = true;
        assert!(validate(&a).expect("strict").1);
        // folder scope honors exclude.
        let mut f = vargs(&dir, &file, true);
        f.folder = Some("assets/tex".into());
        let (json, _) = validate(&f).expect("folder");
        let v: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(v["summary"]["skipped_excluded"], 1);
        assert_eq!(v["summary"]["checked"], 2);
        // with-deps pulls in the scene's dependencies
        let mut w = vargs(&dir, &file, true);
        w.assets = vec!["assets/levels/a.scene".into()];
        w.with_deps = true;
        let v: serde_json::Value = serde_json::from_str(&validate(&w).expect("deps").0).expect("json");
        assert_eq!(v["summary"]["checked"], 3);
        // --max cuts
        let mut m = vargs(&dir, &file, true);
        m.max = Some(1);
        let v: serde_json::Value = serde_json::from_str(&validate(&m).expect("max").0).expect("json");
        assert_eq!(v["summary"]["cap_reached"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cook_validation_modes() {
        let (dir, _) = project(None);
        // `project()` wrote the registry to a custom file; put one where the cook leaves it.
        let platform = "linux";
        let cache = regia_asset::cooked_cache_dir(&dir, platform);
        std::fs::create_dir_all(&cache).expect("cache dir");
        let registry = AssetRegistry::read_from(&dir.join("base.registry.bin"))
            .expect("reads")
            .expect("present");
        registry
            .write_to(&regia_asset::registry::cache_registry_path(&cache))
            .expect("writes");
        // Default (Warn): findings print, the step succeeds.
        cook_validation(&dir, platform).expect("warn continues");
        std::fs::create_dir_all(dir.join("config")).expect("config dir");
        std::fs::write(dir.join("config/validation.ron"), "(cook: Fail)").expect("config");
        let err = cook_validation(&dir, platform).expect_err("fail mode stops");
        assert!(err.to_string().contains("asset validation failed"), "{err:#}");
        std::fs::write(dir.join("config/validation.ron"), "(cook: Off)").expect("config");
        cook_validation(&dir, platform).expect("off runs nothing");
        std::fs::write(dir.join("config/validation.ron"), "(cook: Fail)").expect("config");
        // No registry for another platform: skipped, not an error.
        cook_validation(&dir, "windows").expect("no registry is skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes the `project()` registry's entries as a legacy `asset_manifest.bin` (no registry
    /// file) into `platform`'s cooked cache.
    fn write_legacy_manifest(dir: &Path, file: &Path, platform: &str) {
        let cache = regia_asset::cooked_cache_dir(dir, platform);
        std::fs::create_dir_all(&cache).expect("cache");
        let registry = AssetRegistry::read_from(file).expect("reads").expect("present");
        let manifest = regia_asset::AssetManifest::from_map(
            registry
                .entries()
                .map(|e| {
                    (
                        e.path.clone(),
                        regia_asset::ManifestEntry {
                            path: e.path.clone(),
                            id: e.id,
                            type_path: e.type_path.clone(),
                        },
                    )
                })
                .collect(),
        );
        manifest.write_to_cache(&cache).expect("manifest");
    }

    /// The integ-v06s-final merge audit, finding 1: with no registry file, `cook: Fail` must
    /// validate the legacy manifest fallback, not skip it.
    #[test]
    fn cook_fail_checks_the_legacy_manifest_when_the_registry_file_is_absent() {
        let (dir, file) = project(Some("(cook: Fail)"));
        let platform = "linux";
        write_legacy_manifest(&dir, &file, platform);
        let cache = regia_asset::cooked_cache_dir(&dir, platform);
        let loaded = super::super::content::project_registry(&cache).expect("fallback");
        assert!(!loaded.is_empty(), "the manifest fallback is the registry");
        let err =
            cook_validation_for(&dir, platform, false).expect_err("Fail must see assets/tex/Bad Name.png");
        assert!(err.to_string().contains("asset validation failed"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The package gate requires the registry file (ADR-8390): a missing one fails whether or not
    /// a legacy manifest is there, and an optional missing file with no manifest is a skip.
    #[test]
    fn package_requires_the_registry_file_with_or_without_a_manifest() {
        let (dir, file) = project(None);
        let err = cook_validation_for(&dir, "linux", true).expect_err("no registry, no manifest");
        assert!(
            err.to_string().contains("required asset registry is missing"),
            "{err:#}"
        );
        cook_validation_for(&dir, "linux", false).expect("optional and nothing to validate: skipped");
        write_legacy_manifest(&dir, &file, "linux");
        let err = cook_validation_for(&dir, "linux", true).expect_err("a manifest is not the registry");
        assert!(
            err.to_string().contains("required asset registry is missing"),
            "{err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_reports_dialogue_findings_with_their_codes() {
        let (dir, file) = project(None);
        let dialogue = dir.join("assets/dialogue");
        std::fs::create_dir_all(&dialogue).expect("dialogue dir");
        std::fs::write(
            dialogue.join("intro.dialogue.ron"),
            r#"(format_version: 1, name: "intro", lines: [(id: "hello", speaker: "ghost")])"#,
        )
        .expect("dialogue");
        let (text, failed) = validate(&vargs(&dir, &file, false)).expect("validates");
        assert!(failed);
        assert!(
            text.contains("error dialogue_unknown_voice assets/dialogue/intro.dialogue.ron:"),
            "{text}"
        );
        // Without the dialogue file the validator is not registered and nothing is reported.
        std::fs::remove_file(dialogue.join("intro.dialogue.ron")).expect("remove");
        let (text, _) = validate(&vargs(&dir, &file, false)).expect("validates");
        assert!(!text.contains("dialogue"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_defaults_the_cook_gate_to_fail_unless_the_config_names_it() {
        let (dir, _) = project(None);
        let platform = "linux";
        let cache = regia_asset::cooked_cache_dir(&dir, platform);
        std::fs::create_dir_all(&cache).expect("cache dir");
        let registry = AssetRegistry::read_from(&dir.join("base.registry.bin"))
            .expect("reads")
            .expect("present");
        registry
            .write_to(&regia_asset::registry::cache_registry_path(&cache))
            .expect("writes");
        // `regia cook` keeps Warn as the default; `regia package` (spec 32 § 4.5) means Fail.
        cook_validation_for(&dir, platform, false).expect("cook warns");
        let err = cook_validation_for(&dir, platform, true).expect_err("package fails by default");
        assert!(err.to_string().contains("asset validation failed"), "{err:#}");
        std::fs::create_dir_all(dir.join("config")).expect("config dir");
        std::fs::write(dir.join("config/validation.ron"), "(cook: Warn)").expect("config");
        cook_validation_for(&dir, platform, true).expect("a config that names cook wins");
        std::fs::write(dir.join("config/validation.ron"), "(validate_on_save: false)").expect("config");
        assert!(
            cook_validation_for(&dir, platform, true).is_err(),
            "other keys do not name cook"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn staged_args(project: &Path, registry: Option<&Path>, json: bool) -> AssetsValidateArgs {
        let mut args = vargs(project, project, json);
        args.source.registry = registry.map(Path::to_path_buf);
        if registry.is_none() {
            // A registry path that does not exist: the registry part is skipped.
            args.source.registry = Some(project.join("no-such.registry.bin"));
        }
        args.staged = true;
        args
    }

    /// P-153 (ADR-7789): `--staged` reads the index, not the working tree: a broken staged asset
    /// fails, the same file fixed but not staged still fails, the staged fix passes.
    #[test]
    fn validate_staged_checks_the_index_not_the_working_tree() {
        use crate::commands::hooks::tests::{git_ok, repo};
        let dir = repo("cli-validate-staged");
        let game = dir.join("game");
        std::fs::create_dir_all(game.join("assets/mat")).expect("dirs");
        let good = "Material(base_color: (1.0, 1.0, 1.0, 1.0))";
        let broken = "Material(base_color: (1.0, ";
        let path = game.join("assets/mat/rock.material.ron");
        std::fs::write(&path, good).expect("good");
        // Outside the project: never checked.
        std::fs::write(dir.join("outside.ron"), broken).expect("outside");
        git_ok(&dir, &["add", "-A"]);
        let (text, failed) = validate(&staged_args(&game, None, false)).expect("runs");
        assert!(!failed, "{text}");
        assert!(
            text.contains("staged: 1 file(s) under the project, 1 parsed"),
            "{text}"
        );
        assert!(text.contains("registry validators skipped"), "{text}");

        std::fs::write(&path, broken).expect("broken");
        git_ok(&dir, &["add", "-A"]);
        let (text, failed) = validate(&staged_args(&game, None, false)).expect("runs");
        assert!(failed, "{text}");
        assert!(
            text.contains(
                "error staged_parse_error assets/mat/rock.material.ron: the staged file does not parse"
            ),
            "{text}"
        );
        assert!(text.contains("commit stopped"), "{text}");
        // Fixed in the working tree only: the commit would still hold the broken file.
        std::fs::write(&path, good).expect("fixed, unstaged");
        let (_, failed) = validate(&staged_args(&game, None, false)).expect("runs");
        assert!(failed, "the index is what is checked");
        git_ok(&dir, &["add", "-A"]);
        let (json, failed) = validate(&staged_args(&game, None, true)).expect("runs");
        assert!(!failed, "{json}");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(value["exit_failed"], false);
        assert_eq!(
            value["staged"]["files"][0]["project_path"],
            "assets/mat/rock.material.ron"
        );
        assert_eq!(
            value["staged"]["changed_assets"][0],
            "assets/mat/rock.material.ron"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With a registry, the validators run over the staged paths only (a `.meta` names its asset).
    #[test]
    fn validate_staged_runs_the_validators_over_the_staged_paths() {
        use crate::commands::hooks::tests::{git_ok, repo};
        let dir = repo("cli-validate-staged-registry");
        let file = dir.join("base.registry.bin");
        AssetRegistry::from_entries([
            rec(1, "assets/levels/a.scene", "x::SceneAsset", &[2, 3], 10),
            rec(2, "assets/tex/rock.png", "x::CookedTexture", &[], 1000),
            rec(3, "assets/tex/Bad Name.png", "x::CookedTexture", &[], 500),
            rec(4, "assets/tex/loose.png", "x::CookedTexture", &[], 7),
        ])
        .write_to(&file)
        .expect("write registry");
        std::fs::create_dir_all(dir.join("assets/tex")).expect("dirs");
        let meta = regia_asset::MetaSidecar::new("png", AssetId::new_random());
        let meta = meta.to_ron_string().expect("ron");
        std::fs::write(dir.join("assets/tex/rock.png.meta"), &meta).expect("meta");
        git_ok(&dir, &["add", "assets"]);
        let (text, failed) = validate(&staged_args(&dir, Some(&file), false)).expect("runs");
        assert!(!failed, "rock.png is clean and loose.png is not staged: {text}");
        assert!(!text.contains("loose.png"), "{text}");
        assert!(text.contains("checked of 1 requested"), "{text}");

        std::fs::write(dir.join("assets/tex/Bad Name.png.meta"), &meta).expect("meta");
        git_ok(&dir, &["add", "assets"]);
        let (text, failed) = validate(&staged_args(&dir, Some(&file), false)).expect("runs");
        assert!(failed, "{text}");
        assert!(
            text.contains("error validate_name_invalid assets/tex/Bad Name.png"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_staged_outside_a_repository_is_a_usage_error() {
        let dir = regia_test_utils::unique_temp_dir("cli-validate-staged-norepo");
        if regia_asset_pipeline::precommit::repo_root(&dir).is_ok() {
            eprintln!(
                "skipped: the temp dir {} is inside a git work tree",
                dir.display()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let err = validate(&staged_args(&dir, None, false)).expect_err("no repo");
        assert!(err.downcast_ref::<UsageError>().is_some(), "exit 2: {err}");
        assert!(err.to_string().starts_with("not_a_git_repo:"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_config_is_a_usage_error() {
        let (dir, file) = project(Some("(bogus: 1)"));
        let err = validate(&vargs(&dir, &file, false)).expect_err("bad config");
        assert!(err.is::<UsageError>(), "{err:#}");
        let mut a = vargs(&dir, &file, false);
        a.config = Some(dir.join("missing.ron"));
        assert!(validate(&a).expect_err("missing").is::<UsageError>());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graph_size_map_and_audit_commands() {
        let (dir, file) = project(None);
        let g = graph(&AssetsGraphArgs {
            source: source(&dir, &file, false),
            assets: vec!["assets/levels/a.scene".into()],
            referrers: 1,
            dependencies: 1,
            width: 50,
            intersect: false,
            tree: false,
        })
        .expect("graph");
        assert!(g.contains("dependencies hop 1 (2 of 2):"), "{g}");
        let t = graph(&AssetsGraphArgs {
            source: source(&dir, &file, false),
            assets: vec!["assets/levels/a.scene".into()],
            referrers: 0,
            dependencies: 2,
            width: 50,
            intersect: false,
            tree: true,
        })
        .expect("tree");
        assert!(t.contains("dep assets/tex/rock.png"), "{t}");
        let bad = graph(&AssetsGraphArgs {
            source: source(&dir, &file, false),
            assets: vec!["assets/levels/a.scene".into()],
            referrers: 9,
            dependencies: 1,
            width: 50,
            intersect: false,
            tree: false,
        })
        .expect_err("depth");
        assert!(format!("{bad:#}").contains("graph_bad_depth"));
        let m = size_map_text(&AssetsSizeMapArgs {
            source: source(&dir, &file, true),
            assets: vec!["assets/levels/a.scene".into()],
            group: "kind".into(),
            top: 200,
        })
        .expect("size map");
        let v: serde_json::Value = serde_json::from_str(&m).expect("json");
        assert_eq!(v["total_bytes"], 1510);
        assert!(
            size_map_text(&AssetsSizeMapArgs {
                source: source(&dir, &file, true),
                assets: vec!["assets/levels/a.scene".into()],
                group: "nope".into(),
                top: 1,
            })
            .is_err()
        );
        let out = dir.join("audit.csv");
        let rows = audit(&AssetsAuditArgs {
            source: source(&dir, &file, false),
            folder: Some("assets/tex".into()),
            assets: Vec::new(),
            csv: out.clone(),
        })
        .expect("audit");
        assert_eq!(rows, 3);
        let text = std::fs::read_to_string(&out).expect("csv written");
        assert!(text.starts_with("path,kind,size,"));
        assert!(!dir.join("audit.csv.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
