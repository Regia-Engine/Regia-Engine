// SPDX-License-Identifier: MPL-2.0
//! Package-hygiene policy, waivers and audit review in Project Settings (ADR-8399).
//!
//! The packaging page and `editor.package_hygiene_set` both call `commit_loaded`. The editor
//! does not link `regia_cli` and does not scan files. An audit runs `regia package --audit-dir`
//! through the editor job registry. `HygieneAuditInjector` replaces that process in tests.
//! Findings copied into Problems keep the scrubbed display path, the opaque `location_id`, the
//! rule id, the source and the offset. Raw filesystem paths are dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use regia_agent::extension::AgentHandler;
use regia_agent::jobs::{JobHandle, JobRegistry};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::validators::Severity;
use regia_ecs::prelude::*;
use serde_json::{Map, Value, json};

use crate::asset_panels::{AssetPanels, ProblemRow};
use crate::command::{EditorCommand, UndoStack};
use crate::creators::HygieneLimits;
use crate::data_panels::PanelError;
use crate::project_settings::ProjectSettingsState;

const HINT: &str = "see book/agents/package-hygiene.md";
const ADR: &str = "ADR-8399";

/// Findings copied into the Problems panel from one audit. Matches the CLI default cap.
pub const MAX_COPIED_FINDINGS: usize = 128;

/// Retained stdout from one audit process.
const MAX_AUDIT_STDOUT: usize = 1 << 20;

const BUILTIN_IDS: &[&str] = &[
    "secret_private_key",
    "secret_webhook",
    "secret_aws_access_key",
    "secret_github_token",
    "secret_api_key",
    "secret_slack_token",
    "secret_bot_token",
    "secret_email",
    "secret_key_file",
];

const RAW_KEYS: &[&str] = &[
    "raw_path",
    "local_path",
    "path_raw",
    "filesystem_path",
    "absolute_path",
];

const SET_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "rules",
        kind: "array",
        required: false,
        description: "Replacement explicit rules [{id, kind, body}]. Empty removes rules. Absent leaves them. Refused when a legacy list is also set.",
    },
    ParamSchema {
        name: "allowances",
        kind: "array",
        required: false,
        description: "Replacement allowances [{rule, path, reason}]. The editor stores definition from the named rule. Empty removes allow. Absent leaves them.",
    },
    ParamSchema {
        name: "thresholds",
        kind: "object",
        required: false,
        description: "Positive integer ceilings: the six ADR-8244 limits plus max_entries and max_findings. Only present keys are written.",
    },
    ParamSchema {
        name: "target",
        kind: "object",
        required: false,
        description: "Scan flags {scan_names?, scan_secrets?, scan_emails?} written onto the existing [package_hygiene] keys. This is not a new TOML table.",
    },
    ParamSchema {
        name: "save",
        kind: "boolean",
        required: false,
        description: "Write Regia.toml when true (default). False previews the policy and does not record undo.",
    },
];

const AUDIT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "audit_dir",
        kind: "string",
        required: true,
        description: "Project-relative directory to audit. It is not packaged and it is not a staging publisher.",
    },
    ParamSchema {
        name: "platform",
        kind: "string",
        required: false,
        description: "linux, windows or android. Default linux. Passed to the CLI; not stored in Regia.toml.",
    },
    ParamSchema {
        name: "profile",
        kind: "string",
        required: false,
        description: "Package profile name. Default dev. Passed to the CLI; not stored as a new policy key.",
    },
    ParamSchema {
        name: "confirm",
        kind: "boolean",
        required: true,
        description: "Must be true. A call without it does not start a process.",
    },
];

/// BLAKE3 of a canonical rule definition (`kind` plus body).
///
/// The byte string matches `regia_cli::package_hygiene::definition_digest`: the prefix
/// `regia.package_hygiene.definition.v1`, a newline, the kind, a NUL and the body. Built-in
/// allowances use kind `builtin` and the rule id as the body, so the secret pattern text is
/// not hashed here.
#[must_use]
pub fn definition_digest(kind: &str, body: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regia.package_hygiene.definition.v1\n");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    let hex = hasher.finalize().to_hex();
    format!("b3:{hex}")
}

/// Text fields the packaging page uses to add one rule and one allowance.
#[derive(Resource, Debug, Default, Clone)]
pub struct HygieneForm {
    /// Rule id box.
    pub rule_id: String,
    /// `string`, `pattern` or `term`. Empty is treated as `string`.
    pub rule_kind: String,
    /// Rule body box. The audit status path does not copy this text into Problems.
    pub rule_body: String,
    /// Allowance rule id box.
    pub allow_rule: String,
    /// Allowance path glob box.
    pub allow_path: String,
    /// Allowance reason box.
    pub allow_reason: String,
}

/// One injected audit. The worker sends on `started`, blocks on `gate`, then publishes
/// `report` only when this audit's generation is still current.
///
/// `started`, `gate` and `done` are `std::sync::mpsc` ends from `std::sync::mpsc::channel`.
/// The editor library does not depend on `crossbeam-channel` (that crate is a dev-dependency
/// of the integration tests).
#[derive(Debug)]
pub struct AuditScript {
    /// Signalled once the worker is inside the gate, before it can publish.
    pub started: Sender<()>,
    /// Released by the test. `start` must return before this is signalled.
    pub gate: Receiver<()>,
    /// Signalled after the worker has either published or dropped the report.
    pub done: Sender<()>,
    /// CLI-shaped JSON. Raw path keys are not copied into Problems or the status result.
    pub report: Value,
}

/// Test seam: the next `editor.package_hygiene_audit` runs [`AuditScript`] instead of `regia`.
#[derive(Resource)]
pub struct HygieneAuditInjector {
    script: Mutex<Option<AuditScript>>,
}

impl HygieneAuditInjector {
    /// Arms one audit. A later audit needs another insert.
    #[must_use]
    pub fn once(script: AuditScript) -> Self {
        Self {
            script: Mutex::new(Some(script)),
        }
    }
}

impl std::fmt::Debug for HygieneAuditInjector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HygieneAuditInjector")
    }
}

/// Schemas and handlers for the hygiene policy and its audit job.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, AgentHandler)> {
    vec![
        (
            MethodSchema {
                name: "editor.package_hygiene_get",
                tool: "package_hygiene_get",
                description: "Read [package_hygiene] rules, allowances, thresholds and scan flags. Read permission. Does not scan files.",
                params: &[],
                result: "{rules, allowances, thresholds, target, legacy, provenance}",
                mutating: false,
                permission: Permission::Read,
            },
            get,
        ),
        (
            MethodSchema {
                name: "editor.package_hygiene_set",
                tool: "package_hygiene_set",
                description: "Validate and write [package_hygiene] rules, allowances, thresholds and scan flags. The same commit the packaging page uses. save:false previews and does not write or record undo. Errors carry data.reason: hygiene_rule_id, hygiene_duplicate_rule, hygiene_rule_kind, hygiene_rule_body, hygiene_rules, hygiene_allowance, hygiene_allowance_path, hygiene_threshold, hygiene_target, hygiene_params, hygiene_no_project.",
                params: SET_PARAMS,
                result: "{saved, rules, allowances, thresholds, target, legacy, provenance}",
                mutating: true,
                permission: Permission::EditScene,
            },
            set,
        ),
        (
            MethodSchema {
                name: "editor.package_hygiene_audit",
                tool: "package_hygiene_audit",
                description: "Start regia package --audit-dir for the open project. Needs confirm:true and the build permission. Does not build a package and does not publish a staging folder. Errors: needs_confirmation, hygiene_audit_busy, hygiene_params, hygiene_target, hygiene_no_project, path_outside_project.",
                params: AUDIT_PARAMS,
                result: "{job, state, command, clean:false, receipt:{status:absent}}",
                mutating: true,
                permission: Permission::Build,
            },
            audit,
        ),
        (
            MethodSchema {
                name: "editor.package_hygiene_audit_status",
                tool: "package_hygiene_audit_status",
                description: "Audit job state. clean is true only for the current generation finished with no findings. Receipt status is present only when the CLI JSON included receipt; otherwise absent. A cancel clears a previous clean result. Findings omit raw paths.",
                params: &[],
                result: "{state, verdict, clean, job, findings, receipt, truncated}",
                mutating: false,
                permission: Permission::Read,
            },
            audit_status,
        ),
        (
            MethodSchema {
                name: "editor.package_hygiene_audit_cancel",
                tool: "package_hygiene_audit_cancel",
                description: "Cancel the running audit, drop its findings and clear clean. A late worker cannot publish after this. Error hygiene_audit_not_found when nothing is running.",
                params: &[],
                result: "{state:cancelled, clean:false, receipt:{status:absent}}",
                mutating: true,
                permission: Permission::Build,
            },
            audit_cancel,
        ),
    ]
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(code, message)
}

fn rpc(error: PanelError) -> RpcError {
    error.rpc(HINT)
}

fn manifest_path(root: &Path) -> PathBuf {
    regia_core::brand::project_manifest(root).unwrap_or_else(|| root.join(regia_core::brand::MANIFEST_FILE))
}

fn open_project(world: &World) -> Result<PathBuf, RpcError> {
    if let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
    {
        return Ok(root);
    }
    if let Some(root) = world
        .get_resource::<regia_agent::AgentSettings>()
        .and_then(|settings| settings.project_dir.clone())
    {
        return Ok(root);
    }
    Err(rpc(err("hygiene_no_project", "no project is open")))
}

fn load_state(root: &Path) -> Result<ProjectSettingsState, RpcError> {
    let mut state = ProjectSettingsState::default();
    state
        .load(root)
        .map_err(|message| rpc(err("hygiene_no_project", message)))?;
    Ok(state)
}

/// Structural checks for a manifest that Project Settings is about to write.
///
/// A legacy list without explicit rules is accepted. A table that sets both is refused.
/// Absent ceilings are not compared with defaults here, so an older file that sets only
/// one ceiling still saves.
///
/// # Errors
///
/// A message naming the hygiene problem. Empty when the section is absent or not a table
/// this function is willing to judge (a non-table is left to the existing settings path).
pub fn validate_saved_table(root: &toml::Table) -> Result<(), String> {
    let Some(section) = root.get("package_hygiene").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    structural(section, false).map_err(|error| error.message)
}

/// Apply `patch` to a loaded manifest and, when `save` is set, write it and record undo.
///
/// The packaging page and [`commit`] both use this. Allowance `definition` values are filled
/// from the named rule before a successful save. A failed validation leaves `state` and the
/// file as they were. `save: false` returns the preview and reloads `state` from disk.
///
/// # Errors
///
/// `data.reason` is the hygiene code. Nothing is written in that case.
pub fn commit_loaded(
    world: &mut World,
    root: &Path,
    state: &mut ProjectSettingsState,
    patch: &Value,
    save: bool,
) -> Result<Value, RpcError> {
    let path = manifest_path(root);
    let before = std::fs::read_to_string(&path).map_err(|error| {
        let shown = path.display().to_string();
        rpc(err("hygiene_no_project", format!("cannot read {shown}: {error}")))
    })?;
    let Some(mut edited) = state.manifest().cloned() else {
        return Err(rpc(err("hygiene_no_project", "no Regia.toml is loaded")));
    };
    apply_patch(&mut edited, patch)?;
    bind_allowance_definitions(&mut edited)?;
    if !save {
        let mut view = view_from_table(&edited);
        view["saved"] = json!(false);
        return Ok(view);
    }
    let Some(table) = state.manifest_mut() else {
        return Err(rpc(err("hygiene_no_project", "no Regia.toml is loaded")));
    };
    *table = edited;
    state.mark_dirty();
    state
        .save(root)
        .map_err(|message| rpc(err("hygiene_params", message)))?;
    let after = std::fs::read_to_string(&path).map_err(|error| {
        let shown = path.display().to_string();
        rpc(err(
            "hygiene_params",
            format!("cannot read saved {shown}: {error}"),
        ))
    })?;
    if before != after {
        let mut stack = world.remove_resource::<UndoStack>().unwrap_or_default();
        stack.push(Box::new(HygienePolicyEdit { path, before, after }));
        world.insert_resource(stack);
    }
    let mut view = state.manifest().map(view_from_table).unwrap_or_else(|| json!({}));
    view["saved"] = json!(true);
    Ok(view)
}

/// Load `Regia.toml`, then [`commit_loaded`].
///
/// # Errors
///
/// The same codes as [`commit_loaded`], plus `hygiene_no_project` when the manifest is missing.
pub fn commit(world: &mut World, root: &Path, patch: &Value, save: bool) -> Result<Value, RpcError> {
    let mut state = load_state(root)?;
    commit_loaded(world, root, &mut state, patch, save)
}

fn apply_patch(table: &mut toml::Table, patch: &Value) -> Result<(), RpcError> {
    let mut next = table.clone();
    mutate(&mut next, patch)?;
    if let Some(section) = next.get("package_hygiene").and_then(toml::Value::as_table) {
        structural(section, true).map_err(rpc)?;
    }
    *table = next;
    Ok(())
}

fn mutate(table: &mut toml::Table, patch: &Value) -> Result<(), RpcError> {
    let object = patch
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_params", "the policy patch must be an object")))?;
    for key in object.keys() {
        if key == "save" {
            continue;
        }
        if !matches!(key.as_str(), "rules" | "allowances" | "thresholds" | "target") {
            return Err(rpc(err(
                "hygiene_params",
                format!("`{key}` is not a hygiene policy field"),
            )));
        }
    }
    if !object.keys().any(|key| key != "save") {
        return Ok(());
    }
    let section = ensure_section(table)?;
    if let Some(rules) = object.get("rules") {
        write_rules(section, rules)?;
    }
    if let Some(allowances) = object.get("allowances") {
        write_allowances(section, allowances)?;
    }
    if let Some(thresholds) = object.get("thresholds") {
        write_thresholds(section, thresholds)?;
    }
    if let Some(target) = object.get("target") {
        write_target(section, target)?;
    }
    Ok(())
}

fn ensure_section(table: &mut toml::Table) -> Result<&mut toml::Table, RpcError> {
    match table.get("package_hygiene") {
        Some(value) if !value.is_table() => {
            return Err(rpc(err("hygiene_params", "`[package_hygiene]` is not a table")));
        }
        None => {
            table.insert(
                "package_hygiene".to_owned(),
                toml::Value::Table(toml::Table::new()),
            );
        }
        Some(_) => {}
    }
    table
        .get_mut("package_hygiene")
        .and_then(toml::Value::as_table_mut)
        .map_or_else(
            || Err(rpc(err("hygiene_params", "`[package_hygiene]` is not a table"))),
            Ok,
        )
}

fn write_rules(section: &mut toml::Table, rules: &Value) -> Result<(), RpcError> {
    let rows = rules
        .as_array()
        .ok_or_else(|| rpc(err("hygiene_params", "`rules` must be an array")))?;
    if rows.is_empty() {
        section.remove("rules");
        return Ok(());
    }
    let mut out = Vec::new();
    for row in rows {
        let object = row
            .as_object()
            .ok_or_else(|| rpc(err("hygiene_params", "each rule must be an object")))?;
        for key in object.keys() {
            if !matches!(key.as_str(), "id" | "kind" | "body") {
                return Err(rpc(err(
                    "hygiene_params",
                    format!("`rules.{key}` is not a rule field"),
                )));
            }
        }
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| rpc(err("hygiene_rule_id", "each rule needs a string id")))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| rpc(err("hygiene_rule_kind", "each rule needs a string kind")))?;
        let body = object
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| rpc(err("hygiene_rule_body", "each rule needs a string body")))?;
        let mut table = toml::Table::new();
        table.insert("id".to_owned(), toml::Value::String(id.to_owned()));
        table.insert("kind".to_owned(), toml::Value::String(kind.to_owned()));
        table.insert("body".to_owned(), toml::Value::String(body.to_owned()));
        out.push(toml::Value::Table(table));
    }
    section.insert("rules".to_owned(), toml::Value::Array(out));
    Ok(())
}

fn write_allowances(section: &mut toml::Table, allowances: &Value) -> Result<(), RpcError> {
    let rows = allowances
        .as_array()
        .ok_or_else(|| rpc(err("hygiene_params", "`allowances` must be an array")))?;
    if rows.is_empty() {
        section.remove("allow");
        return Ok(());
    }
    let mut out = Vec::new();
    for row in rows {
        let object = row
            .as_object()
            .ok_or_else(|| rpc(err("hygiene_params", "each allowance must be an object")))?;
        for key in object.keys() {
            if !matches!(key.as_str(), "rule" | "path" | "reason" | "definition") {
                return Err(rpc(err(
                    "hygiene_params",
                    format!("`allowances.{key}` is not an allowance field"),
                )));
            }
        }
        let rule = object
            .get("rule")
            .and_then(Value::as_str)
            .ok_or_else(|| rpc(err("hygiene_allowance", "each allowance needs a string rule")))?
            .to_owned();
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                rpc(err(
                    "hygiene_allowance_path",
                    "each allowance needs a string path",
                ))
            })?
            .to_owned();
        let reason = object
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| rpc(err("hygiene_allowance", "each allowance needs a string reason")))?
            .trim()
            .to_owned();
        let mut table = toml::Table::new();
        table.insert("rule".to_owned(), toml::Value::String(rule));
        table.insert("path".to_owned(), toml::Value::String(path));
        table.insert("reason".to_owned(), toml::Value::String(reason));
        out.push(toml::Value::Table(table));
    }
    section.insert("allow".to_owned(), toml::Value::Array(out));
    Ok(())
}

fn write_thresholds(section: &mut toml::Table, thresholds: &Value) -> Result<(), RpcError> {
    let object = thresholds
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_threshold", "`thresholds` must be an object")))?;
    for (key, value) in object {
        if !threshold_key(key) {
            return Err(rpc(err(
                "hygiene_threshold",
                format!("`{key}` is not a hygiene ceiling"),
            )));
        }
        let limit = i64::MAX;
        let number = positive_u64(value).map_err(|_| {
            rpc(err(
                "hygiene_threshold",
                format!("`{key}` must be a positive integer no larger than {limit}"),
            ))
        })?;
        let stored = i64::try_from(number).map_err(|_| {
            rpc(err(
                "hygiene_threshold",
                format!("`{key}` must be a positive integer no larger than {limit}"),
            ))
        })?;
        section.insert(key.clone(), toml::Value::Integer(stored));
    }
    Ok(())
}

fn write_target(section: &mut toml::Table, target: &Value) -> Result<(), RpcError> {
    let object = target
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_target", "`target` must be an object")))?;
    if object.is_empty() {
        return Err(rpc(err(
            "hygiene_target",
            "`target` names at least one scan flag",
        )));
    }
    for (key, value) in object {
        if !matches!(key.as_str(), "scan_names" | "scan_secrets" | "scan_emails") {
            return Err(rpc(err("hygiene_target", format!("`{key}` is not a scan flag"))));
        }
        let flag = value
            .as_bool()
            .ok_or_else(|| rpc(err("hygiene_target", format!("`{key}` must be a boolean"))))?;
        section.insert(key.clone(), toml::Value::Boolean(flag));
    }
    Ok(())
}

fn threshold_key(key: &str) -> bool {
    matches!(
        key,
        "max_memory_bytes"
            | "max_file_bytes"
            | "max_total_bytes"
            | "max_work"
            | "scan_window_bytes"
            | "max_matcher_bytes"
            | "max_entries"
            | "max_findings"
    )
}

fn positive_u64(value: &Value) -> Result<u64, ()> {
    let number = value.as_u64().ok_or(())?;
    if number == 0 || number > i64::MAX as u64 {
        return Err(());
    }
    Ok(number)
}

fn structural(section: &toml::Table, effective_limits: bool) -> Result<(), PanelError> {
    let rules = section.get("rules").and_then(toml::Value::as_array);
    let mut seen = std::collections::BTreeSet::new();
    let mut rule_count = 0usize;
    if let Some(rows) = rules {
        for row in rows {
            let table = row
                .as_table()
                .ok_or_else(|| err("hygiene_params", "each rule must be a table"))?;
            let id = table
                .get("id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| err("hygiene_rule_id", "each rule needs a string id"))?;
            let kind = table
                .get("kind")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| err("hygiene_rule_kind", "each rule needs a string kind"))?;
            let body = table
                .get("body")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| err("hygiene_rule_body", "each rule needs a string body"))?;
            if !valid_rule_id(id) {
                return Err(err(
                    "hygiene_rule_id",
                    "a rule id is an ASCII letter, then letters, digits or underscore, at most 64 characters",
                ));
            }
            if !matches!(kind, "string" | "pattern" | "term") {
                return Err(err("hygiene_rule_kind", "kind is string, pattern or term"));
            }
            if body.is_empty() || body.len() > 16_384 {
                return Err(err("hygiene_rule_body", "a rule body is 1 to 16384 bytes"));
            }
            if !seen.insert(id.to_owned()) || is_builtin_id(id) {
                return Err(err(
                    "hygiene_duplicate_rule",
                    "a rule id is repeated or copies a built-in id",
                ));
            }
            rule_count += 1;
        }
    }
    if lists_present(section) && rule_count > 0 {
        return Err(err(
            "hygiene_rules",
            "a policy sets explicit rules or the legacy lists, not both",
        ));
    }
    let allowances = section.get("allow").and_then(toml::Value::as_array);
    let allowance_count = allowances.map_or(0, Vec::len);
    if rule_count.saturating_add(allowance_count) > 1024 {
        return Err(err(
            "hygiene_rules",
            "a policy has at most 1024 rules and allowances",
        ));
    }
    if let Some(rows) = allowances {
        for row in rows {
            check_allowance(section, row)?;
        }
    }
    for (key, value) in section {
        if threshold_key(key) {
            check_stored_limit(key, value)?;
        }
    }
    if effective_limits {
        let memory = effective_limit(
            section,
            "max_memory_bytes",
            HygieneLimits::DEFAULT_MAX_MEMORY_BYTES,
        )?;
        let window = effective_limit(
            section,
            "scan_window_bytes",
            HygieneLimits::DEFAULT_SCAN_WINDOW_BYTES,
        )?;
        if window > memory {
            return Err(err(
                "hygiene_threshold",
                "scan_window_bytes must not exceed max_memory_bytes",
            ));
        }
    } else if let (Some(memory), Some(window)) =
        (section.get("max_memory_bytes"), section.get("scan_window_bytes"))
    {
        let memory = stored_u64("max_memory_bytes", memory)?;
        let window = stored_u64("scan_window_bytes", window)?;
        if window > memory {
            return Err(err(
                "hygiene_threshold",
                "scan_window_bytes must not exceed max_memory_bytes",
            ));
        }
    }
    Ok(())
}

fn check_stored_limit(key: &str, value: &toml::Value) -> Result<(), PanelError> {
    stored_u64(key, value).map(|_| ())
}

fn stored_u64(key: &str, value: &toml::Value) -> Result<u64, PanelError> {
    let Some(number) = value.as_integer() else {
        return Err(err(
            "hygiene_threshold",
            format!("`{key}` must be a positive integer"),
        ));
    };
    let number = u64::try_from(number).unwrap_or(0);
    let limit = i64::MAX;
    if number == 0 || number > u64::try_from(limit).unwrap_or(u64::MAX) {
        return Err(err(
            "hygiene_threshold",
            format!("`{key}` must be a positive integer no larger than {limit}"),
        ));
    }
    Ok(number)
}

fn effective_limit(section: &toml::Table, key: &str, default: u64) -> Result<u64, PanelError> {
    match section.get(key) {
        None => Ok(default),
        Some(value) => stored_u64(key, value),
    }
}

fn lists_present(section: &toml::Table) -> bool {
    ["forbidden_strings", "forbidden_patterns", "deny_terms"]
        .into_iter()
        .any(|key| {
            section
                .get(key)
                .and_then(toml::Value::as_array)
                .is_some_and(|rows| !rows.is_empty())
        })
}

fn check_allowance(section: &toml::Table, row: &toml::Value) -> Result<(), PanelError> {
    let table = row
        .as_table()
        .ok_or_else(|| err("hygiene_allowance", "each allowance must be a table"))?;
    let rule = table
        .get("rule")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| err("hygiene_allowance", "each allowance needs a rule id"))?;
    let path = table
        .get("path")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| err("hygiene_allowance_path", "each allowance needs a path glob"))?;
    let reason = table
        .get("reason")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| err("hygiene_allowance", "each allowance needs a reason"))?;
    if reason.trim().is_empty() {
        return Err(err("hygiene_allowance", "an allowance reason must not be blank"));
    }
    allowance_path(path)?;
    if definition_for(section, rule).is_none() {
        return Err(err(
            "hygiene_allowance",
            "an allowance names a rule that is missing or a built-in that this target disables",
        ));
    }
    Ok(())
}

fn allowance_path(path: &str) -> Result<(), PanelError> {
    if path.is_empty() || path.len() > 4096 {
        return Err(err(
            "hygiene_allowance_path",
            "an allowance path is 1 to 4096 bytes",
        ));
    }
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') || path.contains('\\') {
        return Err(err(
            "hygiene_allowance_path",
            "an allowance path is a relative glob without a drive prefix",
        ));
    }
    if path.split('/').any(|part| matches!(part, "." | "..")) {
        return Err(err(
            "hygiene_allowance_path",
            "an allowance path cannot contain . or ..",
        ));
    }
    Ok(())
}

fn definition_for(section: &toml::Table, id: &str) -> Option<String> {
    if let Some((kind, body)) = explicit_rule(section, id) {
        return Some(definition_digest(kind, body));
    }
    let explicit = section
        .get("rules")
        .and_then(toml::Value::as_array)
        .is_some_and(|rows| !rows.is_empty());
    if !explicit && let Some((kind, body)) = synthesized(section, id) {
        return Some(definition_digest(kind, body));
    }
    if builtin_enabled(section, id) {
        return Some(definition_digest("builtin", id));
    }
    None
}

fn explicit_rule<'a>(section: &'a toml::Table, id: &str) -> Option<(&'a str, &'a str)> {
    let rows = section.get("rules")?.as_array()?;
    for row in rows {
        let table = row.as_table()?;
        if table.get("id").and_then(toml::Value::as_str) == Some(id) {
            let kind = table.get("kind").and_then(toml::Value::as_str)?;
            let body = table.get("body").and_then(toml::Value::as_str)?;
            return Some((kind, body));
        }
    }
    None
}

fn synthesized<'a>(section: &'a toml::Table, id: &str) -> Option<(&'static str, &'a str)> {
    let (rest, kind, key) = if let Some(rest) = id.strip_prefix("forbidden_string_") {
        (rest, "string", "forbidden_strings")
    } else if let Some(rest) = id.strip_prefix("forbidden_pattern_") {
        (rest, "pattern", "forbidden_patterns")
    } else {
        let rest = id.strip_prefix("deny_term_")?;
        (rest, "term", "deny_terms")
    };
    let index: usize = rest.parse().ok()?;
    if index == 0 {
        return None;
    }
    let body = section.get(key)?.as_array()?.get(index - 1)?.as_str()?;
    Some((kind, body))
}

fn builtin_enabled(section: &toml::Table, id: &str) -> bool {
    if !is_builtin_id(id) {
        return false;
    }
    if id == "secret_email" {
        flag(section, "scan_emails", false)
    } else {
        flag(section, "scan_secrets", true)
    }
}

fn is_builtin_id(id: &str) -> bool {
    BUILTIN_IDS.contains(&id)
}

fn flag(section: &toml::Table, key: &str, default: bool) -> bool {
    section.get(key).and_then(toml::Value::as_bool).unwrap_or(default)
}

fn valid_rule_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && id.len() <= 64
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn view_from_table(table: &toml::Table) -> Value {
    let section = table.get("package_hygiene").and_then(toml::Value::as_table);
    let rules = section.map(rule_view).unwrap_or_default();
    let allowances = section.map(allowance_view).unwrap_or_default();
    let thresholds = json!({
        "max_memory_bytes": section.map(|s| shown_limit(s, "max_memory_bytes", HygieneLimits::DEFAULT_MAX_MEMORY_BYTES)).unwrap_or(HygieneLimits::DEFAULT_MAX_MEMORY_BYTES),
        "max_file_bytes": section.map(|s| shown_limit(s, "max_file_bytes", HygieneLimits::DEFAULT_MAX_FILE_BYTES)).unwrap_or(HygieneLimits::DEFAULT_MAX_FILE_BYTES),
        "max_total_bytes": section.map(|s| shown_limit(s, "max_total_bytes", HygieneLimits::DEFAULT_MAX_TOTAL_BYTES)).unwrap_or(HygieneLimits::DEFAULT_MAX_TOTAL_BYTES),
        "max_work": section.map(|s| shown_limit(s, "max_work", HygieneLimits::DEFAULT_MAX_WORK)).unwrap_or(HygieneLimits::DEFAULT_MAX_WORK),
        "scan_window_bytes": section.map(|s| shown_limit(s, "scan_window_bytes", HygieneLimits::DEFAULT_SCAN_WINDOW_BYTES)).unwrap_or(HygieneLimits::DEFAULT_SCAN_WINDOW_BYTES),
        "max_matcher_bytes": section.map(|s| shown_limit(s, "max_matcher_bytes", HygieneLimits::DEFAULT_MAX_MATCHER_BYTES)).unwrap_or(HygieneLimits::DEFAULT_MAX_MATCHER_BYTES),
        "max_entries": section.map(|s| shown_limit(s, "max_entries", 100_000)).unwrap_or(100_000),
        "max_findings": section.map(|s| shown_limit(s, "max_findings", 128)).unwrap_or(128),
    });
    let target = json!({
        "scan_names": section.map(|item| flag(item, "scan_names", true)).unwrap_or(true),
        "scan_secrets": section.map(|item| flag(item, "scan_secrets", true)).unwrap_or(true),
        "scan_emails": section.map(|item| flag(item, "scan_emails", false)).unwrap_or(false),
    });
    let legacy = json!({
        "forbidden_strings": section.map(|s| string_list(s, "forbidden_strings")).unwrap_or_default(),
        "forbidden_patterns": section.map(|s| string_list(s, "forbidden_patterns")).unwrap_or_default(),
        "deny_terms": section.map(|s| string_list(s, "deny_terms")).unwrap_or_default(),
    });
    json!({
        "rules": rules,
        "allowances": allowances,
        "thresholds": thresholds,
        "target": target,
        "legacy": legacy,
        "provenance": provenance(section),
    })
}

fn shown_limit(section: &toml::Table, key: &str, default: u64) -> u64 {
    section
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|number| u64::try_from(number).ok())
        .filter(|number| *number > 0)
        .unwrap_or(default)
}

fn string_list(section: &toml::Table, key: &str) -> Vec<String> {
    section
        .get(key)
        .and_then(toml::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn rule_view(section: &toml::Table) -> Vec<Value> {
    let Some(rows) = section.get("rules").and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let table = row.as_table()?;
            Some(json!({
                "id": table.get("id").and_then(toml::Value::as_str).unwrap_or(""),
                "kind": table.get("kind").and_then(toml::Value::as_str).unwrap_or(""),
                "body": table.get("body").and_then(toml::Value::as_str).unwrap_or(""),
            }))
        })
        .collect()
}

fn allowance_view(section: &toml::Table) -> Vec<Value> {
    let Some(rows) = section.get("allow").and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let table = row.as_table()?;
            let rule = table.get("rule").and_then(toml::Value::as_str).unwrap_or("");
            let path = table.get("path").and_then(toml::Value::as_str).unwrap_or("");
            let reason = table.get("reason").and_then(toml::Value::as_str).unwrap_or("");
            let definition = table
                .get("definition")
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .or_else(|| definition_for(section, rule))
                .unwrap_or_default();
            Some(json!({
                "rule": rule,
                "path": path,
                "reason": reason,
                "definition": definition,
            }))
        })
        .collect()
}

fn provenance(section: Option<&toml::Table>) -> Value {
    let mut definitions = Vec::new();
    if let Some(section) = section {
        if let Some(rows) = section.get("rules").and_then(toml::Value::as_array) {
            for row in rows {
                let Some(table) = row.as_table() else {
                    continue;
                };
                let Some(id) = table.get("id").and_then(toml::Value::as_str) else {
                    continue;
                };
                let Some(kind) = table.get("kind").and_then(toml::Value::as_str) else {
                    continue;
                };
                let Some(body) = table.get("body").and_then(toml::Value::as_str) else {
                    continue;
                };
                definitions.push(json!({
                    "id": id,
                    "kind": kind,
                    "definition": definition_digest(kind, body),
                }));
            }
        }
        if let Some(rows) = section.get("allow").and_then(toml::Value::as_array) {
            for row in rows {
                let Some(rule) = row
                    .as_table()
                    .and_then(|table| table.get("rule"))
                    .and_then(toml::Value::as_str)
                else {
                    continue;
                };
                if builtin_enabled(section, rule) {
                    definitions.push(json!({
                        "id": rule,
                        "kind": "builtin",
                        "definition": definition_digest("builtin", rule),
                    }));
                }
            }
        }
    }
    json!({
        "editor": "regia_editor",
        "adr": ADR,
        "definitions": definitions,
    })
}

fn bind_allowance_definitions(table: &mut toml::Table) -> Result<(), RpcError> {
    let Some(section) = table.get("package_hygiene").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    let Some(rows) = section.get("allow").and_then(toml::Value::as_array) else {
        return Ok(());
    };
    let mut digests = Vec::with_capacity(rows.len());
    for row in rows {
        let rule = row
            .as_table()
            .and_then(|entry| entry.get("rule"))
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let digest = definition_for(section, rule).ok_or_else(|| {
            rpc(err(
                "hygiene_allowance",
                "an allowance names a rule that is missing or a built-in that this target disables",
            ))
        })?;
        digests.push(digest);
    }
    let Some(section) = table
        .get_mut("package_hygiene")
        .and_then(toml::Value::as_table_mut)
    else {
        return Ok(());
    };
    let Some(rows) = section.get_mut("allow").and_then(toml::Value::as_array_mut) else {
        return Ok(());
    };
    for (row, digest) in rows.iter_mut().zip(digests) {
        if let Some(entry) = row.as_table_mut() {
            entry.insert("definition".to_owned(), toml::Value::String(digest));
        }
    }
    Ok(())
}

fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_params", "expected an object")))?;
    if !fields.is_empty() {
        return Err(rpc(err(
            "hygiene_params",
            "editor.package_hygiene_get takes no parameters",
        )));
    }
    let root = open_project(world)?;
    let state = load_state(&root)?;
    let Some(table) = state.manifest() else {
        return Err(rpc(err("hygiene_no_project", "no Regia.toml is loaded")));
    };
    Ok(view_from_table(table))
}

fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_params", "expected an object")))?;
    for key in fields.keys() {
        if !matches!(
            key.as_str(),
            "rules" | "allowances" | "thresholds" | "target" | "save"
        ) {
            return Err(rpc(err(
                "hygiene_params",
                format!("`{key}` is not a hygiene policy field"),
            )));
        }
    }
    let save = match fields.get("save") {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(rpc(err("hygiene_params", "`save` must be a boolean"))),
    };
    let mut patch = Map::new();
    for key in ["rules", "allowances", "thresholds", "target"] {
        if let Some(value) = fields.get(key) {
            patch.insert(key.to_owned(), value.clone());
        }
    }
    let root = open_project(world)?;
    let mut state = load_state(&root)?;
    commit_loaded(world, &root, &mut state, &Value::Object(patch), save)
}

/// Draw the policy rules, allowances and scan flags under Packaging.
///
/// Add and Apply call [`commit_loaded`] after binding allowance digests, the same write the
/// agent method performs.
pub fn policy_section(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ProjectSettingsState,
    root: &Path,
    form: &mut HygieneForm,
    status: &mut String,
) {
    if !state.is_loaded() {
        return;
    }
    ui.separator();
    ui.label("Package hygiene policy");
    let mut names = current_flag(state, "scan_names", true);
    let mut secrets = current_flag(state, "scan_secrets", true);
    let mut emails = current_flag(state, "scan_emails", false);
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= ui.checkbox(&mut names, "Scan names").changed();
        changed |= ui.checkbox(&mut secrets, "Scan secrets").changed();
        changed |= ui.checkbox(&mut emails, "Scan emails").changed();
    });
    if changed {
        *status = commit_message(
            world,
            root,
            state,
            &json!({"target": {"scan_names": names, "scan_secrets": secrets, "scan_emails": emails}}),
            "scan flags saved",
        );
    }
    ui.horizontal(|ui| {
        ui.label("Rule");
        ui.text_edit_singleline(&mut form.rule_id);
        if form.rule_kind.is_empty() {
            form.rule_kind = "string".to_owned();
        }
        egui::ComboBox::from_id_salt("hygiene-rule-kind")
            .selected_text(form.rule_kind.clone())
            .show_ui(ui, |ui| {
                for kind in ["string", "pattern", "term"] {
                    ui.selectable_value(&mut form.rule_kind, kind.to_owned(), kind);
                }
            });
        ui.text_edit_singleline(&mut form.rule_body);
        if ui.button("Add rule").clicked() {
            *status = add_rule(world, root, state, form);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Allowance");
        ui.text_edit_singleline(&mut form.allow_rule);
        ui.text_edit_singleline(&mut form.allow_path);
        ui.text_edit_singleline(&mut form.allow_reason);
        if ui.button("Add allowance").clicked() {
            *status = add_allowance(world, root, state, form);
        }
    });
    let entries = current_limit(state, "max_entries", 100_000);
    let findings = current_limit(state, "max_findings", 128);
    ui.label(format!("Entry cap {entries}, finding cap {findings}"));
}

fn current_flag(state: &ProjectSettingsState, key: &str, default: bool) -> bool {
    state
        .manifest()
        .and_then(|table| table.get("package_hygiene"))
        .and_then(toml::Value::as_table)
        .map(|section| flag(section, key, default))
        .unwrap_or(default)
}

fn current_limit(state: &ProjectSettingsState, key: &str, default: u64) -> u64 {
    state
        .manifest()
        .and_then(|table| table.get("package_hygiene"))
        .and_then(toml::Value::as_table)
        .map(|section| shown_limit(section, key, default))
        .unwrap_or(default)
}

fn add_rule(
    world: &mut World,
    root: &Path,
    state: &mut ProjectSettingsState,
    form: &mut HygieneForm,
) -> String {
    let kind = if form.rule_kind.is_empty() {
        "string"
    } else {
        form.rule_kind.as_str()
    };
    let mut rules = current_rules(state);
    rules.push(json!({
        "id": form.rule_id,
        "kind": kind,
        "body": form.rule_body,
    }));
    let message = commit_message(world, root, state, &json!({"rules": rules}), "rule saved");
    if message == "rule saved" {
        form.rule_id.clear();
        form.rule_body.clear();
    }
    message
}

fn add_allowance(
    world: &mut World,
    root: &Path,
    state: &mut ProjectSettingsState,
    form: &mut HygieneForm,
) -> String {
    let mut allowances = current_allowances(state);
    allowances.push(json!({
        "rule": form.allow_rule,
        "path": form.allow_path,
        "reason": form.allow_reason,
    }));
    let message = commit_message(
        world,
        root,
        state,
        &json!({"allowances": allowances}),
        "allowance saved",
    );
    if message == "allowance saved" {
        form.allow_rule.clear();
        form.allow_path.clear();
        form.allow_reason.clear();
    }
    message
}

fn current_rules(state: &ProjectSettingsState) -> Vec<Value> {
    state
        .manifest()
        .and_then(|table| table.get("package_hygiene"))
        .and_then(toml::Value::as_table)
        .map(rule_view)
        .unwrap_or_default()
}

fn current_allowances(state: &ProjectSettingsState) -> Vec<Value> {
    state
        .manifest()
        .and_then(|table| table.get("package_hygiene"))
        .and_then(toml::Value::as_table)
        .map(allowance_view)
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            json!({
                "rule": row["rule"],
                "path": row["path"],
                "reason": row["reason"],
            })
        })
        .collect()
}

fn commit_message(
    world: &mut World,
    root: &Path,
    state: &mut ProjectSettingsState,
    patch: &Value,
    ok: &str,
) -> String {
    match commit_loaded(world, root, state, patch, true) {
        Ok(_) => ok.to_owned(),
        Err(error) => {
            let reason = error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("hygiene_params");
            let message = &error.message;
            format!("{reason}: {message}")
        }
    }
}

struct HygienePolicyEdit {
    path: PathBuf,
    before: String,
    after: String,
}

impl EditorCommand for HygienePolicyEdit {
    fn apply(&mut self, _world: &mut World) {
        if let Err(error) = write_atomic(&self.path, &self.after) {
            tracing::warn!(target: "regia_editor", %error, "hygiene policy redo failed");
        }
    }

    fn revert(&mut self, _world: &mut World) {
        if let Err(error) = write_atomic(&self.path, &self.before) {
            tracing::warn!(target: "regia_editor", %error, "hygiene policy undo failed");
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        "Edit package hygiene policy".to_owned()
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text.as_bytes()).map_err(|error| {
        let shown = tmp.display().to_string();
        format!("cannot write {shown}: {error}")
    })?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        let shown = path.display().to_string();
        format!("cannot rename to {shown}: {error}")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Running,
    Cancelled,
    Done,
    Failed,
}

#[derive(Clone)]
struct SafeFinding {
    path: String,
    location_id: String,
    rule: String,
    source: String,
    offset: u64,
}

struct AuditState {
    generation: u64,
    phase: Phase,
    cancel: bool,
    job_id: String,
    handle: Option<JobHandle>,
    pid: Option<u32>,
    clean: bool,
    verdict: String,
    findings: Vec<SafeFinding>,
    receipt: Value,
    truncated: bool,
    command: Vec<String>,
    /// Generation whose Done status was already copied into the host event log.
    announced_generation: u64,
}

impl Default for AuditState {
    fn default() -> Self {
        Self {
            generation: 0,
            phase: Phase::Idle,
            cancel: false,
            job_id: String::new(),
            handle: None,
            pid: None,
            clean: false,
            verdict: "idle".to_owned(),
            findings: Vec::new(),
            receipt: json!({"status": "absent"}),
            truncated: false,
            command: Vec::new(),
            announced_generation: 0,
        }
    }
}

#[derive(Resource, Clone)]
struct AuditHub(Arc<Mutex<AuditState>>);

fn hub(world: &mut World) -> Arc<Mutex<AuditState>> {
    world
        .get_resource_or_insert_with(|| AuditHub(Arc::new(Mutex::new(AuditState::default()))))
        .0
        .clone()
}

fn lock(shared: &Mutex<AuditState>) -> std::sync::MutexGuard<'_, AuditState> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn audit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_params", "expected an object")))?;
    for key in fields.keys() {
        if !matches!(key.as_str(), "audit_dir" | "platform" | "profile" | "confirm") {
            return Err(rpc(err(
                "hygiene_params",
                format!("`{key}` is not an audit field"),
            )));
        }
    }
    if fields.get("confirm") != Some(&Value::Bool(true)) {
        return Err(rpc(err(
            "needs_confirmation",
            "an audit reads the project tree: repeat with confirm: true",
        )));
    }
    let audit_dir = match fields.get("audit_dir") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        Some(Value::String(_)) => {
            return Err(rpc(err("hygiene_params", "`audit_dir` is empty")));
        }
        Some(_) => return Err(rpc(err("hygiene_params", "`audit_dir` must be a string"))),
        None => return Err(rpc(err("hygiene_params", "`audit_dir` is required"))),
    };
    let platform = match fields.get("platform") {
        None => "linux".to_owned(),
        Some(Value::String(text)) if matches!(text.as_str(), "linux" | "windows" | "android") => text.clone(),
        Some(Value::String(_)) => {
            return Err(rpc(err(
                "hygiene_target",
                "platform is linux, windows or android",
            )));
        }
        Some(_) => return Err(rpc(err("hygiene_target", "platform must be a string"))),
    };
    let profile = match fields.get("profile") {
        None => "dev".to_owned(),
        Some(Value::String(text)) if profile_ok(text) => text.clone(),
        Some(Value::String(_)) => {
            return Err(rpc(err(
                "hygiene_params",
                "profile is empty or not a profile name",
            )));
        }
        Some(_) => return Err(rpc(err("hygiene_params", "profile must be a string"))),
    };
    let root = open_project(world)?;
    let resolved =
        regia_agent::req_path::project_file_in(&root, "audit_dir", &audit_dir).map_err(|error| {
            let reason = error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if reason == "path_outside_project" {
                error
            } else {
                rpc(err(
                    "hygiene_params",
                    "audit_dir must be an existing directory inside the project",
                ))
            }
        })?;
    if !resolved.is_dir() {
        return Err(rpc(err("hygiene_params", "audit_dir must be a directory")));
    }
    let shared = hub(world);
    {
        let state = lock(&shared);
        if state.phase == Phase::Running {
            return Err(rpc(err(
                "hygiene_audit_busy",
                "a package hygiene audit is already running",
            )));
        }
    }
    let program = program(world);
    let command = vec![
        program.display().to_string(),
        "package".to_owned(),
        "--platform".to_owned(),
        platform,
        "--profile".to_owned(),
        profile,
        "--audit-dir".to_owned(),
        audit_dir,
        "--project".to_owned(),
        root.display().to_string(),
        "--json".to_owned(),
    ];
    let (job_id, handle) = world
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start("hygiene_audit", "Package hygiene audit", true);
    world
        .get_resource_or_insert_with::<crate::editor_jobs::EditorJobs>(Default::default)
        .register(&job_id, handle.clone());
    let generation = {
        let mut state = lock(&shared);
        state.generation = state.generation.saturating_add(1);
        state.phase = Phase::Running;
        state.cancel = false;
        state.job_id = job_id.clone();
        state.handle = Some(handle.clone());
        state.pid = None;
        state.clean = false;
        state.verdict = "running".to_owned();
        state.findings.clear();
        state.receipt = json!({"status": "absent"});
        state.truncated = false;
        state.command = command.clone();
        state.announced_generation = 0;
        state.generation
    };
    clear_hygiene_problems(world);
    let script = world.get_resource::<HygieneAuditInjector>().and_then(|injector| {
        injector
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    });
    if let Some(script) = script {
        let shared_worker = Arc::clone(&shared);
        spawn_worker(move || injected_worker(shared_worker, generation, script, handle)).map_err(
            |error| {
                fail_spawn(&shared, &error);
                rpc(err("hygiene_params", error))
            },
        )?;
    } else {
        let shared_worker = Arc::clone(&shared);
        let worker_command = command.clone();
        spawn_worker(move || cli_worker(shared_worker, generation, worker_command, handle)).map_err(
            |error| {
                fail_spawn(&shared, &error);
                rpc(err("hygiene_params", error))
            },
        )?;
    }
    Ok(json!({
        "job": job_id,
        "state": "running",
        "verdict": "running",
        "clean": false,
        "command": command,
        "receipt": {"status": "absent"},
    }))
}

fn profile_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
}

fn program(world: &World) -> PathBuf {
    world
        .get_resource::<regia_agent::cook_package::RegiaCliProgram>()
        .map(|program| program.0.clone())
        .unwrap_or_else(crate::bake_runner::cli_program)
}

fn spawn_worker(work: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("regia-hygiene-audit".to_owned())
        .spawn(work)
        .map_err(|error| format!("cannot start the audit worker: {error}"))
}

fn fail_spawn(shared: &Mutex<AuditState>, message: &str) {
    let mut state = lock(shared);
    if state.phase == Phase::Running {
        state.phase = Phase::Failed;
        state.clean = false;
        state.verdict = "failed".to_owned();
        if let Some(handle) = &state.handle {
            handle.fail("hygiene_params", message.to_owned());
        }
    }
}

fn injected_worker(shared: Arc<Mutex<AuditState>>, generation: u64, script: AuditScript, handle: JobHandle) {
    let _ = script.started.send(());
    let _ = script.gate.recv();
    publish_if_current(&shared, generation, &script.report, &handle);
    let _ = script.done.send(());
}

fn publish_if_current(shared: &Mutex<AuditState>, generation: u64, report: &Value, handle: &JobHandle) {
    let outcome = interpret(report);
    let mut state = lock(shared);
    if state.generation != generation
        || state.cancel
        || state.phase != Phase::Running
        || handle.cancel_requested()
    {
        return;
    }
    state.phase = Phase::Done;
    state.clean = outcome.clean;
    state.verdict = outcome.verdict;
    state.findings = outcome.findings;
    state.receipt = outcome.receipt;
    state.truncated = outcome.truncated;
    handle.finish_as(
        json!({
            "verdict": state.verdict,
            "clean": state.clean,
            "receipt": state.receipt,
        }),
        false,
    );
}

struct Outcome {
    clean: bool,
    verdict: String,
    findings: Vec<SafeFinding>,
    receipt: Value,
    truncated: bool,
}

fn interpret(report: &Value) -> Outcome {
    let mut raws = Vec::new();
    collect_raws(report, &mut raws);
    let mut findings = Vec::new();
    if let Some(rows) = findings_array(report) {
        for row in rows {
            if let Some(finding) = safe_finding(row, &raws) {
                findings.push(finding);
            }
        }
    }
    let truncated = findings.len() > MAX_COPIED_FINDINGS;
    findings.truncate(MAX_COPIED_FINDINGS);
    let complete = complete_flag(report);
    let failed = report.get("error").is_some() || report.get("ok") == Some(&Value::Bool(false));
    let (verdict, clean) = if !complete || truncated {
        ("incomplete".to_owned(), false)
    } else if failed && findings.is_empty() {
        ("failed".to_owned(), false)
    } else if findings.is_empty() {
        ("stage_clean".to_owned(), true)
    } else {
        ("stage_rejected".to_owned(), false)
    };
    Outcome {
        clean,
        verdict,
        findings,
        receipt: receipt_view(report, &raws),
        truncated,
    }
}

fn findings_array(report: &Value) -> Option<&Vec<Value>> {
    report
        .pointer("/hygiene/findings")
        .or_else(|| report.pointer("/report/hygiene/findings"))
        .or_else(|| report.get("findings"))
        .and_then(Value::as_array)
}

fn complete_flag(report: &Value) -> bool {
    let value = report
        .pointer("/hygiene/complete")
        .or_else(|| report.pointer("/report/hygiene/complete"))
        .or_else(|| report.get("complete"));
    match value {
        Some(Value::Bool(complete)) => *complete,
        _ => true,
    }
}

fn receipt_view(report: &Value, raws: &[String]) -> Value {
    let receipt = report
        .get("receipt")
        .or_else(|| report.pointer("/hygiene/receipt"))
        .or_else(|| report.pointer("/report/receipt"))
        .or_else(|| report.pointer("/report/hygiene/receipt"));
    match receipt {
        Some(value) => json!({"status": "present", "receipt": scrub_value(value, raws)}),
        None => json!({"status": "absent"}),
    }
}

fn collect_raws(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if RAW_KEYS.contains(&key.as_str())
                    && let Some(text) = child.as_str()
                    && !text.is_empty()
                {
                    out.push(text.to_owned());
                }
                collect_raws(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_raws(item, out);
            }
        }
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
}

fn safe_finding(value: &Value, raws: &[String]) -> Option<SafeFinding> {
    let object = value.as_object()?;
    let path = scrub_text(
        object.get("path").and_then(Value::as_str).unwrap_or("[redacted]"),
        raws,
    );
    let location_id = scrub_text(
        object
            .get("location_id")
            .and_then(Value::as_str)
            .unwrap_or("location-0"),
        raws,
    );
    let rule = scrub_text(
        object
            .get("rule")
            .and_then(Value::as_str)
            .unwrap_or("package_hygiene"),
        raws,
    );
    let source = scrub_text(
        object.get("source").and_then(Value::as_str).unwrap_or("content"),
        raws,
    );
    let offset = object.get("offset").and_then(Value::as_u64).unwrap_or(0);
    Some(SafeFinding {
        path,
        location_id,
        rule,
        source,
        offset,
    })
}

fn scrub_text(text: &str, raws: &[String]) -> String {
    if raws.iter().any(|raw| !raw.is_empty() && text.contains(raw)) {
        "[redacted]".to_owned()
    } else {
        text.to_owned()
    }
}

fn scrub_value(value: &Value, raws: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(scrub_text(text, raws)),
        Value::Array(items) => Value::Array(items.iter().map(|item| scrub_value(item, raws)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if RAW_KEYS.contains(&key.as_str())
                    || matches!(
                        key.as_str(),
                        "body" | "pattern" | "matched" | "match" | "secret" | "matched_text" | "text"
                    )
                {
                    continue;
                }
                out.insert(key.clone(), scrub_value(child, raws));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn cli_worker(shared: Arc<Mutex<AuditState>>, generation: u64, command: Vec<String>, handle: JobHandle) {
    let Some(program) = command.first() else {
        return;
    };
    let mut process = std::process::Command::new(program);
    process
        .args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(&mut process, 0);
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut state = lock(&shared);
            if state.generation == generation && state.phase == Phase::Running {
                state.phase = Phase::Failed;
                state.clean = false;
                state.verdict = "failed".to_owned();
                handle.fail("hygiene_params", format!("cannot run the package audit: {error}"));
            }
            return;
        }
    };
    let pid = child.id();
    {
        let mut state = lock(&shared);
        if state.generation != generation || state.cancel || state.phase != Phase::Running {
            drop(state);
            kill_pid(pid);
            let _ = child.wait();
            return;
        }
        state.pid = Some(pid);
    }
    let mut output = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut buffer = [0u8; 8192];
        loop {
            if handle.cancel_requested() {
                break;
            }
            match std::io::Read::read(&mut stdout, &mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if output.len() < MAX_AUDIT_STDOUT {
                        let room = MAX_AUDIT_STDOUT - output.len();
                        output.extend_from_slice(&buffer[..count.min(room)]);
                    }
                }
                Err(_) => break,
            }
        }
    }
    let _ = child.wait();
    let current = {
        let state = lock(&shared);
        state.generation == generation
            && !state.cancel
            && state.phase == Phase::Running
            && !handle.cancel_requested()
    };
    if !current {
        return;
    }
    let report = serde_json::from_slice::<Value>(&output).unwrap_or(Value::Null);
    if report.is_null() {
        let mut state = lock(&shared);
        if state.generation == generation && state.phase == Phase::Running {
            state.phase = Phase::Failed;
            state.clean = false;
            state.verdict = "failed".to_owned();
            handle.fail("hygiene_params", "audit output was not a JSON object");
        }
        return;
    }
    publish_if_current(&shared, generation, &report, &handle);
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn audit_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_params", "expected an object")))?;
    if !fields.is_empty() {
        return Err(rpc(err(
            "hygiene_params",
            "editor.package_hygiene_audit_status takes no parameters",
        )));
    }
    let shared = hub(world);
    let snapshot = {
        let state = lock(&shared);
        status_value(&state)
    };
    if snapshot.phase == Phase::Done {
        publish_problems(world, &snapshot.findings);
        if snapshot.announced_generation != snapshot.generation
            && let Some(mut log) = world.get_resource_mut::<regia_agent::AgentEventLog>()
        {
            log.push_host(0, "hygiene_review", snapshot.value.clone());
            lock(&shared).announced_generation = snapshot.generation;
        }
    } else if snapshot.phase != Phase::Idle {
        clear_hygiene_problems(world);
    }
    Ok(snapshot.value)
}

struct StatusSnapshot {
    phase: Phase,
    generation: u64,
    announced_generation: u64,
    findings: Vec<SafeFinding>,
    value: Value,
}

fn status_value(state: &AuditState) -> StatusSnapshot {
    let state_name = match state.phase {
        Phase::Idle => "idle",
        Phase::Running => "running",
        Phase::Cancelled => "cancelled",
        Phase::Done => "done",
        Phase::Failed => "failed",
    };
    let findings: Vec<Value> = if state.phase == Phase::Done {
        state
            .findings
            .iter()
            .map(|finding| {
                json!({
                    "path": finding.path,
                    "location_id": finding.location_id,
                    "rule": finding.rule,
                    "source": finding.source,
                    "offset": finding.offset,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let clean = state.phase == Phase::Done && state.clean;
    let receipt = if state.phase == Phase::Done {
        state.receipt.clone()
    } else {
        json!({"status": "absent"})
    };
    let value = json!({
        "state": state_name,
        "verdict": if state.phase == Phase::Done { state.verdict.clone() } else { state_name.to_owned() },
        "clean": clean,
        "job": state.job_id,
        "findings": findings,
        "receipt": receipt,
        "truncated": state.phase == Phase::Done && state.truncated,
    });
    StatusSnapshot {
        phase: state.phase,
        generation: state.generation,
        announced_generation: state.announced_generation,
        findings: if state.phase == Phase::Done {
            state.findings.clone()
        } else {
            Vec::new()
        },
        value,
    }
}

fn audit_cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| rpc(err("hygiene_params", "expected an object")))?;
    if !fields.is_empty() {
        return Err(rpc(err(
            "hygiene_params",
            "editor.package_hygiene_audit_cancel takes no parameters",
        )));
    }
    let shared = hub(world);
    let pid = {
        let mut state = lock(&shared);
        if state.phase != Phase::Running {
            return Err(rpc(err(
                "hygiene_audit_not_found",
                "no package hygiene audit is running",
            )));
        }
        state.generation = state.generation.saturating_add(1);
        state.cancel = true;
        state.phase = Phase::Cancelled;
        state.clean = false;
        state.verdict = "cancelled".to_owned();
        state.findings.clear();
        state.receipt = json!({"status": "absent"});
        state.truncated = false;
        state.announced_generation = 0;
        if let Some(handle) = &state.handle {
            handle.request_cancel();
            handle.finish_as(
                json!({"verdict": "cancelled", "clean": false, "reason": "hygiene_audit_cancelled"}),
                true,
            );
        }
        state.pid.take()
    };
    if let Some(pid) = pid {
        kill_pid(pid);
    }
    clear_hygiene_problems(world);
    Ok(json!({
        "state": "cancelled",
        "verdict": "cancelled",
        "clean": false,
        "receipt": {"status": "absent"},
        "findings": [],
    }))
}

fn clear_hygiene_problems(world: &mut World) {
    let Some(mut panels) = world.get_resource_mut::<AssetPanels>() else {
        return;
    };
    let selected = panels
        .problems
        .selected_row
        .and_then(|index| panels.problems.rows.get(index))
        .is_some_and(|row| row.validator == "package_hygiene");
    panels
        .problems
        .rows
        .retain(|row| row.validator != "package_hygiene");
    if selected {
        panels.problems.selected_row = None;
    }
}

fn publish_problems(world: &mut World, findings: &[SafeFinding]) {
    let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
    panels
        .problems
        .rows
        .retain(|row| row.validator != "package_hygiene");
    let start = panels.problems.rows.len();
    for finding in findings {
        panels.problems.rows.push(ProblemRow {
            severity: Severity::Error,
            code: finding.rule.clone(),
            validator: "package_hygiene".to_owned(),
            asset: finding.location_id.clone(),
            text: {
                let source = &finding.source;
                let offset = finding.offset;
                format!("{source} offset {offset}")
            },
            hint: Some(finding.location_id.clone()),
        });
    }
    if findings.is_empty() {
        panels.problems.selected_row = None;
    } else {
        panels.problems.selected_row = Some(start);
    }
}
