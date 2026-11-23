// SPDX-License-Identifier: MPL-2.0
//! Source-lint adapter for the validation CLI and the cook/package gate (ADR-8390).
//!
//! [`crate::validators::Validator`] stays disk-free. This module runs the existing geometry and
//! material linters, caches a decode by source dependencies and importer version, and caches the
//! evidence report by that decode plus the resolved settings. Cook and package never pass
//! `fix`. A cancelled scan returns an error and does not publish a partial report.
//!
//! The evidence file under `build/source-lint/` is a build cache. It is not a frozen content
//! format: [`crate::source_lint::SourceLintReport`] is unchanged.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::gltf_importer_version;
use crate::gltf_lint::{CapturedGeometry, evaluate_captured, lint_gltf_captured};
use crate::gltf_material_lint::{
    MaterialLintProfile, MaterialLintReport, MaterialLintSettings, lint_material_file,
};
use crate::source_lint::{
    FindingSeverity, LintFinding, MetricReading, SOURCE_LINT_REPORT_VERSION, SOURCE_LINT_TOOL,
    SourceEvidence, SourceLintError, SourceLintPolicy, SourceLintReport, SupportStatus, WorkCounters,
    content_hash, findings_from_gltf_report, findings_from_material_report, rules,
};
use crate::validation_rules::{CookGate, ValidationFile};

static WRITE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Counters for one adapter run. A warm evidence hit leaves the parse counters at zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScanCounters {
    /// Calls that opened a glTF through the checked loader.
    pub source_parses: u64,
    /// Decoded position payload plus the loader's decoded-byte reservation, for parses only.
    pub decoded_bytes: u64,
    /// Sources or reports served from the build cache.
    pub cache_hits: u64,
}

/// What one scan asked for.
#[derive(Clone, Debug)]
pub struct ScanRequest {
    /// Project root. The cache is `build/source-lint/` under this directory.
    pub project: PathBuf,
    /// Policy target. `default` is always valid.
    pub target: String,
    /// Files or directories to scan. Empty, with no [`Self::changed`] list, scans `assets/`.
    pub paths: Vec<PathBuf>,
    /// Project-relative paths. `Some` limits the scan to those glTF sources.
    pub changed: Option<Vec<String>>,
    /// Recorded for the caller. The scan itself does not change publication policy.
    pub strict: bool,
    /// Material repair. Cook and package pass `false`.
    pub fix: bool,
    /// When this path exists, the scan stops with [`ScanError::Cancelled`].
    pub cancel_file: Option<PathBuf>,
    /// Evidence JSON destination. Default: `build/source-lint/report.json`.
    pub report_path: Option<PathBuf>,
    /// Replaces the resolved material profile when `Some` (the material CLI).
    pub material_profile: Option<MaterialLintProfile>,
    /// Skip files and directories whose names start with `_`.
    pub skip_staging: bool,
}

/// One finished scan.
#[derive(Clone, Debug)]
pub struct ScanOutcome {
    /// Evidence report. The schema is the existing source-lint report.
    pub report: SourceLintReport,
    /// This run's counters. They are not fields of [`SourceLintReport`].
    pub counters: ScanCounters,
    /// Where the JSON report was written.
    pub report_path: PathBuf,
    /// Sorted rule ids, including suppressed rows.
    pub rule_ids: Vec<String>,
}

/// Why a scan did not finish.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The cancel file was present.
    #[error("{0}")]
    Cancelled(String),
    /// `config/source_lint.ron` or a resolved setting is unusable.
    #[error("{0}")]
    Policy(String),
    /// The target name is empty or not in the policy.
    #[error("{0}")]
    Target(String),
    /// A cache or source file could not be read or written.
    #[error("{0}")]
    Io(String),
    /// A source could not be linted. No partial report is returned.
    #[error("{0}")]
    Lint(String),
    /// The evidence JSON could not be encoded.
    #[error("{0}")]
    Report(String),
}

impl ScanError {
    /// Stable lowercase reason for the CLI and the agent job.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled(_) => "content_lint_cancelled",
            Self::Policy(_) => "content_lint_policy",
            Self::Target(_) => "content_lint_target",
            Self::Io(_) => "content_lint_io",
            Self::Lint(_) => "content_lint_failed",
            Self::Report(_) => "content_lint_report",
        }
    }

    fn policy(message: impl Into<String>) -> Self {
        Self::Policy(message.into())
    }

    fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }

    fn lint(message: impl Into<String>) -> Self {
        Self::Lint(message.into())
    }

    fn from_source(error: SourceLintError) -> Self {
        let message = error.to_string();
        match error.code() {
            "content_lint_target" => Self::Target(message),
            "content_lint_io" => Self::Io(message),
            "content_lint_report" => Self::Report(message),
            _ => Self::Policy(message),
        }
    }
}

/// Lint status stored for the cook and package job result.
#[derive(Clone, Debug, Serialize)]
pub struct LintStatus {
    /// `skipped`, `clean`, `warnings`, or `failed`.
    pub status: String,
    /// Absolute or project report path. Absent when the gate did not run.
    pub report: Option<String>,
    /// Adapter evidence key. Absent when the gate did not run.
    pub evidence_key: Option<String>,
    /// True when this status must stop publication.
    #[serde(skip)]
    pub blocking: bool,
}

/// Project, target and whether the caller is `regia package`.
#[derive(Clone, Debug)]
pub struct PublishRequest {
    /// Project root.
    pub project: PathBuf,
    /// Source-lint target. Cook and package use `default`.
    pub target: String,
    /// Package steps use `Fail` unless `config/validation.ron` names `cook`.
    pub package: bool,
}

/// Opens a cooked registry for the cook or package gate.
///
/// A missing file is `Ok(None)` when `required` is false. A missing required file is an error.
/// A file that exists but does not decode is always an error. The error text does not say the
/// check was skipped.
///
/// # Errors
/// The registry is required and missing, or the file exists but cannot be read or decoded.
pub fn open_cook_registry(
    path: &Path,
    required: bool,
) -> Result<Option<regia_asset::registry::AssetRegistry>, String> {
    crate::registry_bridge::open_cook_registry(path, required)
}

/// The material CLI's file check. Cook never calls this with `fix`.
///
/// # Errors
/// The material linter's own file, policy or parse errors.
pub fn adapt_material(
    path: &Path,
    settings: &MaterialLintSettings,
    fix: bool,
) -> Result<MaterialLintReport, crate::gltf_material_lint::MaterialLintError> {
    lint_material_file(path, settings, fix)
}

/// Scans the selected glTF and texture sources and writes one evidence report.
///
/// # Errors
/// Invalid policy, an unknown target, a cancel file, or a source that cannot be linted.
pub fn scan(request: &ScanRequest) -> Result<ScanOutcome, ScanError> {
    if cancelled(request) {
        return Err(ScanError::Cancelled(
            "source lint scan cancelled before publication".to_owned(),
        ));
    }
    let policy = SourceLintPolicy::load_project(&request.project).map_err(ScanError::from_source)?;
    let _ = policy
        .resolve(&request.target, None)
        .map_err(ScanError::from_source)?;
    let files = collect_files(request)?;
    if cancelled(request) {
        return Err(ScanError::Cancelled(
            "source lint scan cancelled before publication".to_owned(),
        ));
    }

    let importer = gltf_importer_version();
    let mut prepared = Vec::with_capacity(files.len());
    for path in &files {
        if cancelled(request) {
            return Err(ScanError::Cancelled(
                "source lint scan cancelled before publication".to_owned(),
            ));
        }
        prepared.push(prepare_source(&request.project, path)?);
    }
    let deps_hash = dependency_hash(importer, &prepared);
    let mut resolved = Vec::with_capacity(prepared.len());
    for source in &prepared {
        let mut settings = policy
            .resolve(&request.target, Some(&source.rel))
            .map_err(ScanError::from_source)?;
        if let Some(profile) = request.material_profile {
            settings.materials.profile = profile;
        }
        resolved.push(settings);
    }
    let settings_hash = settings_fingerprint(&request.project, &request.target, &prepared, &resolved)?;
    let evidence_key = content_hash(
        format!(
            "{}\n{deps_hash}\n{settings_hash}\n{importer}\n{SOURCE_LINT_TOOL}\n{SOURCE_LINT_REPORT_VERSION}",
            request.target
        )
        .as_bytes(),
    );
    let report_path = request
        .report_path
        .clone()
        .unwrap_or_else(|| cache_root(&request.project).join("report.json"));

    if let Some(report) = read_evidence(&request.project, &evidence_key)? {
        write_atomic(&report_path, &report.to_json().map_err(ScanError::from_source)?)?;
        return Ok(outcome(
            report,
            ScanCounters {
                source_parses: 0,
                decoded_bytes: 0,
                cache_hits: u64::try_from(prepared.len().max(1)).unwrap_or(u64::MAX),
            },
            report_path,
        ));
    }

    let mut counters = ScanCounters::default();
    let mut findings = Vec::new();
    let mut rows = Vec::new();
    let mut bytes_hashed = 0_u64;
    let mut complete = true;
    for (index, source) in prepared.iter().enumerate() {
        if cancelled(request) {
            return Err(ScanError::Cancelled(
                "source lint scan cancelled before publication".to_owned(),
            ));
        }
        if crate::texture_lint::is_texture_path(&source.rel) {
            let outcome = assess_texture(request, source)?;
            if outcome.incomplete {
                complete = false;
            }
            findings.extend(outcome.findings.iter().cloned().map(cache_texture_finding));
            rows.push(texture_evidence(source, &outcome));
            bytes_hashed = bytes_hashed.saturating_add(u64::try_from(source.bytes.len()).unwrap_or(u64::MAX));
            continue;
        }
        let settings = &resolved[index];
        let geometry_report = if settings.geometry.mode == crate::gltf_lint::CoincidenceMode::Robust {
            // Robust coincidence (ADR-8347) needs attributes the decode cache does not keep:
            // parse the source and leave the decode cache alone (ADR-8574).
            live_geometry(request, source, &settings.geometry, &mut counters)?
        } else {
            let geometry = load_or_decode(request, source, &settings.geometry, importer, &mut counters)?;
            evaluate_captured(&geometry, &settings.geometry)
                .map_err(|error| ScanError::lint(format!("{}: {error}", source.rel)))?
        };
        findings.extend(findings_from_gltf_report(&source.rel, &geometry_report));
        let material = adapt_material(&source.path, &settings.materials, request.fix)
            .map_err(|error| ScanError::lint(format!("{}: {} {}", source.rel, error.code, error.message)))?;
        findings.extend(findings_from_material_report(&source.rel, &material));
        findings.extend(shared_texture_findings(request, source)?);
        rows.push(source_row(
            source,
            &geometry_report,
            material_count(&source.bytes),
        ));
        bytes_hashed = bytes_hashed.saturating_add(u64::try_from(source.bytes.len()).unwrap_or(u64::MAX));
    }
    suppress(&mut findings, &policy, &prepared);
    sort_findings(&mut findings);
    let report = assemble_report(ReportParts {
        target: &request.target,
        evidence_key: &evidence_key,
        source_hash: &deps_hash,
        settings_hash: &settings_hash,
        sources: rows,
        findings,
        seen: prepared.len(),
        bytes_hashed,
        complete,
    })?;
    let body = report.to_json().map_err(ScanError::from_source)?;
    write_atomic(&evidence_path(&request.project, &evidence_key), &body)?;
    write_atomic(&report_path, &body)?;
    Ok(outcome(report, counters, report_path))
}

/// Runs source diagnostics before `publish`.
///
/// `publish` is called only when the cook gate allows the step to continue. `cook: Off` skips
/// the scan. `cook: Warn` keeps a geometry or texture warning from stopping the step. `cook: Fail`,
/// and a package step whose config does not name `cook`, stops before `publish` when a glTF
/// geometry warning or a texture warning is unsuppressed, or the report is incomplete. Material
/// warnings do not by themselves stop the step. An invalid policy is an error for every gate that runs.
///
/// # Errors
/// The scan failed, or `publish` returned an error. A blocking finding is `Err` and does not
/// call `publish`.
pub fn run_publish_gate<T, E: std::fmt::Display>(
    request: &PublishRequest,
    publish: impl FnOnce() -> Result<T, E>,
) -> Result<(LintStatus, T), String> {
    let status = diagnose_before_publish(request).map_err(|error| error.to_string())?;
    if status.blocking {
        return Err(format!(
            "content_lint_failed: source lint blocked publication ({})",
            status.status
        ));
    }
    publish()
        .map(|value| (status, value))
        .map_err(|error| error.to_string())
}

/// Resolves the cook gate and scans when the gate runs.
///
/// # Errors
/// Invalid policy, a cancel during the scan, or a source that cannot be linted.
pub fn diagnose_before_publish(request: &PublishRequest) -> Result<LintStatus, ScanError> {
    let gate = cook_gate_for(&request.project, request.package)?;
    if !gate.runs() {
        let status = LintStatus {
            status: "skipped".to_owned(),
            report: None,
            evidence_key: None,
            blocking: false,
        };
        write_status(&request.project, &status)?;
        return Ok(status);
    }
    let outcome = scan(&ScanRequest {
        project: request.project.clone(),
        target: request.target.clone(),
        paths: Vec::new(),
        changed: None,
        strict: gate == CookGate::Fail,
        fix: false,
        cancel_file: None,
        report_path: None,
        material_profile: None,
        skip_staging: false,
    })?;
    let blocking_warning = outcome.report.findings.iter().any(|finding| {
        !finding.suppressed
            && finding.severity == FindingSeverity::Warning
            && (finding.rule_id.starts_with("gltf_") || finding.rule_id.starts_with("texture_"))
    });
    let blocking = gate == CookGate::Fail && (blocking_warning || !outcome.report.complete);
    let status_name = if blocking {
        "failed"
    } else if outcome
        .report
        .findings
        .iter()
        .any(|finding| !finding.suppressed && finding.severity == FindingSeverity::Warning)
    {
        "warnings"
    } else {
        "clean"
    };
    let status = LintStatus {
        status: status_name.to_owned(),
        report: Some(outcome.report_path.display().to_string()),
        evidence_key: Some(outcome.report.evidence_key.clone()),
        blocking,
    };
    write_status(&request.project, &status)?;
    Ok(status)
}

struct Prepared {
    path: PathBuf,
    rel: String,
    bytes: Vec<u8>,
    hash: String,
    deps: Vec<(String, String)>,
    dep_key: String,
}

fn outcome(report: SourceLintReport, counters: ScanCounters, report_path: PathBuf) -> ScanOutcome {
    let mut rule_ids: Vec<String> = report
        .findings
        .iter()
        .map(|finding| finding.rule_id.clone())
        .collect();
    rule_ids.sort();
    rule_ids.dedup();
    ScanOutcome {
        report,
        counters,
        report_path,
        rule_ids,
    }
}

fn cancelled(request: &ScanRequest) -> bool {
    request.cancel_file.as_ref().is_some_and(|path| path.is_file())
}

fn cache_root(project: &Path) -> PathBuf {
    project.join("build").join("source-lint")
}

fn evidence_path(project: &Path, key: &str) -> PathBuf {
    cache_root(project).join("evidence").join(format!("{key}.json"))
}

fn decode_path(project: &Path, key: &str) -> PathBuf {
    cache_root(project).join("decode").join(format!("{key}.json"))
}

fn status_path(project: &Path) -> PathBuf {
    cache_root(project).join("status.json")
}

fn collect_files(request: &ScanRequest) -> Result<Vec<PathBuf>, ScanError> {
    let mut files = Vec::new();
    if let Some(changed) = &request.changed {
        for rel in changed {
            let path = request.project.join(rel);
            if is_lint_source(&path) && path.is_file() {
                files.push(path);
            }
        }
    } else if request.paths.is_empty() {
        let assets = request.project.join("assets");
        if assets.is_dir() {
            walk_sources(&assets, request.skip_staging, &mut files)?;
        }
    } else {
        for path in &request.paths {
            // Registry scopes name every selected asset. A missing texture or scene is not a
            // glTF source, so it is not this scan's error. A missing `.gltf`/`.glb` still is.
            let meta = match std::fs::symlink_metadata(path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_gltf(path) => {
                    continue;
                }
                Err(error) => {
                    return Err(ScanError::io(format!("cannot read {}: {error}", path.display())));
                }
            };
            if meta.file_type().is_symlink() {
                return Err(ScanError::io(format!(
                    "source path must not be a symlink: {}",
                    path.display()
                )));
            }
            if meta.is_dir() {
                walk_sources(path, request.skip_staging, &mut files)?;
            } else if is_lint_source(path) {
                files.push(path.clone());
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn walk_sources(dir: &Path, skip_staging: bool, out: &mut Vec<PathBuf>) -> Result<(), ScanError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| ScanError::io(format!("cannot read {}: {error}", dir.display())))?;
    let mut listed = Vec::new();
    for entry in entries {
        listed.push(entry.map_err(|error| ScanError::io(format!("cannot read {}: {error}", dir.display())))?);
    }
    listed.sort_by_key(|entry| entry.file_name());
    for entry in listed {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if skip_staging && name.starts_with('_') {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| ScanError::io(format!("cannot read {}: {error}", entry.path().display())))?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_sources(&path, skip_staging, out)?;
        } else if file_type.is_file() && is_lint_source(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_gltf(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gltf") || ext.eq_ignore_ascii_case("glb"))
}

fn is_lint_source(path: &Path) -> bool {
    is_gltf(path) || crate::texture_lint::is_texture_path(&path.to_string_lossy())
}

fn prepare_source(project: &Path, path: &Path) -> Result<Prepared, ScanError> {
    let bytes = std::fs::read(path)
        .map_err(|error| ScanError::io(format!("cannot read {}: {error}", path.display())))?;
    let hash = content_hash(&bytes);
    let deps = external_deps(path, &bytes)?;
    let rel = project_relative(project, path);
    let dep_key = dependency_hash(
        gltf_importer_version(),
        &[PreparedStub {
            rel: rel.clone(),
            hash: hash.clone(),
            deps: deps.clone(),
        }],
    );
    Ok(Prepared {
        path: path.to_path_buf(),
        rel,
        bytes,
        hash,
        deps,
        dep_key,
    })
}

struct PreparedStub {
    rel: String,
    hash: String,
    deps: Vec<(String, String)>,
}

fn dependency_hash(importer: u32, sources: &[impl DepRow]) -> String {
    let mut text = format!("{importer}\n");
    for source in sources {
        text.push_str(source.rel());
        text.push('\n');
        text.push_str(source.hash());
        text.push('\n');
        for (name, hash) in source.deps() {
            text.push_str(name);
            text.push('\n');
            text.push_str(hash);
            text.push('\n');
        }
    }
    content_hash(text.as_bytes())
}

trait DepRow {
    fn rel(&self) -> &str;
    fn hash(&self) -> &str;
    fn deps(&self) -> &[(String, String)];
}

impl DepRow for Prepared {
    fn rel(&self) -> &str {
        &self.rel
    }
    fn hash(&self) -> &str {
        &self.hash
    }
    fn deps(&self) -> &[(String, String)] {
        &self.deps
    }
}

impl DepRow for PreparedStub {
    fn rel(&self) -> &str {
        &self.rel
    }
    fn hash(&self) -> &str {
        &self.hash
    }
    fn deps(&self) -> &[(String, String)] {
        &self.deps
    }
}

fn external_deps(path: &Path, bytes: &[u8]) -> Result<Vec<(String, String)>, ScanError> {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    // Images and GLB have no JSON buffer table. Parsing those bytes as JSON would fail the scan.
    if !extension.eq_ignore_ascii_case("gltf") {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| ScanError::lint(format!("{} is not JSON: {error}", path.display())))?;
    let parent = path
        .parent()
        .ok_or_else(|| ScanError::io(format!("{} has no parent directory", path.display())))?;
    let mut deps = Vec::new();
    for key in ["buffers", "images"] {
        let Some(items) = value.get(key).and_then(|item| item.as_array()) else {
            continue;
        };
        for item in items {
            let Some(uri) = item.get("uri").and_then(|uri| uri.as_str()) else {
                continue;
            };
            if uri.starts_with("data:") {
                continue;
            }
            let relative = uri.split('?').next().unwrap_or(uri);
            if relative.contains('\\') || relative.starts_with('/') || relative.contains("..") {
                return Err(ScanError::lint(format!(
                    "{} names a buffer or image outside the source directory",
                    path.display()
                )));
            }
            let dep_path = parent.join(relative);
            let dep_bytes = std::fs::read(&dep_path)
                .map_err(|error| ScanError::io(format!("cannot read {}: {error}", dep_path.display())))?;
            deps.push((relative.replace('\\', "/"), content_hash(&dep_bytes)));
        }
    }
    deps.sort();
    Ok(deps)
}

fn project_relative(project: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(project).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn settings_fingerprint(
    project: &Path,
    target: &str,
    prepared: &[Prepared],
    resolved: &[crate::source_lint::ResolvedLintSettings],
) -> Result<String, ScanError> {
    let mut text = String::new();
    text.push_str(target);
    text.push('\n');
    if prepared
        .iter()
        .any(|source| crate::texture_lint::is_texture_path(&source.rel))
    {
        let stamp = crate::texture_lint::project_stamp(project, target).map_err(ScanError::policy)?;
        text.push_str(&stamp);
        text.push('\n');
        for source in prepared {
            if !crate::texture_lint::is_texture_path(&source.rel) {
                continue;
            }
            text.push_str(&source.rel);
            text.push('\n');
            text.push_str(
                &crate::texture_settings::TextureImporterSettings::for_source(&source.path).fingerprint(),
            );
            text.push('\n');
        }
    }
    for (source, settings) in prepared.iter().zip(resolved) {
        text.push_str(&source.rel);
        text.push('\n');
        text.push_str(
            &serde_json::to_string(&settings.geometry)
                .map_err(|error| ScanError::Report(error.to_string()))?,
        );
        text.push('\n');
        text.push_str(
            &serde_json::to_string(&settings.materials)
                .map_err(|error| ScanError::Report(error.to_string()))?,
        );
        text.push('\n');
    }
    Ok(content_hash(text.as_bytes()))
}

fn load_or_decode(
    request: &ScanRequest,
    source: &Prepared,
    settings: &crate::gltf_lint::GltfLintSettings,
    importer: u32,
    counters: &mut ScanCounters,
) -> Result<CapturedGeometry, ScanError> {
    let path = decode_path(&request.project, &source.dep_key);
    if let Some(captured) = read_decode(&path, importer)? {
        counters.cache_hits = counters.cache_hits.saturating_add(1);
        return Ok(captured);
    }
    let cancel = || cancelled(request);
    let mut budget = crate::content_lint::ContentBudget::new(settings.input_limits.clone(), &cancel);
    let (report, captured) = lint_gltf_captured(&source.path, settings, &mut budget).map_err(|error| {
        if error.code() == "content_lint_cancelled" {
            ScanError::Cancelled(error.to_string())
        } else {
            ScanError::lint(format!("{}: {error}", source.rel))
        }
    })?;
    counters.source_parses = counters.source_parses.saturating_add(1);
    counters.decoded_bytes = counters
        .decoded_bytes
        .saturating_add(report.input_usage.decoded_bytes)
        .saturating_add(position_bytes(&captured));
    let body = serde_json::to_vec(&DecodeFile {
        importer,
        geometry: captured.clone(),
    })
    .map_err(|error| ScanError::Report(error.to_string()))?;
    write_atomic(&path, &body)?;
    Ok(captured)
}

/// Lints one source from its bytes without reading or writing the decode cache.
fn live_geometry(
    request: &ScanRequest,
    source: &Prepared,
    settings: &crate::gltf_lint::GltfLintSettings,
    counters: &mut ScanCounters,
) -> Result<crate::gltf_lint::GltfLintReport, ScanError> {
    let cancel = || cancelled(request);
    let mut budget = crate::content_lint::ContentBudget::new(settings.input_limits.clone(), &cancel);
    let (report, _) = lint_gltf_captured(&source.path, settings, &mut budget).map_err(|error| {
        if error.code() == "content_lint_cancelled" {
            ScanError::Cancelled(error.to_string())
        } else {
            ScanError::lint(format!("{}: {error}", source.rel))
        }
    })?;
    counters.source_parses = counters.source_parses.saturating_add(1);
    counters.decoded_bytes = counters
        .decoded_bytes
        .saturating_add(report.input_usage.decoded_bytes);
    Ok(report)
}

fn position_bytes(captured: &CapturedGeometry) -> u64 {
    captured
        .meshes
        .iter()
        .flat_map(|mesh| &mesh.primitives)
        .fold(0_u64, |total, primitive| {
            total.saturating_add(
                u64::try_from(primitive.positions_bits.len().saturating_mul(12)).unwrap_or(u64::MAX),
            )
        })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DecodeFile {
    importer: u32,
    geometry: CapturedGeometry,
}

fn read_decode(path: &Path, importer: u32) -> Result<Option<CapturedGeometry>, ScanError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ScanError::io(format!("cannot read {}: {error}", path.display()))),
    };
    let file: DecodeFile = serde_json::from_slice(&bytes)
        .map_err(|error| ScanError::lint(format!("decode cache {} is unusable: {error}", path.display())))?;
    if file.importer != importer {
        return Ok(None);
    }
    Ok(Some(file.geometry))
}

fn read_evidence(project: &Path, key: &str) -> Result<Option<SourceLintReport>, ScanError> {
    let path = evidence_path(project, key);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ScanError::io(format!("cannot read {}: {error}", path.display()))),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| ScanError::lint(format!("evidence cache {} is unusable: {error}", path.display())))
}

fn material_count(bytes: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| value.get("materials")?.as_array().map(|items| items.len() as u64))
        .unwrap_or(0)
}

fn assess_texture(
    request: &ScanRequest,
    source: &Prepared,
) -> Result<crate::texture_lint::TextureLintOutcome, ScanError> {
    let platform = crate::texture_lint::platform_for_target(&request.target);
    let policy = crate::texture_policy::TexturePolicy::for_project(&request.project, platform)
        .map_err(ScanError::policy)?;
    let settings = crate::texture_settings::TextureImporterSettings::for_source(&source.path);
    let budget = crate::content_lint::ContentLimits::default().max_decoded_bytes;
    Ok(crate::texture_lint::assess(
        &crate::texture_lint::TextureLintRequest {
            path: &source.rel,
            bytes: &source.bytes,
            file_len: None,
            platform,
            policy,
            settings,
            usages: &[],
            max_decoded_bytes: budget,
        },
    ))
}

fn shared_texture_findings(request: &ScanRequest, source: &Prepared) -> Result<Vec<LintFinding>, ScanError> {
    let shared = crate::texture_lint::shared_usage_findings(
        &request.project,
        &source.rel,
        &source.bytes,
        &request.target,
    )
    .map_err(|error| {
        if error.starts_with("cannot read") {
            ScanError::io(error)
        } else {
            ScanError::policy(error)
        }
    })?;
    Ok(shared.into_iter().map(cache_texture_finding).collect())
}

fn cache_texture_finding(finding: crate::texture_lint::TextureLintFinding) -> LintFinding {
    LintFinding {
        path: finding.path,
        rule_id: finding.rule_id,
        location: finding.location,
        message: finding.message,
        severity: FindingSeverity::Warning,
        suppressed: false,
        suppression_reason: None,
        effect: Some("preserves_appearance".to_owned()),
    }
}

fn texture_evidence(source: &Prepared, outcome: &crate::texture_lint::TextureLintOutcome) -> SourceEvidence {
    let mut metrics = std::collections::BTreeMap::new();
    metrics.insert("texture_width".to_owned(), edge_metric(outcome.width));
    metrics.insert("texture_height".to_owned(), edge_metric(outcome.height));
    metrics.insert(
        "decoded_bytes".to_owned(),
        match outcome.decoded_bytes {
            Some(value) => MetricReading::known(value, "texture"),
            None => MetricReading::unknown("texture"),
        },
    );
    SourceEvidence {
        path: source.rel.clone(),
        content_hash: source.hash.clone(),
        support: if outcome.incomplete {
            SupportStatus::Incomplete
        } else {
            SupportStatus::Supported
        },
        reason: outcome
            .incomplete
            .then(|| crate::texture_lint::RULE_BUDGET_UNKNOWN.to_owned()),
        metrics,
    }
}

fn edge_metric(value: Option<u32>) -> MetricReading {
    match value {
        Some(value) => MetricReading::known(u64::from(value), "texture"),
        None => MetricReading::unknown("texture"),
    }
}

fn source_row(
    source: &Prepared,
    geometry: &crate::gltf_lint::GltfLintReport,
    materials: u64,
) -> SourceEvidence {
    let mut metrics = std::collections::BTreeMap::new();
    metrics.insert(
        "triangles".to_owned(),
        MetricReading::known(
            u64::try_from(geometry.triangles).unwrap_or(u64::MAX),
            "gltf_geometry",
        ),
    );
    metrics.insert(
        "meshes".to_owned(),
        MetricReading::known(
            u64::try_from(geometry.meshes).unwrap_or(u64::MAX),
            "gltf_geometry",
        ),
    );
    metrics.insert(
        "materials".to_owned(),
        MetricReading::known(materials, "gltf_materials"),
    );
    SourceEvidence {
        path: source.rel.clone(),
        content_hash: source.hash.clone(),
        support: SupportStatus::Supported,
        reason: None,
        metrics,
    }
}

fn suppress(findings: &mut [LintFinding], policy: &SourceLintPolicy, prepared: &[Prepared]) {
    for finding in findings {
        let Some(source) = prepared.iter().find(|source| source.rel == finding.path) else {
            continue;
        };
        for suppression in &policy.suppressions {
            if suppression.rule_id != finding.rule_id || suppression.path != finding.path {
                continue;
            }
            if suppression.content_hash != source.hash {
                continue;
            }
            if suppression
                .location
                .as_ref()
                .is_some_and(|location| location != &finding.location)
            {
                continue;
            }
            finding.suppressed = true;
            finding.suppression_reason = Some(suppression.reason.clone());
            break;
        }
    }
}

fn sort_findings(findings: &mut [LintFinding]) {
    findings.sort_by(|left, right| {
        (
            &left.path,
            &left.rule_id,
            &left.location,
            &left.message,
            left.suppressed,
        )
            .cmp(&(
                &right.path,
                &right.rule_id,
                &right.location,
                &right.message,
                right.suppressed,
            ))
    });
}

struct ReportParts<'a> {
    target: &'a str,
    evidence_key: &'a str,
    source_hash: &'a str,
    settings_hash: &'a str,
    sources: Vec<SourceEvidence>,
    findings: Vec<LintFinding>,
    seen: usize,
    bytes_hashed: u64,
    complete: bool,
}

fn assemble_report(mut parts: ReportParts<'_>) -> Result<SourceLintReport, ScanError> {
    parts.sources.sort_by(|left, right| left.path.cmp(&right.path));
    let ReportParts {
        target,
        evidence_key,
        source_hash,
        settings_hash,
        sources,
        findings,
        seen,
        bytes_hashed,
        complete,
    } = parts;
    let findings_len = findings.len();
    Ok(SourceLintReport {
        format_version: SOURCE_LINT_REPORT_VERSION,
        complete,
        target: target.to_owned(),
        evidence_key: evidence_key.to_owned(),
        source_hash: source_hash.to_owned(),
        settings_hash: settings_hash.to_owned(),
        tool_hash: content_hash(format!("{SOURCE_LINT_TOOL}\n{SOURCE_LINT_REPORT_VERSION}").as_bytes()),
        sources,
        findings,
        work: WorkCounters {
            sources_seen: seen,
            sources_processed: seen,
            bytes_hashed,
            findings: findings_len,
            findings_kept: findings_len,
            rules_checked: seen.saturating_mul(rules().len()),
        },
    })
}

fn cook_gate_for(project: &Path, package: bool) -> Result<CookGate, ScanError> {
    let path = project.join("config").join("validation.ron");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(if package { CookGate::Fail } else { CookGate::Warn });
        }
        Err(error) => {
            return Err(ScanError::io(format!("cannot read {}: {error}", path.display())));
        }
    };
    let file = ValidationFile::from_ron(&text).map_err(|error| ScanError::policy(error.to_string()))?;
    if package && !names_cook(&text) {
        Ok(CookGate::Fail)
    } else {
        Ok(file.cook)
    }
}

fn names_cook(text: &str) -> bool {
    match ron::from_str::<ron::Value>(text) {
        Ok(ron::Value::Map(map)) => map
            .iter()
            .any(|(key, _)| matches!(key, ron::Value::String(name) if name == "cook")),
        _ => false,
    }
}

fn write_status(project: &Path, status: &LintStatus) -> Result<(), ScanError> {
    let body = serde_json::to_vec(status).map_err(|error| ScanError::Report(error.to_string()))?;
    write_atomic(&status_path(project), &body)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ScanError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ScanError::io(format!("cannot create {}: {error}", parent.display())))?;
    }
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "lint".to_owned());
    let tmp = path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()));
    let write_result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(ScanError::io(format!("cannot write {}: {error}", path.display())));
    }
    Ok(())
}
