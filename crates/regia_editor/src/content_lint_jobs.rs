// SPDX-License-Identifier: MPL-2.0
//! Source-lint jobs for the editor (ADR-8391).
//!
//! One `content_lint` worker parses `.gltf`, `.glb`, and texture sources off the editor thread.
//! The editor publishes Problems rows only when the source generation, the settings
//! generation, and the BLAKE3 content hashes still match the scan that produced them.
//! Save and import queue one coalesced scan. Threshold edits write the existing
//! `config/source_lint.ron` overlays. Fix links are previews; nothing here rewrites a source.
//!
//! The publication rule is editor-side. It uses [`regia_asset_pipeline::source_lint`] precedence
//! (ADR-8248) and [`crate::editor_jobs::WorkerJob`] (ADR-7664). See `docs/provenance.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use regia_agent::extension::AgentHandler;
use regia_agent::protocol::{self, RpcError};
use regia_agent::req_path;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::source_lint::{
    self, FindingSeverity, GeometryOverlay, LintFinding, MaterialOverlay, RuleOverlay, SourceDocument,
    SourceLintPolicy, SourceLintReport, produce_in,
};
use regia_asset_pipeline::validators::Severity;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::asset_panels::{AssetPanels, ProblemRow, SEVERITIES};
use crate::editor_jobs::{WorkerContext, WorkerJob};

const HINT: &str = "see book/agents/content-lint.md";
const VALIDATOR: &str = "source_lint";
const TASK_ID: &str = "content_lint";
const JOB_KIND: &str = "content_lint";

/// Encoded-byte ceiling for the legacy synchronous reads (`asset.gltf_lint`,
/// `asset.gltf_material_lint`). Larger files use `asset.content_lint_start`.
pub const SYNC_MAX_ENCODED_BYTES: u64 = 1_048_576;

/// `Some(reason, message)` when `path` is over [`SYNC_MAX_ENCODED_BYTES`].
/// Metadata errors fall through to the linter.
pub(crate) fn sync_limit_error(path: &Path) -> Option<(&'static str, String)> {
    let len = path.metadata().ok()?.len();
    if len > SYNC_MAX_ENCODED_BYTES {
        Some((
            "content_lint_limit",
            format!(
                "encoded size {len} bytes exceeds the synchronous ceiling of {SYNC_MAX_ENCODED_BYTES}; use asset.content_lint_start"
            ),
        ))
    } else {
        None
    }
}

/// Park/release gate shared with a test. The worker sends on `parked` and then waits on `release`.
struct PauseGate {
    parked: Sender<()>,
    release: Mutex<Receiver<()>>,
}

impl PauseGate {
    fn park(&self) {
        let _ = self.parked.send(());
        let release = self.release.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = release.recv();
    }
}

/// Sends one completion ping when dropped, after the scan slot has been filled.
struct DonePing(Option<Sender<()>>);

impl Drop for DonePing {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

/// What the worker stored before it pinged completion.
struct ScanSlot {
    end: Mutex<Option<ScanEnd>>,
}

enum ScanEnd {
    Cancelled,
    Failed {
        code: &'static str,
        message: String,
    },
    Ready {
        report: Box<SourceLintReport>,
        hashes: BTreeMap<String, String>,
    },
}

/// The test channel closed before the worker parked, released, or stored an outcome.
#[derive(Debug)]
pub struct LintGateClosed;

impl std::fmt::Display for LintGateClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source-lint test gate closed")
    }
}

impl std::error::Error for LintGateClosed {}

struct ActiveScan {
    job: WorkerJob<()>,
    slot: Arc<ScanSlot>,
    source_generation: u64,
    settings_generation: u64,
    paths: Vec<String>,
    target: String,
}

struct Last {
    state: &'static str,
    code: Option<&'static str>,
    job: String,
    report: Option<SourceLintReport>,
    findings_published: bool,
}

/// Editor queue for source lint. One worker at a time.
#[derive(Resource, Default)]
pub struct ContentLintJobs {
    source_generation: u64,
    settings_generation: u64,
    queued: bool,
    pending: BTreeSet<String>,
    active: Option<ActiveScan>,
    reap: Option<WorkerJob<()>>,
    scans_started: u64,
    discarded: u64,
    drop_publish: bool,
    /// Empty means `default`. `start` and threshold edits set it for the follow-up scan.
    scan_target: String,
    last: Option<Last>,
    pause: Option<Arc<PauseGate>>,
    done: Option<Sender<()>>,
}

/// Test handle for a paused worker. Channels only: no sleeps.
pub struct LintTestGate {
    parked: Receiver<()>,
    release: Sender<()>,
    done: Receiver<()>,
}

impl LintTestGate {
    /// Blocks until the worker reaches its next park.
    ///
    /// # Errors
    /// The worker dropped the gate.
    pub fn wait_parked(&self) -> Result<(), LintGateClosed> {
        self.parked.recv().map(|_| ()).map_err(|_| LintGateClosed)
    }

    /// Lets the worker leave the current park.
    ///
    /// # Errors
    /// The worker dropped the gate.
    pub fn release_worker(&self) -> Result<(), LintGateClosed> {
        self.release.send(()).map_err(|_| LintGateClosed)
    }

    /// Blocks until the worker has stored its outcome.
    ///
    /// # Errors
    /// The worker dropped the completion sender.
    pub fn wait_done(&self) -> Result<(), LintGateClosed> {
        self.done.recv().map(|_| ()).map_err(|_| LintGateClosed)
    }
}

/// Completion ping with no park. The worker stores its outcome before the ping.
pub struct LintDone {
    done: Receiver<()>,
}

impl LintDone {
    /// Blocks until the worker has stored its outcome.
    ///
    /// # Errors
    /// The worker dropped the completion sender.
    pub fn wait_done(&self) -> Result<(), LintGateClosed> {
        self.done.recv().map(|_| ()).map_err(|_| LintGateClosed)
    }
}

fn error(reason: &str, message: impl Into<String>) -> RpcError {
    let mut rpc = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = rpc.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    rpc
}

fn is_model(path: &str) -> bool {
    if path.starts_with('/') || path.contains(':') {
        return false;
    }
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) => {
            ext.eq_ignore_ascii_case("gltf")
                || ext.eq_ignore_ascii_case("glb")
                || regia_asset_pipeline::texture_lint::is_texture_path(name)
        }
        None => false,
    }
}

fn take_jobs(world: &mut World) -> ContentLintJobs {
    world.remove_resource::<ContentLintJobs>().unwrap_or_default()
}

fn clear_rows(world: &mut World) {
    if let Some(mut panels) = world.get_resource_mut::<AssetPanels>() {
        panels.problems.rows.retain(|row| row.validator != VALIDATOR);
    }
}

fn forget_findings(jobs: &mut ContentLintJobs) {
    if let Some(last) = jobs.last.as_mut() {
        last.report = None;
        last.findings_published = false;
    }
}

/// A project-relative glTF or texture source changed. Queues one scan; does not start it.
pub fn note_path(world: &mut World, path: &str) {
    if !is_model(path) {
        return;
    }
    {
        let mut jobs = world.get_resource_or_insert_with(ContentLintJobs::default);
        jobs.source_generation = jobs.source_generation.wrapping_add(1);
        jobs.pending.insert(path.to_string());
        jobs.queued = true;
        forget_findings(&mut jobs);
    }
    clear_rows(world);
}

/// Imported paths. Paths that are not glTF or texture sources are ignored. One later tick scans the whole burst.
pub fn note_imported(world: &mut World, paths: &[String]) {
    for path in paths {
        note_path(world, path);
    }
}

/// How many scans have been started on this world.
#[must_use]
pub fn scans_started(world: &World) -> u64 {
    world
        .get_resource::<ContentLintJobs>()
        .map_or(0, |jobs| jobs.scans_started)
}

/// Whether a scan is waiting for the next tick.
#[must_use]
pub fn scan_queued(world: &World) -> bool {
    world
        .get_resource::<ContentLintJobs>()
        .is_some_and(|jobs| jobs.queued)
}

/// Scans whose results were dropped because the source or the settings moved.
#[must_use]
pub fn discarded_scans(world: &World) -> u64 {
    world
        .get_resource::<ContentLintJobs>()
        .map_or(0, |jobs| jobs.discarded)
}

/// Installs a two-park gate and a completion ping. Call before `asset.content_lint_start`.
pub fn inject_pause(world: &mut World) -> LintTestGate {
    let (parked_tx, parked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let mut jobs = world.get_resource_or_insert_with(ContentLintJobs::default);
    jobs.pause = Some(Arc::new(PauseGate {
        parked: parked_tx,
        release: Mutex::new(release_rx),
    }));
    jobs.done = Some(done_tx);
    LintTestGate {
        parked: parked_rx,
        release: release_tx,
        done: done_rx,
    }
}

/// Installs a completion ping and no park. Call before `asset.content_lint_start`.
pub fn inject_done(world: &mut World) -> LintDone {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let mut jobs = world.get_resource_or_insert_with(ContentLintJobs::default);
    jobs.pause = None;
    jobs.done = Some(done_tx);
    LintDone { done: done_rx }
}

fn encode_query(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn preview_href(path: &str, rule: &str, location: &str) -> String {
    format!(
        "content-lint://fix-preview?path={}&rule={}&location={}",
        encode_query(path),
        encode_query(rule),
        encode_query(location)
    )
}

fn severity_name(severity: FindingSeverity) -> &'static str {
    match severity {
        FindingSeverity::Warning => "warning",
        FindingSeverity::Info => "info",
    }
}

fn panel_severity(severity: FindingSeverity) -> Severity {
    match severity {
        FindingSeverity::Warning => Severity::Warning,
        FindingSeverity::Info => Severity::Info,
    }
}

fn severity_index(severity: Severity) -> usize {
    SEVERITIES
        .iter()
        .position(|item| *item == severity)
        .unwrap_or(SEVERITIES.len())
}

fn publish_rows(world: &mut World, findings: &[LintFinding]) {
    let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
    panels.problems.rows.retain(|row| row.validator != VALIDATOR);
    panels
        .problems
        .rows
        .extend(findings.iter().map(|finding| ProblemRow {
            severity: panel_severity(finding.severity),
            code: finding.rule_id.clone(),
            validator: VALIDATOR.to_string(),
            asset: finding.path.clone(),
            text: finding.message.clone(),
            hint: Some(preview_href(&finding.path, &finding.rule_id, &finding.location)),
        }));
    panels
        .problems
        .rows
        .sort_by_key(|row| severity_index(row.severity));
    panels.problems.selected_row = None;
}

fn finding_value(finding: &LintFinding) -> Value {
    json!({
        "path": finding.path,
        "rule_id": finding.rule_id,
        "location": finding.location,
        "message": finding.message,
        "severity": severity_name(finding.severity),
        "suppressed": finding.suppressed,
        "fix_preview": preview_href(&finding.path, &finding.rule_id, &finding.location),
    })
}

fn parse_panel_severity(name: &str) -> Result<Severity, RpcError> {
    match name {
        "error" => Ok(Severity::Error),
        "warning" => Ok(Severity::Warning),
        "info" => Ok(Severity::Info),
        _ => Err(error(
            "content_lint_params",
            "severity must be error, warning, or info",
        )),
    }
}

/// Shows one severity, one validator, or both, on the Problems panel.
///
/// # Errors
/// `content_lint_params` when `severity` is not `error`, `warning`, or `info`.
pub fn set_problems_filter(
    world: &mut World,
    severity: Option<&str>,
    validator: Option<&str>,
) -> Result<(), RpcError> {
    let keep = severity.map(parse_panel_severity).transpose()?;
    let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
    panels.problems.validator = validator.filter(|text| !text.is_empty()).map(str::to_string);
    panels.problems.hidden_severities.clear();
    if let Some(keep) = keep {
        for item in SEVERITIES {
            if item != keep {
                panels.problems.hidden_severities.push(item);
            }
        }
    }
    Ok(())
}

fn object(params: &Value) -> Result<&serde_json::Map<String, Value>, RpcError> {
    params
        .as_object()
        .ok_or_else(|| error("content_lint_params", "expected an object"))
}

fn reject_unknown(fields: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), RpcError> {
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(error("content_lint_params", "unknown parameter"));
    }
    Ok(())
}

fn optional_string(fields: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, RpcError> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(error("content_lint_params", format!("{key} must be a string"))),
    }
}

fn target_name(fields: &serde_json::Map<String, Value>, required: bool) -> Result<String, RpcError> {
    match fields.get("target") {
        None if !required => Ok("default".to_string()),
        None => Err(error("content_lint_params", "target is required")),
        Some(Value::String(text)) if text.is_empty() => Err(error("content_lint_target", "target is empty")),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(error("content_lint_params", "target must be a string")),
    }
}

fn profile_overlay<'a>(policy: &'a SourceLintPolicy, target: &str) -> Option<&'a RuleOverlay> {
    if target == "default" {
        policy.profiles.get("default")
    } else {
        policy.profiles.get(target)
    }
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    regia_asset::import_rules::glob_match(&pattern.to_ascii_lowercase(), &path.to_ascii_lowercase())
}

fn geometry_layer(
    policy: &SourceLintPolicy,
    target: &str,
    path: Option<&str>,
    set: impl Fn(&GeometryOverlay) -> bool,
) -> &'static str {
    let mut layer = "engine_defaults";
    if policy.defaults.geometry.as_ref().is_some_and(&set) {
        layer = "project_defaults";
    }
    if profile_overlay(policy, target)
        .and_then(|row| row.geometry.as_ref())
        .is_some_and(&set)
    {
        layer = "target_profile";
    }
    if let Some(path) = path {
        for asset in &policy.assets {
            if glob_matches(&asset.glob, path) && asset.geometry.as_ref().is_some_and(&set) {
                layer = "asset_overlay";
            }
        }
    }
    layer
}

fn material_layer(
    policy: &SourceLintPolicy,
    target: &str,
    path: Option<&str>,
    set: impl Fn(&MaterialOverlay) -> bool,
) -> &'static str {
    let mut layer = "engine_defaults";
    if policy.defaults.materials.as_ref().is_some_and(&set) {
        layer = "project_defaults";
    }
    if profile_overlay(policy, target)
        .and_then(|row| row.materials.as_ref())
        .is_some_and(&set)
    {
        layer = "target_profile";
    }
    if let Some(path) = path {
        for asset in &policy.assets {
            if glob_matches(&asset.glob, path) && asset.materials.as_ref().is_some_and(&set) {
                layer = "asset_overlay";
            }
        }
    }
    layer
}

fn provenance(policy: &SourceLintPolicy, target: &str, path: Option<&str>) -> Value {
    json!({
        "geometry": {
            "quantization": geometry_layer(policy, target, path, |row| row.quantization.is_some()),
            "separation": geometry_layer(policy, target, path, |row| row.separation.is_some()),
            "exact_separation": geometry_layer(policy, target, path, |row| row.exact_separation.is_some()),
            "uv_distance": geometry_layer(policy, target, path, |row| row.uv_distance.is_some()),
            "anisotropy_ratio": geometry_layer(policy, target, path, |row| row.anisotropy_ratio.is_some()),
            "anisotropy_floor": geometry_layer(policy, target, path, |row| row.anisotropy_floor.is_some()),
            "max_vertices": geometry_layer(policy, target, path, |row| row.max_vertices.is_some()),
            "max_triangles": geometry_layer(policy, target, path, |row| row.max_triangles.is_some()),
            "max_channels": geometry_layer(policy, target, path, |row| row.max_channels.is_some()),
            "max_animation_elements": geometry_layer(policy, target, path, |row| row.max_animation_elements.is_some()),
            "max_findings": geometry_layer(policy, target, path, |row| row.max_findings.is_some()),
        },
        "materials": {
            "profile": "engine_defaults",
            "default_metallic": material_layer(policy, target, path, |row| row.default_metallic.is_some()),
            "default_roughness": material_layer(policy, target, path, |row| row.default_roughness.is_some()),
            "default_emissive_strength": material_layer(policy, target, path, |row| row.default_emissive_strength.is_some()),
            "default_volume_thickness": material_layer(policy, target, path, |row| row.default_volume_thickness.is_some()),
            "default_attenuation_distance": material_layer(policy, target, path, |row| row.default_attenuation_distance.is_some()),
            "classify_untextured": material_layer(policy, target, path, |row| row.classify_untextured.is_some()),
            "name_rules": material_layer(policy, target, path, |row| row.name_rules.is_some()),
        }
    })
}

fn stored_report<'a>(jobs: &'a ContentLintJobs, target: &str) -> Option<&'a SourceLintReport> {
    let report = jobs.last.as_ref().and_then(|last| last.report.as_ref())?;
    (report.target == target).then_some(report)
}

fn finding_list(report: Option<&SourceLintReport>, path: Option<&str>) -> Vec<Value> {
    let Some(report) = report else {
        return Vec::new();
    };
    report
        .findings
        .iter()
        .filter(|finding| path.is_none_or(|wanted| finding.path == wanted))
        .map(finding_value)
        .collect()
}

/// Effective policy, provenance, and any findings already published for `target`.
///
/// # Errors
/// `content_lint_params`, `content_lint_policy`, `content_lint_target`, `content_lint_io`,
/// or `path_outside_project`.
pub fn policy_answer(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["path", "target"])?;
    let target = target_name(fields, false)?;
    let path_text = optional_string(fields, "path")?;
    let root = req_path::project_root(world);
    if let Some(text) = &path_text {
        req_path::project_path_in(&root, "path", text)?;
    }
    let loaded = SourceLintPolicy::load_project(&root).map_err(|err| error(err.code(), err.to_string()))?;
    let resolved = loaded
        .resolve(&target, path_text.as_deref())
        .map_err(|err| error(err.code(), err.to_string()))?;
    let jobs = world.get_resource::<ContentLintJobs>();
    let report = jobs.as_ref().and_then(|jobs| stored_report(jobs, &target));
    let findings = finding_list(report, path_text.as_deref());
    let work = report.and_then(|report| serde_json::to_value(report.work).ok());
    let mut answer = json!({
        "ok": true,
        "format_version": source_lint::SOURCE_LINT_POLICY_VERSION,
        "target": target,
        "precedence": source_lint::PRECEDENCE,
        "geometry": resolved.geometry,
        "materials": resolved.materials,
        "provenance": provenance(&loaded, &target, path_text.as_deref()),
        "findings": findings,
    });
    if let Some(text) = path_text {
        answer["path"] = json!(text);
    }
    if let Some(work) = work {
        answer["work"] = work;
    }
    Ok(answer)
}

/// The same view `asset.content_lint_policy` returns.
///
/// # Errors
/// The same codes as [`policy_answer`].
pub fn human_view(world: &mut World, target: &str, path: Option<&str>) -> Result<Value, RpcError> {
    let mut params = json!({ "target": target });
    if let Some(path) = path {
        params["path"] = json!(path);
    }
    policy_answer(world, &params)
}

fn merge_geometry(into: &mut Option<GeometryOverlay>, over: &GeometryOverlay) {
    let slot = into.get_or_insert_with(GeometryOverlay::default);
    if over.quantization.is_some() {
        slot.quantization = over.quantization;
    }
    if over.separation.is_some() {
        slot.separation = over.separation;
    }
    if over.exact_separation.is_some() {
        slot.exact_separation = over.exact_separation;
    }
    if over.uv_distance.is_some() {
        slot.uv_distance = over.uv_distance;
    }
    if over.anisotropy_ratio.is_some() {
        slot.anisotropy_ratio = over.anisotropy_ratio;
    }
    if over.anisotropy_floor.is_some() {
        slot.anisotropy_floor = over.anisotropy_floor;
    }
    if over.max_vertices.is_some() {
        slot.max_vertices = over.max_vertices;
    }
    if over.max_triangles.is_some() {
        slot.max_triangles = over.max_triangles;
    }
    if over.max_channels.is_some() {
        slot.max_channels = over.max_channels;
    }
    if over.max_animation_elements.is_some() {
        slot.max_animation_elements = over.max_animation_elements;
    }
    if over.max_findings.is_some() {
        slot.max_findings = over.max_findings;
    }
}

fn merge_materials(into: &mut Option<MaterialOverlay>, over: &MaterialOverlay) {
    let slot = into.get_or_insert_with(MaterialOverlay::default);
    if over.default_metallic.is_some() {
        slot.default_metallic = over.default_metallic;
    }
    if over.default_roughness.is_some() {
        slot.default_roughness = over.default_roughness;
    }
    if over.default_emissive_strength.is_some() {
        slot.default_emissive_strength = over.default_emissive_strength;
    }
    if over.default_volume_thickness.is_some() {
        slot.default_volume_thickness = over.default_volume_thickness;
    }
    if over.default_attenuation_distance.is_some() {
        slot.default_attenuation_distance = over.default_attenuation_distance;
    }
    if over.classify_untextured.is_some() {
        slot.classify_untextured = over.classify_untextured;
    }
    if over.name_rules.is_some() {
        slot.name_rules.clone_from(&over.name_rules);
    }
}

fn merge_layer(
    layer: &mut RuleOverlay,
    geometry: Option<&GeometryOverlay>,
    materials: Option<&MaterialOverlay>,
) {
    if let Some(geometry) = geometry {
        merge_geometry(&mut layer.geometry, geometry);
    }
    if let Some(materials) = materials {
        merge_materials(&mut layer.materials, materials);
    }
}

fn note_settings(jobs: &mut ContentLintJobs, path: Option<&str>) {
    jobs.settings_generation = jobs.settings_generation.wrapping_add(1);
    if let Some(path) = path.filter(|path| is_model(path)) {
        jobs.pending.insert(path.to_string());
    }
    jobs.queued = true;
    forget_findings(jobs);
}

fn write_policy(root: &Path, policy: &SourceLintPolicy) -> Result<(), RpcError> {
    let text = ron::ser::to_string_pretty(policy, regia_core::ron_pretty())
        .map_err(|err| error("content_lint_policy", err.to_string()))?;
    SourceLintPolicy::from_ron(&text).map_err(|err| error(err.code(), err.to_string()))?;
    let path = root.join(source_lint::SOURCE_LINT_FILE);
    crate::atomic_write::write_atomic(&path, text.as_bytes()).map_err(|err| error("content_lint_io", err))?;
    Ok(())
}

fn apply_thresholds(
    world: &mut World,
    target: &str,
    geometry: Option<GeometryOverlay>,
    materials: Option<MaterialOverlay>,
    path: Option<&str>,
) -> Result<Value, RpcError> {
    if target.is_empty() {
        return Err(error("content_lint_target", "target is empty"));
    }
    let root = req_path::project_root(world);
    if let Some(path) = path {
        req_path::project_path_in(&root, "path", path)?;
    }
    let mut policy =
        SourceLintPolicy::load_project(&root).map_err(|err| error(err.code(), err.to_string()))?;
    let resolve_as = if target == "project" {
        merge_layer(&mut policy.defaults, geometry.as_ref(), materials.as_ref());
        "default"
    } else {
        let layer = policy.profiles.entry(target.to_string()).or_default();
        merge_layer(layer, geometry.as_ref(), materials.as_ref());
        target
    };
    policy
        .resolve(resolve_as, path)
        .map_err(|err| error(err.code(), err.to_string()))?;
    write_policy(&root, &policy)?;
    {
        let mut jobs = world.get_resource_or_insert_with(ContentLintJobs::default);
        jobs.scan_target = resolve_as.to_string();
        note_settings(&mut jobs, path);
    }
    clear_rows(world);
    let mut params = json!({ "target": resolve_as });
    if let Some(path) = path {
        params["path"] = json!(path);
    }
    policy_answer(world, &params)
}

fn require_assets(world: &World) -> Result<(), RpcError> {
    let permissions = world
        .get_resource::<regia_agent::AgentPermissions>()
        .cloned()
        .unwrap_or_default();
    if permissions.allows(Permission::Assets) {
        Ok(())
    } else {
        Err(permissions.denied_error(Permission::Assets))
    }
}

/// Human threshold edit. Same writer as `asset.content_lint_thresholds`.
///
/// `target` `project` writes project defaults and the returned view resolves `default`.
/// Any other name writes that profile, creating it when it is new.
///
/// # Errors
/// `F0012` without the `assets` permission, or the policy codes from [`policy_answer`].
pub fn human_apply_thresholds(
    world: &mut World,
    target: &str,
    geometry: Option<GeometryOverlay>,
    materials: Option<MaterialOverlay>,
    path: Option<&str>,
) -> Result<Value, RpcError> {
    require_assets(world)?;
    apply_thresholds(world, target, geometry, materials, path)
}

/// Human fix request. Does not write the file. Read-only sessions are refused.
///
/// # Errors
/// `F0012` without the `assets` permission. With `assets`, the value is
/// `content_lint_preview_only` and the source bytes stay as they were.
pub fn human_apply_fix(
    world: &mut World,
    path: &str,
    rule_id: Option<&str>,
    location: Option<&str>,
) -> Result<Value, RpcError> {
    require_assets(world)?;
    preview_only(world, path, rule_id, location)
}

fn preview_only(
    world: &mut World,
    path: &str,
    rule_id: Option<&str>,
    location: Option<&str>,
) -> Result<Value, RpcError> {
    let root = req_path::project_root(world);
    req_path::project_path_in(&root, "path", path)?;
    let href = preview_href(path, rule_id.unwrap_or(""), location.unwrap_or(""));
    Ok(json!({
        "ok": false,
        "code": "content_lint_preview_only",
        "reason": "content_lint_preview_only",
        "fix_preview": href,
    }))
}

struct ScanWork {
    pause: Option<Arc<PauseGate>>,
    done: Option<Sender<()>>,
    slot: Arc<ScanSlot>,
    root: PathBuf,
    paths: Vec<String>,
    target: String,
}

fn stopped_for_cancel(report: &SourceLintReport) -> bool {
    !report.complete
        && report
            .sources
            .iter()
            .any(|row| row.reason.as_deref() == Some("cancelled"))
}

fn compute_scan(cx: &WorkerContext, work: &ScanWork) -> ScanEnd {
    let _span =
        tracing::info_span!("content_lint_job", target = %work.target, sources = work.paths.len()).entered();
    if let Some(pause) = &work.pause {
        pause.park();
    }
    if cx.cancelled() {
        return ScanEnd::Cancelled;
    }
    let mut owned = Vec::with_capacity(work.paths.len());
    let mut hashes = BTreeMap::new();
    for rel in &work.paths {
        if cx.cancelled() {
            return ScanEnd::Cancelled;
        }
        match std::fs::read(work.root.join(rel)) {
            Ok(bytes) => {
                hashes.insert(rel.clone(), source_lint::content_hash(&bytes));
                owned.push((rel.clone(), bytes));
            }
            Err(err) => {
                return ScanEnd::Failed {
                    code: "content_lint_io",
                    message: format!("cannot read {rel}: {err}"),
                };
            }
        }
    }
    let policy = match SourceLintPolicy::load_project(&work.root) {
        Ok(policy) => policy,
        Err(err) => {
            return ScanEnd::Failed {
                code: err.code(),
                message: err.to_string(),
            };
        }
    };
    let documents: Vec<SourceDocument<'_>> = owned
        .iter()
        .map(|(path, bytes)| SourceDocument { path, bytes })
        .collect();
    let report = match produce_in(
        Some(&work.root),
        &documents,
        &policy,
        &work.target,
        &source_lint::ProduceLimits::default(),
        &|| cx.cancelled(),
    ) {
        Ok(report) => report,
        Err(err) => {
            return ScanEnd::Failed {
                code: err.code(),
                message: err.to_string(),
            };
        }
    };
    if let Some(pause) = &work.pause {
        pause.park();
    }
    if cx.cancelled() || stopped_for_cancel(&report) {
        return ScanEnd::Cancelled;
    }
    ScanEnd::Ready {
        report: Box::new(report),
        hashes,
    }
}

fn run_scan(cx: WorkerContext, work: ScanWork) {
    let _ping = DonePing(work.done.clone());
    let end = compute_scan(&cx, &work);
    let mut slot = work.slot.end.lock().unwrap_or_else(PoisonError::into_inner);
    *slot = Some(end);
}

fn spawn_scan(
    world: &mut World,
    jobs: &mut ContentLintJobs,
    paths: Vec<String>,
    target: String,
) -> Result<(), RpcError> {
    if paths.is_empty() {
        return Err(error("content_lint_params", "no source paths are queued"));
    }
    let root = req_path::project_root(world);
    for rel in &paths {
        let abs = req_path::project_path_in(&root, "path", rel)?;
        if !abs.is_file() {
            return Err(RpcError::not_found(format!("no file at {rel}"), HINT));
        }
    }
    let slot = Arc::new(ScanSlot {
        end: Mutex::new(None),
    });
    let work = ScanWork {
        pause: jobs.pause.clone(),
        done: jobs.done.clone(),
        slot: Arc::clone(&slot),
        root,
        paths: paths.clone(),
        target: target.clone(),
    };
    let source_generation = jobs.source_generation;
    let settings_generation = jobs.settings_generation;
    let job = WorkerJob::spawn(
        world,
        JOB_KIND,
        TASK_ID,
        format!("source lint ({} files)", paths.len()),
        "regia-content-lint",
        move |cx| run_scan(cx, work),
    )
    .map_err(|message| error("content_lint_io", message))?;
    jobs.scans_started = jobs.scans_started.wrapping_add(1);
    jobs.queued = false;
    jobs.pending.clear();
    jobs.drop_publish = false;
    jobs.scan_target.clone_from(&target);
    jobs.active = Some(ActiveScan {
        job,
        slot,
        source_generation,
        settings_generation,
        paths,
        target,
    });
    Ok(())
}

fn hashes_match(root: &Path, paths: &[String], recorded: &BTreeMap<String, String>) -> bool {
    for rel in paths {
        let bytes = match std::fs::read(root.join(rel)) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        };
        match recorded.get(rel) {
            Some(hash) if hash == &source_lint::content_hash(&bytes) => {}
            _ => return false,
        }
    }
    true
}

fn cancelled_value(job: &str) -> Value {
    json!({
        "ok": true,
        "state": "cancelled",
        "code": "content_lint_cancelled",
        "job": job,
        "findings_published": false,
    })
}

fn finish_job(world: &mut World, job: &WorkerJob<()>, value: Value, cancelled: bool) {
    job.end(world, Ok((value, cancelled)));
}

fn fail_job(
    world: &mut World,
    jobs: &mut ContentLintJobs,
    job: &WorkerJob<()>,
    code: &'static str,
    message: String,
) {
    let id = job.id.clone();
    job.end(world, Err((code, message)));
    jobs.last = Some(Last {
        state: "failed",
        code: Some(code),
        job: id,
        report: None,
        findings_published: false,
    });
}

fn settle(world: &mut World, jobs: &mut ContentLintJobs, active: &ActiveScan, end: ScanEnd) {
    let root = req_path::project_root(world);
    let cancel = jobs.drop_publish || active.job.context().cancelled();
    match end {
        ScanEnd::Cancelled => {
            finish_job(world, &active.job, cancelled_value(&active.job.id), true);
            jobs.last = Some(Last {
                state: "cancelled",
                code: Some("content_lint_cancelled"),
                job: active.job.id.clone(),
                report: None,
                findings_published: false,
            });
        }
        ScanEnd::Failed { code, message } => fail_job(world, jobs, &active.job, code, message),
        ScanEnd::Ready { report, hashes } => {
            if cancel || stopped_for_cancel(&report) {
                finish_job(world, &active.job, cancelled_value(&active.job.id), true);
                jobs.last = Some(Last {
                    state: "cancelled",
                    code: Some("content_lint_cancelled"),
                    job: active.job.id.clone(),
                    report: None,
                    findings_published: false,
                });
                return;
            }
            let stale = jobs.source_generation != active.source_generation
                || jobs.settings_generation != active.settings_generation
                || report.target != active.target
                || !hashes_match(&root, &active.paths, &hashes);
            if stale {
                jobs.discarded = jobs.discarded.wrapping_add(1);
                finish_job(
                    world,
                    &active.job,
                    json!({
                        "ok": true,
                        "discarded": true,
                        "code": "content_lint_stale",
                        "job": active.job.id,
                    }),
                    false,
                );
                jobs.last = Some(Last {
                    state: "done",
                    code: Some("content_lint_stale"),
                    job: active.job.id.clone(),
                    report: None,
                    findings_published: false,
                });
                return;
            }
            publish_rows(world, &report.findings);
            let count = report.findings.len();
            finish_job(
                world,
                &active.job,
                json!({
                    "ok": true,
                    "job": active.job.id,
                    "findings": count,
                    "work": report.work,
                }),
                false,
            );
            jobs.last = Some(Last {
                state: "done",
                code: None,
                job: active.job.id.clone(),
                report: Some(*report),
                findings_published: true,
            });
        }
    }
}

fn reap(jobs: &mut ContentLintJobs) {
    if jobs.reap.as_ref().is_some_and(WorkerJob::worker_done)
        && let Some(mut job) = jobs.reap.take()
    {
        let _ = job.take();
    }
}

/// Polls the source-lint worker. Never waits. Starts one queued scan when the worker is idle.
pub fn tick(world: &mut World) {
    if world.get_resource::<ContentLintJobs>().is_none() {
        return;
    }
    let mut jobs = take_jobs(world);
    reap(&mut jobs);
    let finished = jobs.active.as_ref().and_then(|active| {
        active
            .slot
            .end
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    });
    if let Some(end) = finished
        && let Some(active) = jobs.active.take()
    {
        settle(world, &mut jobs, &active, end);
        if active.job.worker_done() {
            let mut job = active.job;
            let _ = job.take();
        } else {
            jobs.reap = Some(active.job);
        }
    } else if jobs
        .active
        .as_ref()
        .is_some_and(|active| active.job.worker_done())
        && let Some(mut active) = jobs.active.take()
    {
        match active.job.take() {
            Some(Err(message)) => fail_job(world, &mut jobs, &active.job, "content_lint_io", message),
            _ => fail_job(
                world,
                &mut jobs,
                &active.job,
                "content_lint_io",
                "the source-lint worker ended without a result".to_string(),
            ),
        }
    } else if let Some(active) = jobs.active.as_ref() {
        let fraction = active.job.context().fraction();
        let message = active.job.context().message();
        active.job.sync_row(world, &message, fraction);
    }
    if jobs.active.is_none() {
        if jobs.pending.is_empty() {
            jobs.queued = false;
        } else if jobs.queued {
            let paths: Vec<String> = jobs.pending.iter().cloned().collect();
            let target = if jobs.scan_target.is_empty() {
                "default".to_string()
            } else {
                jobs.scan_target.clone()
            };
            if let Err(err) = spawn_scan(world, &mut jobs, paths, target) {
                jobs.queued = false;
                jobs.pending.clear();
                let code = err
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("content_lint_io");
                let code = match code {
                    "content_lint_params" => "content_lint_params",
                    "content_lint_target" => "content_lint_target",
                    "content_lint_policy" => "content_lint_policy",
                    _ => "content_lint_io",
                };
                jobs.last = Some(Last {
                    state: "failed",
                    code: Some(code),
                    job: String::new(),
                    report: None,
                    findings_published: false,
                });
            }
        }
    }
    world.insert_resource(jobs);
}

fn status_value(jobs: &ContentLintJobs) -> Value {
    if let Some(active) = &jobs.active {
        return json!({
            "ok": true,
            "state": "running",
            "job": active.job.id,
            "source_generation": active.source_generation,
            "settings_generation": active.settings_generation,
            "queued": jobs.queued,
        });
    }
    if let Some(last) = &jobs.last {
        let mut answer = json!({
            "ok": true,
            "state": last.state,
            "job": last.job,
            "findings_published": last.findings_published,
        });
        if let Some(code) = last.code {
            answer["code"] = json!(code);
        }
        return answer;
    }
    json!({ "ok": true, "state": "idle", "code": "content_lint_idle" })
}

fn read_value(
    jobs: &ContentLintJobs,
    target: &str,
    path: Option<&str>,
    severity: Option<&str>,
    validator: Option<&str>,
) -> Value {
    if let Some(last) = &jobs.last
        && !last.findings_published
        && let Some(code) = last.code
    {
        return json!({ "ok": true, "code": code, "findings": [] });
    }
    let report = jobs
        .last
        .as_ref()
        .and_then(|last| last.report.as_ref())
        .filter(|report| report.target == target);
    let findings: Vec<Value> = report
        .map(|report| {
            report
                .findings
                .iter()
                .filter(|finding| path.is_none_or(|wanted| finding.path == wanted))
                .filter(|finding| severity.is_none_or(|wanted| severity_name(finding.severity) == wanted))
                .filter(|_| validator.is_none_or(|wanted| wanted == VALIDATOR))
                .map(finding_value)
                .collect()
        })
        .unwrap_or_default();
    if report.is_none() {
        return json!({ "ok": true, "code": "content_lint_idle", "findings": [] });
    }
    let mut answer = json!({ "ok": true, "findings": findings });
    if let Some(report) = report
        && let Ok(work) = serde_json::to_value(report.work)
    {
        answer["work"] = work;
        answer["target"] = json!(report.target);
    }
    answer
}

fn dedupe(paths: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

fn start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["paths", "target"])?;
    let target = target_name(fields, false)?;
    let explicit = match fields.get("paths") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut paths = Vec::new();
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| error("content_lint_params", "paths must be strings"))?;
                paths.push(text.to_string());
            }
            Some(dedupe(paths))
        }
        Some(_) => return Err(error("content_lint_params", "paths must be an array of strings")),
    };
    let mut jobs = take_jobs(world);
    let result = (|| {
        if jobs.active.is_some() {
            if let Some(paths) = explicit {
                for path in paths {
                    if is_model(&path) {
                        jobs.pending.insert(path);
                    }
                }
            }
            jobs.queued = true;
            let id = jobs
                .active
                .as_ref()
                .map(|active| active.job.id.clone())
                .unwrap_or_default();
            return Ok(json!({
                "ok": true,
                "state": "running",
                "job": id,
                "queued": true,
                "source_generation": jobs.source_generation,
                "settings_generation": jobs.settings_generation,
            }));
        }
        let paths = match explicit {
            Some(paths) if !paths.is_empty() => paths,
            _ if !jobs.pending.is_empty() => jobs.pending.iter().cloned().collect(),
            _ => {
                return Err(error(
                    "content_lint_params",
                    "paths is required when nothing is queued",
                ));
            }
        };
        spawn_scan(world, &mut jobs, paths, target)?;
        let Some(active) = jobs.active.as_ref() else {
            return Err(error("content_lint_io", "the source-lint worker did not start"));
        };
        Ok(json!({
            "ok": true,
            "state": "running",
            "job": active.job.id,
            "queued": false,
            "source_generation": active.source_generation,
            "settings_generation": active.settings_generation,
        }))
    })();
    world.insert_resource(jobs);
    result
}

fn cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["id"])?;
    let id = optional_string(fields, "id")?;
    let mut jobs = world.get_resource_or_insert_with(ContentLintJobs::default);
    let running = if let Some(active) = jobs.active.as_ref() {
        if id.as_ref().is_some_and(|wanted| wanted != &active.job.id) {
            return Err(error("content_lint_params", "unknown job"));
        }
        active.job.cancel();
        active.job.context().token.cancel();
        Some(active.job.id.clone())
    } else {
        None
    };
    if let Some(job_id) = running {
        jobs.drop_publish = true;
        return Ok(json!({
            "ok": true,
            "requested": true,
            "state": "running",
            "job": job_id,
        }));
    }
    if let Some(id) = id
        && jobs.last.as_ref().is_none_or(|last| last.job != id)
    {
        return Err(error("content_lint_params", "unknown job"));
    }
    Ok(status_value(&jobs))
}

fn status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["id"])?;
    let id = optional_string(fields, "id")?;
    let jobs = world.get_resource::<ContentLintJobs>();
    let Some(jobs) = jobs else {
        return Ok(json!({ "ok": true, "state": "idle", "code": "content_lint_idle" }));
    };
    if let Some(id) = id.as_deref() {
        let running = jobs.active.as_ref().is_some_and(|active| active.job.id == id);
        let known = running || jobs.last.as_ref().is_some_and(|last| last.job == id);
        if !known {
            return Err(error("content_lint_params", "unknown job"));
        }
    }
    Ok(status_value(jobs))
}

fn read(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["target", "path", "severity", "validator"])?;
    let target = target_name(fields, false)?;
    let path = optional_string(fields, "path")?;
    let severity = optional_string(fields, "severity")?;
    let validator = optional_string(fields, "validator")?;
    if let Some(text) = &path {
        let root = req_path::project_root(world);
        req_path::project_path_in(&root, "path", text)?;
    }
    if severity.is_some() || validator.is_some() {
        set_problems_filter(world, severity.as_deref(), validator.as_deref())?;
    }
    let jobs = world.get_resource::<ContentLintJobs>();
    let Some(jobs) = jobs else {
        return Ok(json!({ "ok": true, "code": "content_lint_idle", "findings": [] }));
    };
    Ok(read_value(
        jobs,
        &target,
        path.as_deref(),
        severity.as_deref(),
        validator.as_deref(),
    ))
}

/// Drops a coalesced follow-up that a test does not want to run.
pub fn dismiss_pending(world: &mut World) {
    if let Some(mut jobs) = world.get_resource_mut::<ContentLintJobs>() {
        jobs.queued = false;
        jobs.pending.clear();
    }
}

fn overlay_geometry(value: Option<&Value>) -> Result<Option<GeometryOverlay>, RpcError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|err| error("content_lint_policy", err.to_string())),
    }
}

fn overlay_materials(value: Option<&Value>) -> Result<Option<MaterialOverlay>, RpcError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|err| error("content_lint_policy", err.to_string())),
    }
}

fn thresholds(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["target", "geometry", "materials", "path"])?;
    let target = target_name(fields, true)?;
    let path = optional_string(fields, "path")?;
    let geometry = overlay_geometry(fields.get("geometry"))?;
    let materials = overlay_materials(fields.get("materials"))?;
    apply_thresholds(world, &target, geometry, materials, path.as_deref())
}

fn fix_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["path", "rule_id", "location"])?;
    let path =
        optional_string(fields, "path")?.ok_or_else(|| error("content_lint_params", "path is required"))?;
    let rule = optional_string(fields, "rule_id")?;
    let location = optional_string(fields, "location")?;
    let root = req_path::project_root(world);
    req_path::project_path_in(&root, "path", &path)?;
    let jobs = world.get_resource::<ContentLintJobs>();
    let report = jobs
        .as_ref()
        .and_then(|jobs| jobs.last.as_ref().and_then(|last| last.report.as_ref()));
    let previews: Vec<Value> = report
        .map(|report| {
            report
                .findings
                .iter()
                .filter(|finding| finding.path == path)
                .filter(|finding| rule.as_ref().is_none_or(|rule| &finding.rule_id == rule))
                .filter(|finding| {
                    location
                        .as_ref()
                        .is_none_or(|location| &finding.location == location)
                })
                .map(finding_value)
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "ok": true, "path": path, "previews": previews }))
}

fn apply_fix(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = object(params)?;
    reject_unknown(fields, &["path", "rule_id", "location"])?;
    let path =
        optional_string(fields, "path")?.ok_or_else(|| error("content_lint_params", "path is required"))?;
    let rule = optional_string(fields, "rule_id")?;
    let location = optional_string(fields, "location")?;
    preview_only(world, &path, rule.as_deref(), location.as_deref())
}

/// Job methods registered beside the policy catalog.
#[must_use]
pub fn agent_methods() -> [(MethodSchema, AgentHandler); 7] {
    const START_PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "paths",
            kind: "array",
            required: false,
            description: "Project-relative .gltf or .glb paths. Omitted uses the coalesced save/import queue.",
        },
        ParamSchema {
            name: "target",
            kind: "string",
            required: false,
            description: "Profile name. Omitted means default.",
        },
    ];
    const ID_PARAMS: &[ParamSchema] = &[ParamSchema {
        name: "id",
        kind: "string",
        required: false,
        description: "Job id returned by asset.content_lint_start. Omitted means the current job.",
    }];
    const READ_PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "target",
            kind: "string",
            required: false,
            description: "Profile name. Omitted means default. Findings come from the published scan.",
        },
        ParamSchema {
            name: "path",
            kind: "string",
            required: false,
            description: "Keep findings for this project-relative path.",
        },
        ParamSchema {
            name: "severity",
            kind: "string",
            required: false,
            description: "Problems filter: error, warning, or info. The panel uses the same filter.",
        },
        ParamSchema {
            name: "validator",
            kind: "string",
            required: false,
            description: "Problems filter. Source-lint rows use source_lint.",
        },
    ];
    const THRESHOLD_PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "target",
            kind: "string",
            required: true,
            description: "Profile name, or project to write project defaults. The view then resolves default.",
        },
        ParamSchema {
            name: "geometry",
            kind: "object",
            required: false,
            description: "GeometryOverlay fields to replace. Omitted fields stay on the lower layer.",
        },
        ParamSchema {
            name: "materials",
            kind: "object",
            required: false,
            description: "MaterialOverlay fields to replace. Omitted fields stay on the lower layer.",
        },
        ParamSchema {
            name: "path",
            kind: "string",
            required: false,
            description: "Optional project-relative path queued for the next scan.",
        },
    ];
    const FIX_PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "path",
            kind: "string",
            required: true,
            description: "Project-relative source path.",
        },
        ParamSchema {
            name: "rule_id",
            kind: "string",
            required: false,
            description: "Stable rule id. Omitted covers every rule on the path.",
        },
        ParamSchema {
            name: "location",
            kind: "string",
            required: false,
            description: "Finding location. Omitted covers every location of the rule.",
        },
    ];
    [
        (
            MethodSchema {
                name: "asset.content_lint_apply_fix",
                tool: "asset_content_lint_apply_fix",
                description: "Does not write the source. Returns code content_lint_preview_only and a fix_preview link. Errors: content_lint_params, path_outside_project. Read-only sessions are denied (F0012).",
                params: FIX_PARAMS,
                result: "{ok:false, code:content_lint_preview_only, reason:content_lint_preview_only, fix_preview}",
                mutating: true,
                permission: Permission::Assets,
            },
            apply_fix,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_cancel",
                tool: "asset_content_lint_cancel",
                description: "Asks the source-lint job to stop. A later status or read uses code content_lint_cancelled and publishes nothing. Errors: content_lint_params.",
                params: ID_PARAMS,
                result: "{ok, requested, state, job}",
                mutating: false,
                permission: Permission::Read,
            },
            cancel,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_fix_preview",
                tool: "asset_content_lint_fix_preview",
                description: "Read-only fix-preview links for published findings. Does not write. Errors: content_lint_params, path_outside_project.",
                params: FIX_PARAMS,
                result: "{ok, path, previews:[{path, rule_id, location, message, severity, fix_preview}]}",
                mutating: false,
                permission: Permission::Read,
            },
            fix_preview,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_read",
                tool: "asset_content_lint_read",
                description: "Published source-lint findings and work counters. Does not block. severity and validator use the Problems panel filter. Codes: content_lint_idle, content_lint_cancelled, content_lint_stale. Errors: content_lint_params, path_outside_project.",
                params: READ_PARAMS,
                result: "{ok, findings, work?, target?, code?}",
                mutating: false,
                permission: Permission::Read,
            },
            read,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_start",
                tool: "asset_content_lint_start",
                description: "Starts one source-lint job off the editor thread. A second start while one runs coalesces onto that job (queued true). Errors: content_lint_params, content_lint_io, path_outside_project.",
                params: START_PARAMS,
                result: "{ok, state:running, job, queued, source_generation, settings_generation}",
                mutating: false,
                permission: Permission::Read,
            },
            start,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_status",
                tool: "asset_content_lint_status",
                description: "Job state without waiting. Idle is content_lint_idle. After cancel the code is content_lint_cancelled. Errors: content_lint_params.",
                params: ID_PARAMS,
                result: "{ok, state, job?, code?, findings_published?, source_generation?, settings_generation?, queued?}",
                mutating: false,
                permission: Permission::Read,
            },
            status,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_thresholds",
                tool: "asset_content_lint_thresholds",
                description: "Writes geometry and materials overlays into config/source_lint.ron. target project writes project defaults. Other names write that profile. Resolves before replace. Errors: content_lint_params, content_lint_policy, content_lint_target, content_lint_io, path_outside_project.",
                params: THRESHOLD_PARAMS,
                result: "{ok, format_version, target, path?, precedence, geometry, materials, provenance, findings, work?}",
                mutating: true,
                permission: Permission::Assets,
            },
            thresholds,
        ),
    ]
}
