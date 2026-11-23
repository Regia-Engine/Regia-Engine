// SPDX-License-Identifier: MPL-2.0
//! The Project Health panel (Window ▸ Project Health, ADR-7789): one ranked list built from the
//! checks the project already has, each row with a recommended action and a link.
//!
//! The sources are:
//!
//! - the asset validators over the loaded registry with the project's `config/validation.ron`
//!   (`crate::asset_panels::project_rules`): one row per validator and finding code, with how
//!   many assets it names; missing references and the texture, mesh and size budgets of that file
//!   get their own categories;
//! - unreferenced large assets: nothing references them, they are no root (a scene or a listed
//!   root), and their cooked size is at least [`UNREFERENCED_LARGE_BYTES`];
//! - the cook's import warnings (`import_reports.json` in the cooked cache);
//! - `regia doctor --json` findings (toolchains and SDKs), run as the CLI the editor finds
//!   (`crate::bake_runner::cli_program`);
//! - autosave and recovery leftovers (`crate::autosave::recovery_candidates`).
//!
//! Rows rank errors first, then warnings, then notes; within a severity by category (the order of
//! [`HealthCategory`]), then by title. *Re-run* gathers the inputs on the editor thread and runs the
//! checks as an editor job (`project_health:<n>`, cancellable like every [`WorkerJob`]); its end
//! fills the panel and the Problems panel (so a row's Problems link shows the same findings).
//!
//! The agent surface is `editor.project_health { action: run | state | open }`
//! (`book/agents/project-health.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::asset_graph::{AssetKind, AssetSet, DependencyGraph};
use regia_asset_pipeline::validation_rules::ValidationFile;
use regia_asset_pipeline::validators::{
    RunSettings, Scope, Severity, Trigger, ValidationReport, ValidatorRegistry,
};
use regia_ecs::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};

use crate::asset_panels::{AssetPanels, PanelSource, SEVERITIES, severity_name};
use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::viewport_agent::Handler;

/// The cooked size from which an asset nothing references is listed (1 MiB).
pub const UNREFERENCED_LARGE_BYTES: u64 = 1024 * 1024;

/// Unreferenced assets listed one by one (largest first); the rest are one summary row.
pub const UNREFERENCED_LISTED: usize = 20;

/// Example assets named in a validator row's detail.
const EXAMPLES: usize = 3;

/// What a row is about. Rows of one severity sort in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthCategory {
    /// A referenced path that is not in the project.
    MissingReference,
    /// Over a texture, mesh or size budget of `config/validation.ron`.
    Budget,
    /// Another validator finding.
    Validation,
    /// A toolchain or SDK finding of `regia doctor`.
    Toolchain,
    /// An import warning of the last cook.
    CookWarning,
    /// A large asset nothing references.
    UnreferencedLarge,
    /// An autosave left by a crash or an unsaved session.
    Recovery,
}

impl HealthCategory {
    /// The wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingReference => "missing_reference",
            Self::Budget => "budget",
            Self::Validation => "validation",
            Self::Toolchain => "toolchain",
            Self::CookWarning => "cook_warning",
            Self::UnreferencedLarge => "unreferenced_large",
            Self::Recovery => "recovery",
        }
    }

    /// The panel's label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::MissingReference => "Missing reference",
            Self::Budget => "Budget",
            Self::Validation => "Validation",
            Self::Toolchain => "Toolchain",
            Self::CookWarning => "Cook warning",
            Self::UnreferencedLarge => "Unreferenced",
            Self::Recovery => "Recovery",
        }
    }
}

/// Where a row's link goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HealthLink {
    /// Nothing to open.
    None,
    /// Open the asset (or source file) as the Content Browser does (project-relative path).
    Asset {
        /// The path.
        path: String,
    },
    /// Open the Problems panel showing this validator's findings.
    Problems {
        /// The validator.
        validator: String,
    },
    /// A command that fixes it, for the user to run (copied, never run by the editor).
    Command {
        /// The command line.
        command: String,
    },
}

/// One ranked row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthRow {
    /// How serious.
    pub severity: Severity,
    /// What it is about.
    pub category: HealthCategory,
    /// One line.
    pub title: String,
    /// What was found.
    pub detail: String,
    /// What to do about it.
    pub action: String,
    /// Where the link goes.
    pub link: HealthLink,
    /// How many findings, assets or files the row stands for.
    pub count: usize,
}

/// Whether one source of rows ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthSource {
    /// `validators`, `unreferenced_large`, `cook_warnings`, `doctor` or `recovery`.
    pub source: &'static str,
    /// It ran.
    pub ran: bool,
    /// Why not, or what it covered.
    pub note: String,
}

/// The outcome of a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HealthReport {
    /// The rows, ranked.
    pub rows: Vec<HealthRow>,
    /// The sources and whether each ran.
    pub sources: Vec<HealthSource>,
    /// The validators' report (it fills the Problems panel too).
    #[serde(skip)]
    pub validation: Option<ValidationReport>,
    /// The run was cancelled part way (the rows are what ran).
    pub cancelled: bool,
}

impl HealthReport {
    /// Rows of `severity`.
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.rows.iter().filter(|r| r.severity == severity).count()
    }
}

/// What a run reads, gathered on the editor thread.
#[derive(Debug, Clone, Default)]
pub struct HealthInputs {
    /// The project root.
    pub root: Option<PathBuf>,
    /// The loaded registry as an asset set.
    pub set: Option<AssetSet>,
    /// `config/validation.ron` (or the error reading it).
    pub rules: Option<Result<ValidationFile, String>>,
    /// The `regia` CLI for `regia doctor --json` (`None`: the toolchain checks are skipped).
    pub cli: Option<PathBuf>,
    /// The cooked platform whose import reports are read.
    pub platform: String,
}

fn severity_rank(severity: Severity) -> usize {
    SEVERITIES
        .iter()
        .position(|s| *s == severity)
        .unwrap_or(SEVERITIES.len())
}

/// Ranks `rows`: severity, category, title.
pub fn rank(rows: &mut [HealthRow]) {
    rows.sort_by(|a, b| {
        (severity_rank(a.severity), a.category, &a.title).cmp(&(
            severity_rank(b.severity),
            b.category,
            &b.title,
        ))
    });
}

fn category_of(validator: &str) -> HealthCategory {
    match validator {
        "missing_references" | "ui_image_missing" => HealthCategory::MissingReference,
        "texture_size_limits" | "mesh_triangle_budget" | "size_budget" => HealthCategory::Budget,
        _ => HealthCategory::Validation,
    }
}

/// One validator row being gathered: severity, assets, the first hint and the first text.
type FindingGroup = (Severity, Vec<String>, Option<String>, String);

/// The rows of a validation report: one per validator, finding code and severity.
#[must_use]
pub fn validation_rows(report: &ValidationReport) -> Vec<HealthRow> {
    let mut groups: BTreeMap<(String, String, usize), FindingGroup> = BTreeMap::new();
    for (result, finding) in report.all_findings() {
        let key = (
            result.validator.clone(),
            finding.code.clone(),
            severity_rank(finding.severity),
        );
        let entry = groups.entry(key).or_insert_with(|| {
            (
                finding.severity,
                Vec::new(),
                finding.hint.clone(),
                finding.text.clone(),
            )
        });
        entry.1.push(result.asset.clone());
    }
    groups
        .into_iter()
        .map(|((validator, code, _), (severity, assets, hint, text))| {
            let category = category_of(&validator);
            let count = assets.len();
            let examples: Vec<&str> = assets.iter().take(EXAMPLES).map(String::as_str).collect();
            let more = if count > EXAMPLES {
                format!(" and {} more", count - EXAMPLES)
            } else {
                String::new()
            };
            let action = hint.unwrap_or_else(|| match category {
                HealthCategory::MissingReference => {
                    "Restore the missing file or point the reference at an existing asset".to_owned()
                }
                HealthCategory::Budget => {
                    "Reduce the asset, or raise the limit in config/validation.ron".to_owned()
                }
                _ => "Open the findings in Problems and fix each asset".to_owned(),
            });
            HealthRow {
                severity,
                category,
                title: format!("{validator}: {code} ({count})"),
                detail: format!("{}{more}: {text}", examples.join(", ")),
                action,
                link: HealthLink::Problems { validator },
                count,
            }
        })
        .collect()
}

/// Large assets nothing references and no root reaches by name: largest first, the first
/// [`UNREFERENCED_LISTED`] one row each, the rest one row.
#[must_use]
pub fn unreferenced_rows(set: &AssetSet, extra_roots: &[String], threshold: u64) -> Vec<HealthRow> {
    let graph = DependencyGraph::build(set);
    let mut found: Vec<(&str, u64)> = set
        .assets
        .iter()
        .filter(|a| a.kind != AssetKind::Scene && a.cooked_bytes >= threshold)
        .filter(|a| !set.roots.contains(&a.path) && !extra_roots.contains(&a.path))
        .filter(|a| graph.referrers(&a.path).is_empty())
        .map(|a| (a.path.as_str(), a.cooked_bytes))
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut rows: Vec<HealthRow> = found
        .iter()
        .take(UNREFERENCED_LISTED)
        .map(|(path, bytes)| HealthRow {
            severity: Severity::Warning,
            category: HealthCategory::UnreferencedLarge,
            title: format!(
                "{path} is unreferenced ({})",
                crate::asset_panels::format_bytes(*bytes)
            ),
            detail: "Nothing references it and it is no root, yet it is cooked and shipped".to_owned(),
            action: "Delete it, or reference it from a level (Reference Graph shows its referrers)"
                .to_owned(),
            link: HealthLink::Asset {
                path: (*path).to_owned(),
            },
            count: 1,
        })
        .collect();
    if found.len() > UNREFERENCED_LISTED {
        let rest = &found[UNREFERENCED_LISTED..];
        let bytes: u64 = rest.iter().map(|(_, b)| b).sum();
        rows.push(HealthRow {
            severity: Severity::Info,
            category: HealthCategory::UnreferencedLarge,
            title: format!(
                "{} more unreferenced assets ({})",
                rest.len(),
                crate::asset_panels::format_bytes(bytes)
            ),
            detail: rest
                .iter()
                .take(EXAMPLES)
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(", "),
            action: "Review them in the Size Map and delete what the game never loads".to_owned(),
            link: HealthLink::None,
            count: rest.len(),
        });
    }
    rows
}

/// One row per imported source with cook warnings (`import_reports.json` of `cache_dir`).
#[must_use]
pub fn cook_rows(cache_dir: &Path) -> Vec<HealthRow> {
    let reports = regia_asset_pipeline::import_report::ImportReports::load(cache_dir);
    reports
        .entries
        .iter()
        .filter(|(_, entry)| !entry.warnings.is_empty())
        .map(|(source, entry)| HealthRow {
            severity: Severity::Warning,
            category: HealthCategory::CookWarning,
            title: format!("{source}: {} import warning(s)", entry.warnings.len()),
            detail: entry.warnings.join("; "),
            action: format!(
                "Adjust the source in its tool so the {} importer keeps it, then cook again",
                entry.importer
            ),
            link: HealthLink::Asset { path: source.clone() },
            count: entry.warnings.len(),
        })
        .collect()
}

/// The rows of a `regia doctor --json` document: one per `warn` or `fail` check.
#[must_use]
pub fn doctor_rows(doc: &Value) -> Vec<HealthRow> {
    doc.get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|check| {
            let severity = match check.get("status").and_then(Value::as_str)? {
                "fail" => Severity::Error,
                "warn" => Severity::Warning,
                _ => return None,
            };
            let text = |key: &str| {
                check
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let fix = text("fix");
            Some(HealthRow {
                severity,
                category: HealthCategory::Toolchain,
                title: format!("{}: {} ({})", text("section"), text("name"), text("code")),
                detail: text("detail"),
                action: if fix.is_empty() {
                    "See `regia doctor` for the fix".to_owned()
                } else {
                    fix.clone()
                },
                link: if fix.is_empty() {
                    HealthLink::None
                } else {
                    HealthLink::Command { command: fix }
                },
                count: 1,
            })
        })
        .collect()
}

/// Runs `cli doctor --json --project root` and reads its document (the command exits non-zero
/// when a check fails, but still prints it).
fn run_doctor(cli: &Path, root: &Path) -> Result<Value, String> {
    let output = std::process::Command::new(cli)
        .args(["doctor", "--json", "--project"])
        .arg(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{} did not run: {e}", cli.display()))?;
    serde_json::from_slice(&output.stdout).map_err(|e| {
        format!(
            "{} doctor printed no JSON ({e}): {}",
            cli.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })
}

/// One row per autosave left in the project: a recovery still on offer is a warning, an older
/// leftover a note.
#[must_use]
pub fn recovery_rows(root: &Path) -> Vec<HealthRow> {
    crate::autosave::recovery_candidates(root)
        .into_iter()
        .map(|candidate| {
            let name = candidate
                .autosave
                .strip_prefix(root)
                .unwrap_or(&candidate.autosave)
                .to_string_lossy()
                .replace('\\', "/");
            let level = candidate
                .original
                .as_ref()
                .and_then(|p| p.strip_prefix(root).ok())
                .map_or_else(
                    || "an untitled level".to_owned(),
                    |p| p.to_string_lossy().replace('\\', "/"),
                );
            let offered = crate::autosave::should_offer_recovery(&candidate);
            HealthRow {
                severity: if offered {
                    Severity::Warning
                } else {
                    Severity::Info
                },
                category: HealthCategory::Recovery,
                title: if offered {
                    format!("Unsaved work of {level} is waiting in {name}")
                } else {
                    format!("Old autosave {name} of {level}")
                },
                detail: if offered {
                    "The autosave is newer than the level: a crash or a session that ended without saving"
                        .to_owned()
                } else {
                    "The level was saved after it; the autosave is a leftover".to_owned()
                },
                action: if offered {
                    "Restore or discard it in the recovery window (it opens with the project)".to_owned()
                } else {
                    "Delete the file".to_owned()
                },
                link: HealthLink::Asset { path: name },
                count: 1,
            }
        })
        .collect()
}

/// Runs every check of `inputs` (the worker's body), stopping between sources when `cancelled`
/// says so. `progress` hears each source as it starts.
pub fn collect(
    inputs: &HealthInputs,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(f64, &str),
) -> HealthReport {
    let _span = tracing::info_span!("project_health").entered();
    let mut report = HealthReport::default();
    let mut rows = Vec::new();
    let steps = 5.0;
    let stop = |report: &mut HealthReport| {
        if cancelled() {
            report.cancelled = true;
            true
        } else {
            false
        }
    };

    progress(0.0, "Running the asset validators");
    let rules = match &inputs.rules {
        Some(Ok(rules)) => Some(rules.clone()),
        Some(Err(err)) => {
            rows.push(HealthRow {
                severity: Severity::Error,
                category: HealthCategory::Validation,
                title: "config/validation.ron does not load".to_owned(),
                detail: err.clone(),
                action: "Fix the file; until then the validators run with the defaults".to_owned(),
                link: HealthLink::Asset {
                    path: "config/validation.ron".to_owned(),
                },
                count: 1,
            });
            None
        }
        None => None,
    };
    let file = rules.clone().unwrap_or_default();
    match &inputs.set {
        Some(set) => {
            let settings = RunSettings {
                scope: Scope::Project,
                trigger: Trigger::Manual,
                exclude: file.exclude.clone(),
                ..RunSettings::default()
            };
            let validation = ValidatorRegistry::with_rules(&file).run_with(set, &settings);
            rows.extend(validation_rows(&validation));
            report.sources.push(HealthSource {
                source: "validators",
                ran: true,
                note: format!("{} asset(s) checked", validation.summary.checked),
            });
            report.validation = Some(validation);
        }
        None => report.sources.push(HealthSource {
            source: "validators",
            ran: false,
            note: "no asset registry is loaded: cook the project first".to_owned(),
        }),
    }
    if stop(&mut report) {
        rank(&mut rows);
        report.rows = rows;
        return report;
    }

    progress(1.0 / steps, "Looking for unreferenced large assets");
    match &inputs.set {
        Some(set) => {
            let found = unreferenced_rows(set, &file.roots, UNREFERENCED_LARGE_BYTES);
            report.sources.push(HealthSource {
                source: "unreferenced_large",
                ran: true,
                note: format!(
                    "{} asset(s) of {} or more with no referrer",
                    found.iter().map(|r| r.count).sum::<usize>(),
                    crate::asset_panels::format_bytes(UNREFERENCED_LARGE_BYTES)
                ),
            });
            rows.extend(found);
        }
        None => report.sources.push(HealthSource {
            source: "unreferenced_large",
            ran: false,
            note: "no asset registry is loaded".to_owned(),
        }),
    }

    progress(2.0 / steps, "Reading the cook's import warnings");
    match &inputs.root {
        Some(root) => {
            let cache = regia_asset::cooked_cache_dir(root, &inputs.platform);
            let found = cook_rows(&cache);
            report.sources.push(HealthSource {
                source: "cook_warnings",
                ran: true,
                note: format!("{} source(s) with warnings ({})", found.len(), inputs.platform),
            });
            rows.extend(found);
        }
        None => report.sources.push(HealthSource {
            source: "cook_warnings",
            ran: false,
            note: "no project is open".to_owned(),
        }),
    }
    if stop(&mut report) {
        rank(&mut rows);
        report.rows = rows;
        return report;
    }

    progress(3.0 / steps, "Checking toolchains (regia doctor)");
    match (&inputs.cli, &inputs.root) {
        (Some(cli), Some(root)) => match run_doctor(cli, root) {
            Ok(doc) => {
                let found = doctor_rows(&doc);
                report.sources.push(HealthSource {
                    source: "doctor",
                    ran: true,
                    note: format!("{} toolchain finding(s)", found.len()),
                });
                rows.extend(found);
            }
            Err(err) => {
                rows.push(HealthRow {
                    severity: Severity::Info,
                    category: HealthCategory::Toolchain,
                    title: "The toolchain checks did not run".to_owned(),
                    detail: err.clone(),
                    action: "Build the CLI (cargo build -p regia_cli) or set REGIA_CLI to its path"
                        .to_owned(),
                    link: HealthLink::None,
                    count: 1,
                });
                report.sources.push(HealthSource {
                    source: "doctor",
                    ran: false,
                    note: err,
                });
            }
        },
        _ => report.sources.push(HealthSource {
            source: "doctor",
            ran: false,
            note: "skipped".to_owned(),
        }),
    }
    if stop(&mut report) {
        rank(&mut rows);
        report.rows = rows;
        return report;
    }

    progress(4.0 / steps, "Looking for autosave leftovers");
    match &inputs.root {
        Some(root) => {
            let found = recovery_rows(root);
            report.sources.push(HealthSource {
                source: "recovery",
                ran: true,
                note: format!("{} autosave(s)", found.len()),
            });
            rows.extend(found);
        }
        None => report.sources.push(HealthSource {
            source: "recovery",
            ran: false,
            note: "no project is open".to_owned(),
        }),
    }
    rank(&mut rows);
    report.rows = rows;
    report
}

// ---------------------------------------------------------------------------------------------
// The editor model and job
// ---------------------------------------------------------------------------------------------

/// The panel's model, shared by the panel and the agent methods.
#[derive(Resource, Debug, Clone, Default)]
pub struct ProjectHealth {
    /// The last finished run.
    pub report: Option<HealthReport>,
    /// The job of the run in progress.
    pub running: Option<String>,
    /// What the last run or link did, when it did not work.
    pub status: Option<String>,
    /// The selected row (an index into the report's rows).
    pub selected: Option<usize>,
    /// Set by a Problems link; the shell brings the Problems panel forward and clears it.
    pub focus_problems: bool,
}

/// The running check.
#[derive(Resource, Debug)]
struct ProjectHealthJob {
    job: WorkerJob<HealthReport>,
}

/// What a run reads from the world (the CLI found as the bake runner finds it, unless `doctor` is
/// off).
#[must_use]
pub fn gather_inputs(world: &World, doctor: bool) -> HealthInputs {
    let root = crate::mesh_source::project_root(world);
    HealthInputs {
        rules: root.as_ref().map(|_| crate::asset_panels::project_rules(world)),
        set: crate::asset_panels::registry_set(world),
        cli: doctor.then(crate::bake_runner::cli_program),
        platform: regia_asset_pipeline::host_platform().to_owned(),
        root,
    }
}

/// Starts a run as an editor job and answers its id.
///
/// # Errors
///
/// `project_health_running` while one runs; `job_start_failed` when the worker cannot start.
pub fn start_run(world: &mut World, inputs: HealthInputs) -> Result<String, RpcError> {
    if let Some(job) = world.get_resource::<ProjectHealthJob>() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "project_health_running",
            format!("a project health run is in progress ({})", job.job.id),
            "wait for it (editor.project_health { action: \"state\" }) or cancel it with editor.cancel_job",
        ));
    }
    let job = WorkerJob::spawn(
        world,
        "project_health",
        "project_health",
        "Project health".to_owned(),
        "regia-project-health",
        move |cx: WorkerContext| {
            let token = cx.clone();
            collect(&inputs, &move || token.cancelled(), &|fraction, message| {
                cx.progress(Some(fraction), message);
            })
        },
    )
    .map_err(|message| RpcError::new(INVALID_PARAMS, "job_start_failed", message, "try again"))?;
    let id = job.id.clone();
    let mut health = world.get_resource_or_insert_with(ProjectHealth::default);
    health.running = Some(id.clone());
    health.status = None;
    world.insert_resource(ProjectHealthJob { job });
    Ok(id)
}

/// Applies a finished run to the world: the panel's report and the Problems panel.
fn apply(world: &mut World, job: &ProjectHealthJob, outcome: Result<HealthReport, String>) {
    let mut health = world.get_resource_or_insert_with(ProjectHealth::default);
    health.running = None;
    match outcome {
        Ok(mut report) => {
            let summary = json!({
                "rows": report.rows.len(),
                "errors": report.count(Severity::Error),
                "warnings": report.count(Severity::Warning),
                "cancelled": report.cancelled,
            });
            let cancelled = report.cancelled;
            let validation = report.validation.take();
            health.selected = None;
            health.report = Some(report);
            if let Some(validation) = validation {
                let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
                panels.problems.set_report(validation);
                panels.problems.last_scope = Some(Scope::Project);
                panels.source = PanelSource::Registry;
            }
            job.job.end(world, Ok((summary, cancelled)));
        }
        Err(message) => {
            health.status = Some(message.clone());
            job.job.end(world, Err(("project_health_failed", message)));
        }
    }
}

/// Polls the running check (every frame, [`crate::editor_jobs::tick`]); never waits.
pub fn tick_job(world: &mut World) {
    let Some(mut job) = world.remove_resource::<ProjectHealthJob>() else {
        return;
    };
    match job.job.take() {
        Some(outcome) => apply(world, &job, outcome),
        None => {
            let message = job.job.context().message();
            job.job.sync_row(world, &message, job.job.context().fraction());
            world.insert_resource(job);
        }
    }
}

/// Waits for the running check and applies it (the agent's `wait`, tests). `false` when none ran.
pub fn finish_now(world: &mut World) -> bool {
    let Some(mut job) = world.remove_resource::<ProjectHealthJob>() else {
        return false;
    };
    match job.job.join() {
        Some(outcome) => apply(world, &job, outcome),
        None => apply(
            world,
            &job,
            Err("the worker's outcome was already taken".to_owned()),
        ),
    }
    true
}

/// Opens a row's link: an asset as the Content Browser does, a validator's findings in Problems,
/// a command onto the clipboard request (`copy` is set). Answers the status line.
pub fn open_link(world: &mut World, link: &HealthLink) -> (String, Option<String>) {
    match link {
        HealthLink::None => ("Nothing to open".to_owned(), None),
        HealthLink::Asset { path } => {
            world
                .get_resource_or_insert_with(crate::log_links::AssetOpenRequests::default)
                .paths
                .push(path.clone());
            (format!("Opening {path}"), None)
        }
        HealthLink::Problems { validator } => {
            world
                .get_resource_or_insert_with(AssetPanels::default)
                .problems
                .validator = Some(validator.clone());
            world
                .get_resource_or_insert_with(ProjectHealth::default)
                .focus_problems = true;
            (format!("Problems shows {validator}"), None)
        }
        HealthLink::Command { command } => (format!("Copied: {command}"), Some(command.clone())),
    }
}

/// Whether a Problems link asked for the Problems panel (cleared by the call).
pub fn take_focus_problems(world: &mut World) -> bool {
    world
        .get_resource_mut::<ProjectHealth>()
        .is_some_and(|mut health| std::mem::take(&mut health.focus_problems))
}

// ---------------------------------------------------------------------------------------------
// Project Settings ▸ Validation: the pre-commit hook
// ---------------------------------------------------------------------------------------------

/// The Validation page's view of the pre-commit hook (read when the page opens and after each
/// action, never every frame: reading it runs git).
#[derive(Resource, Debug, Clone, Default)]
pub struct HookPage {
    /// The hook's state, or why it could not be read.
    pub state: Option<Result<regia_asset_pipeline::precommit::HookState, String>>,
    /// Replace a hook Regia did not write (moved aside, put back on removal).
    pub force: bool,
    /// What the last action did.
    pub status: Option<String>,
    /// The egui frame whose *Replace the existing hook* click was applied.
    force_frame: Option<u64>,
}

impl HookPage {
    /// The *Replace the existing hook* checkbox asks for `on` in egui frame `frame`: applied once
    /// per frame, so egui's second UI pass (which sees the same click) does not flip it back
    /// (review sprint-w12-rev-g5 #6). Returns whether it applied.
    pub fn set_force(&mut self, on: bool, frame: u64) -> bool {
        if self.force_frame == Some(frame) {
            return false;
        }
        self.force_frame = Some(frame);
        self.force = on;
        true
    }
}

fn hook_error(err: &regia_asset_pipeline::precommit::PrecommitError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        err.code(),
        err.to_string(),
        "the project must be inside a git work tree; force replaces a hook Regia did not write",
    )
}

/// Installs (`install`) or removes Regia's pre-commit hook of the open project; the same work as
/// `regia hooks install|uninstall`, running the CLI the editor finds.
///
/// # Errors
///
/// `no_project`, and the codes of `regia_asset_pipeline::precommit::PrecommitError` (`hook_foreign`,
/// `hook_missing`, `not_a_git_repo`, `git_not_found`, `git_failed`, `hook_io`).
pub fn set_hook(world: &mut World, install: bool, force: bool) -> Result<Value, RpcError> {
    let root = crate::mesh_source::project_root(world).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "no_project",
            "no project is open",
            "open a project first",
        )
    })?;
    let change = if install {
        regia_asset_pipeline::precommit::install_hook(&root, &crate::bake_runner::cli_program(), force)
    } else {
        regia_asset_pipeline::precommit::uninstall_hook(&root, force)
    }
    .map_err(|e| hook_error(&e))?;
    let state = regia_asset_pipeline::precommit::hook_state(&root).map_err(|e| hook_error(&e))?;
    let mut page = world.get_resource_or_insert_with(HookPage::default);
    page.status = Some(format!("Pre-commit hook {}", change.action));
    page.state = Some(Ok(state.clone()));
    Ok(json!({ "change": change, "state": state }))
}

/// Draws Project Settings ▸ Validation: the pre-commit hook's state, *Install pre-commit hook*
/// and *Remove pre-commit hook*.
pub fn validation_settings_ui(ui: &mut egui::Ui, world: &mut World, root: &Path) {
    egui::CollapsingHeader::new("Validation")
        .id_salt("project-settings-validation")
        .show(ui, |ui| {
            let mut page = world.remove_resource::<HookPage>().unwrap_or_default();
            if page.state.is_none() {
                page.state = Some(regia_asset_pipeline::precommit::hook_state(root).map_err(|e| e.to_string()));
            }
            ui.label(
                "A pre-commit hook runs `regia assets validate --staged` before each commit: a staged asset that \
                 does not parse, or fails a validator, stops the commit.",
            );
            let (installed, foreign) = match &page.state {
                Some(Ok(state)) => {
                    let what = match (state.exists, state.regia) {
                        (false, _) => "Not installed".to_owned(),
                        (true, true) => format!("Installed: {}", state.path.display()),
                        (true, false) => format!("Another pre-commit hook is installed: {}", state.path.display()),
                    };
                    ui.weak(what);
                    (state.exists && state.regia, state.exists && !state.regia)
                }
                Some(Err(err)) => {
                    ui.weak(err.as_str());
                    (false, false)
                }
                None => (false, false),
            };
            let mut action = None;
            ui.horizontal_wrapped(|ui| {
                if foreign {
                    let mut force = page.force;
                    let replace =
                        ui.checkbox(&mut force, "Replace the existing hook (it is kept and put back on removal)");
                    if replace.changed() {
                        page.set_force(force, ui.ctx().cumulative_frame_nr());
                    }
                }
                let can_install = page.state.as_ref().is_some_and(Result::is_ok) && (!foreign || page.force);
                if ui
                    .add_enabled(can_install, egui::Button::new("Install pre-commit hook"))
                    .clicked()
                {
                    action = Some(true);
                }
                if ui
                    .add_enabled(installed, egui::Button::new("Remove pre-commit hook"))
                    .clicked()
                {
                    action = Some(false);
                }
                if ui.button("Refresh").clicked() {
                    page.state = None;
                }
            });
            if let Some(status) = &page.status {
                ui.weak(status.as_str());
            }
            let force = page.force;
            world.insert_resource(page);
            if let Some(install) = action
                && let Err(err) = set_hook(world, install, force)
            {
                world.resource_mut::<HookPage>().status = Some(err.message);
            }
        });
}

/// `editor.precommit_hook`.
fn precommit_hook_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
    match params.get("action").and_then(Value::as_str).unwrap_or("state") {
        "install" => set_hook(world, true, force),
        "uninstall" => set_hook(world, false, force),
        "state" => {
            let root = crate::mesh_source::project_root(world).ok_or_else(|| {
                RpcError::new(
                    INVALID_PARAMS,
                    "no_project",
                    "no project is open",
                    "open a project first",
                )
            })?;
            let state = regia_asset_pipeline::precommit::hook_state(&root).map_err(|e| hook_error(&e))?;
            Ok(json!({ "state": state }))
        }
        other => Err(RpcError::new(
            INVALID_PARAMS,
            "precommit_hook_action",
            format!("unknown action {other:?}"),
            "action is state, install or uninstall",
        )),
    }
}

/// Tools ▸ Project Health: brings the panel's tab forward. Answers the toast.
pub fn menu_item(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    if !crate::agent_parity::action_menu_item(ui, &crate::agent_parity::TOOLS_PROJECT_HEALTH).clicked() {
        return None;
    }
    crate::command_palette::run(world, "window.open_project_health")
}

// ---------------------------------------------------------------------------------------------
// egui
// ---------------------------------------------------------------------------------------------

fn link_label(link: &HealthLink) -> Option<&'static str> {
    match link {
        HealthLink::None => None,
        HealthLink::Asset { .. } => Some("Open"),
        HealthLink::Problems { .. } => Some("Show in Problems"),
        HealthLink::Command { .. } => Some("Copy command"),
    }
}

/// Draws the Project Health panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "project_health_panel")]
pub fn project_health_panel(ui: &mut egui::Ui, world: &mut World) {
    tick_job(world);
    let mut rerun = false;
    let mut activate: Option<usize> = None;
    let mut select: Option<usize> = None;
    let mut health = world.remove_resource::<ProjectHealth>().unwrap_or_default();
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let weak = ui.visuals().weak_text_color();
    let color_of = |severity: Severity| match severity {
        Severity::Error => accents.log_error,
        Severity::Warning => accents.log_warn,
        Severity::Info => weak,
    };
    ui.horizontal_wrapped(|ui| {
        ui.strong("Project Health");
        rerun = ui
            .add_enabled(health.running.is_none(), egui::Button::new("Re-run"))
            .on_hover_text("Run every check again (an editor job you can cancel)")
            .clicked();
        if let Some(job) = &health.running {
            ui.spinner();
            ui.weak(format!("running ({job})"));
        }
        if let Some(report) = &health.report {
            ui.label(format!(
                "{} error(s), {} warning(s), {} note(s)",
                report.count(Severity::Error),
                report.count(Severity::Warning),
                report.count(Severity::Info)
            ));
        }
        if let Some(status) = &health.status {
            ui.weak(status);
        }
    });
    ui.separator();
    match &health.report {
        None if health.running.is_none() => {
            ui.label("No checks ran yet: use Re-run.");
        }
        None => {}
        Some(report) => {
            if report.rows.is_empty() {
                ui.label("No problems found.");
            }
            let row_height = ui.spacing().interact_size.y * 2.0;
            egui::ScrollArea::vertical()
                .id_salt("health-rows")
                .auto_shrink([false, false])
                .show_rows(ui, row_height, report.rows.len(), |ui, range| {
                    for index in range {
                        let Some(row) = report.rows.get(index) else {
                            continue;
                        };
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(severity_name(row.severity))
                                    .color(color_of(row.severity))
                                    .strong(),
                            );
                            ui.weak(row.category.label());
                            let response = ui
                                .selectable_label(health.selected == Some(index), &row.title)
                                .on_hover_text(&row.detail);
                            if response.clicked() {
                                select = Some(index);
                            }
                            if let Some(label) = link_label(&row.link)
                                && ui.small_button(label).clicked()
                            {
                                activate = Some(index);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(ui.spacing().indent);
                            ui.weak(format!("Recommended: {}", row.action));
                        });
                    }
                });
            ui.separator();
            ui.collapsing("Checks", |ui| {
                for source in &report.sources {
                    let mark = if source.ran { "ran" } else { "skipped" };
                    ui.weak(format!("{}: {mark}, {}", source.source, source.note));
                }
            });
        }
    }
    if let Some(index) = select {
        health.selected = Some(index);
    }
    let link = activate.and_then(|index| {
        health
            .report
            .as_ref()
            .and_then(|r| r.rows.get(index))
            .map(|r| r.link.clone())
    });
    world.insert_resource(health);
    {
        if let Some(link) = link {
            let (status, copy) = open_link(world, &link);
            if let Some(text) = copy {
                ui.ctx().copy_text(text);
            }
            world.resource_mut::<ProjectHealth>().status = Some(status);
        }
    }
    if rerun {
        let inputs = gather_inputs(world, true);
        if let Err(err) = start_run(world, inputs) {
            world.resource_mut::<ProjectHealth>().status = Some(err.message);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn state_json(world: &World) -> Value {
    let health = world.get_resource::<ProjectHealth>();
    let report = health.and_then(|h| h.report.as_ref());
    json!({
        "running": health.and_then(|h| h.running.clone()),
        "status": health.and_then(|h| h.status.clone()),
        "ran": report.is_some(),
        "errors": report.map_or(0, |r| r.count(Severity::Error)),
        "warnings": report.map_or(0, |r| r.count(Severity::Warning)),
        "notes": report.map_or(0, |r| r.count(Severity::Info)),
        "cancelled": report.is_some_and(|r| r.cancelled),
        "rows": report.map(|r| r.rows.iter().enumerate().map(|(rank, row)| {
            let mut value = json!({
                "rank": rank,
                "severity": severity_name(row.severity),
                "category": row.category.name(),
                "title": row.title,
                "detail": row.detail,
                "action": row.action,
                "link": row.link,
                "count": row.count,
            });
            if let Some(object) = value.as_object_mut() {
                object.retain(|_, v| !v.is_null());
            }
            value
        }).collect::<Vec<_>>()).unwrap_or_default(),
        "sources": report.map(|r| r.sources.clone()).unwrap_or_default(),
    })
}

/// `editor.project_health`.
fn project_health_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("state");
    match action {
        "state" => {
            tick_job(world);
            Ok(state_json(world))
        }
        "run" => {
            let doctor = params.get("doctor").and_then(Value::as_bool).unwrap_or(true);
            let wait = params.get("wait").and_then(Value::as_bool).unwrap_or(false);
            let inputs = gather_inputs(world, doctor);
            let id = start_run(world, inputs)?;
            if wait {
                finish_now(world);
            }
            let mut state = state_json(world);
            state["job"] = json!(id);
            Ok(state)
        }
        "open" => {
            let index = params
                .get("row")
                .and_then(Value::as_u64)
                .ok_or_else(|| RpcError::invalid_params("`row` (a rank from state) is required"))?;
            let link = world
                .get_resource::<ProjectHealth>()
                .and_then(|h| h.report.as_ref())
                .and_then(|r| r.rows.get(index as usize))
                .map(|r| r.link.clone())
                .ok_or_else(|| {
                    RpcError::new(
                        INVALID_PARAMS,
                        "project_health_no_row",
                        format!("there is no row {index}"),
                        "run first, then take a rank from editor.project_health { action: \"state\" }",
                    )
                })?;
            let (status, copy) = open_link(world, &link);
            world.get_resource_or_insert_with(ProjectHealth::default).selected = Some(index as usize);
            Ok(json!({ "status": status, "link": link, "copy": copy }))
        }
        other => Err(RpcError::new(
            INVALID_PARAMS,
            "project_health_action",
            format!("unknown action {other:?}"),
            "action is run, state or open",
        )),
    }
}

/// The Project Health and pre-commit hook agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.precommit_hook",
                tool: "precommit_hook",
                description: "Project Settings ▸ Validation: the git pre-commit hook that runs `regia assets validate --staged`. `state` reads it, `install` writes it (refuses a hook Regia did not write unless force, which keeps that hook beside it), `uninstall` removes it and puts a replaced hook back.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: false,
                        description: "state (default), install or uninstall.",
                    },
                    ParamSchema {
                        name: "force",
                        kind: "boolean",
                        required: false,
                        description: "Replace (install) or remove (uninstall) a hook Regia did not write.",
                    },
                ],
                result: "{ state: { path, exists, regia, backup }, change?: { path, action, backup } }",
                mutating: true,
                permission: Permission::Play,
            },
            precommit_hook_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.project_health",
                tool: "project_health",
                description: "The Project Health panel: `run` starts every check (validators, unreferenced large assets, budgets, missing references, cook warnings, regia doctor, autosave leftovers) as an editor job, `state` answers the ranked rows with a recommended action and a link each, `open` follows a row's link as the panel's button does.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: false,
                        description: "run, state (default) or open.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "With run: wait for the job and answer its rows.",
                    },
                    ParamSchema {
                        name: "doctor",
                        kind: "boolean",
                        required: false,
                        description: "With run: include the regia doctor checks (default true).",
                    },
                    ParamSchema {
                        name: "row",
                        kind: "number",
                        required: false,
                        description: "With open: the row's rank.",
                    },
                ],
                result: "{ running, status, ran, errors, warnings, notes, cancelled, rows: [{ rank, severity, category, title, detail, action, link, count }], sources, job?, copy? }",
                mutating: true,
                permission: Permission::Play,
            },
            project_health_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset_pipeline::import_report::{ImportReportEntry, ImportReports};

    /// Review sprint-w12-rev-g5 #6: the second UI pass of a frame reads `force: true` and its
    /// checkbox asks for false; the click applies once, so Replace sticks.
    #[test]
    fn replace_the_existing_hook_applies_once_per_frame() {
        let mut page = HookPage::default();
        assert!(page.set_force(true, 9));
        assert!(!page.set_force(false, 9));
        assert!(page.force, "the checkbox stays on");
        assert!(page.set_force(false, 10));
        assert!(!page.force);
    }

    const SET: &str = r##"#![enable(implicit_some)]
    (
        format_version: 2,
        assets: [
            (path: "assets/scenes/main.scene", kind: scene, references: ["assets/meshes/rock.mesh", "assets/textures/gone.png"], cooked_bytes: 10),
            (path: "assets/meshes/rock.mesh", kind: mesh, references: ["assets/textures/huge.png"], cooked_bytes: 100, triangles: 500000),
            (path: "assets/textures/huge.png", kind: texture, cooked_bytes: 1000, texture_size: (8192, 8192)),
            (path: "assets/textures/loose_big.png", kind: texture, cooked_bytes: 3000000, texture_size: (1024, 1024)),
            (path: "assets/textures/loose_small.png", kind: texture, cooked_bytes: 5, texture_size: (16, 16)),
        ],
    )"##;

    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        // A cook warning.
        let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
        std::fs::create_dir_all(&cache).expect("cache");
        let mut reports = ImportReports::new();
        reports
            .upsert_and_save(
                &cache,
                "assets/sprites/hero.aseprite",
                ImportReportEntry {
                    importer: "aseprite".to_owned(),
                    data: json!({}),
                    warnings: vec!["blend mode overlay dropped".to_owned()],
                },
            )
            .expect("reports");
        // An autosave left by a crash (no level file matches it).
        let autosave = crate::autosave::autosave_dir(&root);
        std::fs::create_dir_all(&autosave).expect("autosave dir");
        std::fs::write(autosave.join("lost.level.ron"), "()").expect("autosave");
        root
    }

    fn inputs(root: &Path) -> HealthInputs {
        HealthInputs {
            root: Some(root.to_path_buf()),
            set: Some(AssetSet::from_ron(SET).expect("set")),
            rules: Some(Ok(ValidationFile::default())),
            cli: None,
            platform: regia_asset_pipeline::host_platform().to_owned(),
        }
    }

    /// P-155: one ranked list from the existing checks: errors first; missing references, budgets,
    /// unreferenced large assets, cook warnings and autosave leftovers each have a row with an
    /// action and a link; the doctor source says it was skipped.
    #[test]
    fn a_run_ranks_rows_from_every_source() {
        let root = project("health-ranks");
        let report = collect(&inputs(&root), &|| false, &|_, _| {});
        assert!(!report.cancelled);
        let ranks: Vec<usize> = report.rows.iter().map(|r| severity_rank(r.severity)).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(
            ranks, sorted,
            "errors, then warnings, then notes: {:#?}",
            report.rows
        );
        let category = |c: HealthCategory| report.rows.iter().find(|r| r.category == c);
        let missing = category(HealthCategory::MissingReference).expect("missing reference row");
        assert_eq!(missing.severity, Severity::Error);
        assert_eq!(
            missing.link,
            HealthLink::Problems {
                validator: "missing_references".to_owned()
            }
        );
        assert_eq!(
            report.rows[0].category,
            HealthCategory::MissingReference,
            "{:#?}",
            report.rows
        );
        let budgets: Vec<&HealthRow> = report
            .rows
            .iter()
            .filter(|r| r.category == HealthCategory::Budget)
            .collect();
        assert!(
            budgets.iter().any(|r| r.title.starts_with("texture_size_limits"))
                && budgets
                    .iter()
                    .any(|r| r.title.starts_with("mesh_triangle_budget")),
            "{budgets:#?}"
        );
        let unreferenced = category(HealthCategory::UnreferencedLarge).expect("unreferenced row");
        assert_eq!(
            unreferenced.link,
            HealthLink::Asset {
                path: "assets/textures/loose_big.png".to_owned()
            }
        );
        assert!(
            !report
                .rows
                .iter()
                .any(|r| r.title.contains("loose_small") && r.category == HealthCategory::UnreferencedLarge),
            "small assets are not listed"
        );
        let cook = category(HealthCategory::CookWarning).expect("cook row");
        assert!(cook.detail.contains("blend mode overlay dropped"), "{cook:?}");
        let recovery = category(HealthCategory::Recovery).expect("recovery row");
        assert_eq!(recovery.severity, Severity::Warning);
        assert!(
            report.rows.iter().all(|r| !r.action.is_empty()),
            "every row recommends an action"
        );
        let doctor = report
            .sources
            .iter()
            .find(|s| s.source == "doctor")
            .expect("doctor source");
        assert!(!doctor.ran);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cancel between sources ends the run with what ran.
    #[test]
    fn a_cancelled_run_keeps_what_ran() {
        let root = project("health-cancel");
        let report = collect(&inputs(&root), &|| true, &|_, _| {});
        assert!(report.cancelled);
        assert_eq!(
            report.sources.len(),
            1,
            "only the validators ran: {:?}",
            report.sources
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn doctor_findings_become_rows_with_their_fix() {
        let doc = json!({
            "schema": 1,
            "checks": [
                { "section": "rust", "name": "rustc", "status": "ok", "code": "ok", "detail": "1.99", "fix": null },
                { "section": "android", "name": "ndk", "status": "fail", "code": "ndk_missing", "detail": "no NDK", "fix": "install NDK r27" },
                { "section": "windows", "name": "cargo-xwin", "status": "warn", "code": "cargo_xwin_missing", "detail": "", "fix": "cargo install cargo-xwin" },
            ],
        });
        let mut rows = doctor_rows(&doc);
        rank(&mut rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].severity, Severity::Error);
        assert_eq!(rows[0].title, "android: ndk (ndk_missing)");
        assert_eq!(
            rows[0].link,
            HealthLink::Command {
                command: "install NDK r27".to_owned()
            }
        );
        assert_eq!(rows[1].action, "cargo install cargo-xwin");
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .unwrap_or_else(|| panic!("{method} is installed"));
        handler(world, &params)
    }

    /// The dispatcher route: `run` as an editor job (with `wait`), `state` with the ranked rows,
    /// `open` following a row's link; the Problems panel holds the same validator findings.
    #[test]
    fn project_health_dispatches_run_state_and_open() {
        let root = project("health-agent");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let id = start_run(&mut world, inputs(&root)).expect("starts");
        assert!(id.starts_with("project_health:"), "{id}");
        let err = start_run(&mut world, inputs(&root)).expect_err("one at a time");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("project_health_running")
        );
        assert!(finish_now(&mut world));
        let state = call(&mut world, "editor.project_health", json!({ "action": "state" })).expect("state");
        assert_eq!(state["ran"], true);
        assert!(state["errors"].as_u64() >= Some(1), "{state}");
        assert_eq!(state["rows"][0]["category"], "missing_reference", "{state}");
        assert!(
            world.resource::<AssetPanels>().problems.report.is_some(),
            "Problems filled too"
        );
        let open = call(
            &mut world,
            "editor.project_health",
            json!({ "action": "open", "row": 0 }),
        )
        .expect("open");
        assert_eq!(open["link"]["kind"], "problems", "{open}");
        assert_eq!(
            world.resource::<AssetPanels>().problems.validator.as_deref(),
            Some("missing_references")
        );
        assert!(take_focus_problems(&mut world));
        assert!(!take_focus_problems(&mut world), "taken once");
        let err = call(
            &mut world,
            "editor.project_health",
            json!({ "action": "open", "row": 9999 }),
        )
        .expect_err("no row");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("project_health_no_row")
        );
        let err =
            call(&mut world, "editor.project_health", json!({ "action": "fly" })).expect_err("bad action");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("project_health_action")
        );
        // `run` through the dispatcher, toolchain checks off: no registry is loaded here.
        let out = call(
            &mut world,
            "editor.project_health",
            json!({ "action": "run", "wait": true, "doctor": false }),
        )
        .expect("run");
        assert!(
            out["job"]
                .as_str()
                .is_some_and(|j| j.starts_with("project_health:")),
            "{out}"
        );
        let sources = out["sources"].as_array().expect("sources");
        assert!(
            sources
                .iter()
                .any(|s| s["source"] == "validators" && s["ran"] == false),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn precommit_hook_needs_a_project() {
        let mut world = World::new();
        let err = call(&mut world, "editor.precommit_hook", json!({})).expect_err("no project");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("no_project")
        );
        let root = regia_test_utils::unique_temp_dir("health-hook-norepo");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        if regia_asset_pipeline::precommit::repo_root(&root).is_err() {
            let err = call(
                &mut world,
                "editor.precommit_hook",
                json!({ "action": "install" }),
            )
            .expect_err("no repo");
            assert_eq!(
                err.data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str),
                Some("not_a_git_repo")
            );
        }
        let err =
            call(&mut world, "editor.precommit_hook", json!({ "action": "zap" })).expect_err("bad action");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("precommit_hook_action")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Accessibility bar: Re-run and a row's link button are reached from the keyboard, and Enter
    /// on the link opens it.
    #[test]
    fn the_panel_works_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let root = project("health-keys");
        let mut world = World::new();
        let mut report = collect(&inputs(&root), &|| false, &|_, _| {});
        report
            .rows
            .retain(|r| r.category == HealthCategory::UnreferencedLarge);
        world.insert_resource(ProjectHealth {
            report: Some(report),
            ..ProjectHealth::default()
        });
        let ctx = egui::Context::default();
        let size = [1200.0, 600.0];
        let draw = |ui: &mut egui::Ui, world: &mut World| {
            egui::CentralPanel::default().show(ui, |ui| project_health_panel(ui, world));
        };
        frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        tab_to(&ctx, size, "Re-run", false, 40, |ui| draw(ui, &mut world));
        tab_to(&ctx, size, "Open", false, 40, |ui| draw(ui, &mut world));
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        let requests = world
            .get_resource::<crate::log_links::AssetOpenRequests>()
            .map(|r| r.paths.clone())
            .unwrap_or_default();
        assert_eq!(requests, ["assets/textures/loose_big.png"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Project Settings ▸ Validation from the keyboard: open the page, Tab to Install pre-commit
    /// hook, Enter; then `editor.precommit_hook` reads and removes it.
    #[test]
    fn the_validation_page_installs_the_hook_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let root = regia_test_utils::unique_temp_dir("health-hook-keys");
        let init = std::process::Command::new("git")
            .current_dir(&root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .args(["init", "-q"])
            .output()
            .expect("git runs (git is required)");
        assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let ctx = egui::Context::default();
        let size = [1200.0, 600.0];
        let draw = |ui: &mut egui::Ui, world: &mut World| {
            egui::CentralPanel::default().show(ui, |ui| validation_settings_ui(ui, world, &root));
        };
        frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        tab_to(&ctx, size, "Validation", false, 10, |ui| draw(ui, &mut world));
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        tab_to(&ctx, size, "Install pre-commit hook", false, 20, |ui| {
            draw(ui, &mut world)
        });
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        let state = regia_asset_pipeline::precommit::hook_state(&root).expect("state");
        assert!(state.exists && state.regia, "{state:?}");
        let out = call(&mut world, "editor.precommit_hook", json!({ "action": "state" })).expect("state");
        assert_eq!(out["state"]["regia"], true);
        let out = call(
            &mut world,
            "editor.precommit_hook",
            json!({ "action": "uninstall" }),
        )
        .expect("removed");
        assert_eq!(out["change"]["action"], "removed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
