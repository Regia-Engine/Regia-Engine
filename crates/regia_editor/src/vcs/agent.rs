// SPDX-License-Identifier: MPL-2.0
//! Agent surface of revision control (ADR-7837, `book/agents/revision-control.md`):
//! `editor.vcs_status`, `editor.vcs_stage`, `editor.vcs_unstage`, `editor.vcs_discard`,
//! `editor.vcs_commit`, `editor.vcs_diff`, `editor.vcs_log` and `editor.vcs_branch`. The panel's
//! controls call the same functions ([`stage`], [`commit`], ...).

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::World;
use serde_json::{Value, json};

use super::{RevisionControl as _, VcsError, VcsState};
use crate::viewport_agent::Handler;

/// Every error code the methods answer (the book page lists them).
pub const ERROR_CODES: &[&str] = &[
    "no_project",
    "no_revision_control",
    "git_not_found",
    "git_failed",
    "invalid_path",
    "nothing_staged",
    "empty_message",
    "conflicts",
    "confirm_required",
    "validation_failed",
    "validation_warnings",
    "invalid_branch",
    "unknown_branch",
    "branch_exists",
    "dirty_tree",
    "invalid_action",
];

/// History rows `editor.vcs_log` answers when `limit` is absent.
pub const DEFAULT_LOG_LIMIT: usize = 100;

const PATHS_PARAM: ParamSchema = ParamSchema {
    name: "paths",
    kind: "array",
    required: true,
    description: "Project-relative paths (`/`-separated), as editor.vcs_status lists them.",
};

/// The eight methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.vcs_status",
                tool: "vcs_status",
                description: "Revision-control status of the project (git): the branch, every changed, untracked, ignored and conflicted path with its badge state, the Staged and Changes lists, and the background poll counters. A project outside a git work tree answers state no_revision_control.",
                params: &[],
                result: "{ state, system, branch, detached, head, files: [{ path, orig_path, state, staged, unstaged }], staged: [path], unstaged: [path], changed, polls_started, polls_finished, error }",
                mutating: false,
                permission: Permission::Read,
            },
            status_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_stage",
                tool: "vcs_stage",
                description: "Stages paths (new, changed and deleted files alike): the panel's Stage.",
                params: &[PATHS_PARAM],
                result: "{ staged: [path], unstaged: [path] }",
                mutating: true,
                permission: Permission::Assets,
            },
            stage_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_unstage",
                tool: "vcs_unstage",
                description: "Takes paths out of the index; their working-tree changes stay: the panel's Unstage.",
                params: &[PATHS_PARAM],
                result: "{ staged: [path], unstaged: [path] }",
                mutating: true,
                permission: Permission::Assets,
            },
            unstage_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_discard",
                tool: "vcs_discard",
                description: "Drops the unstaged changes of paths: tracked files return to their staged (or committed) contents, untracked files are deleted. Needs confirm: true (the panel's confirmation). Staged changes stay; unstage first to drop them too.",
                params: &[
                    PATHS_PARAM,
                    ParamSchema {
                        name: "confirm",
                        kind: "boolean",
                        required: true,
                        description: "Must be true: discarding cannot be undone.",
                    },
                ],
                result: "{ discarded: [path], staged: [path], unstaged: [path] }",
                mutating: true,
                permission: Permission::Assets,
            },
            discard_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_commit",
                tool: "vcs_commit",
                description: "Validates the staged assets (the project's validators, as Validate runs them), then commits the staged changes with message. A validation error refuses the commit (validation_failed; the report goes to the Problems panel); warnings refuse it with validation_warnings unless allow_warnings is true. paths, when given, are staged first.",
                params: &[
                    ParamSchema {
                        name: "message",
                        kind: "string",
                        required: true,
                        description: "The commit message.",
                    },
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: false,
                        description: "Paths to stage before committing.",
                    },
                    ParamSchema {
                        name: "allow_warnings",
                        kind: "boolean",
                        required: false,
                        description: "Commit even when validation reports warnings (the panel's Commit anyway).",
                    },
                ],
                result: "{ commit, validated: [path], warnings: [string] }",
                mutating: true,
                permission: Permission::Assets,
            },
            commit_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_diff",
                tool: "vcs_diff",
                description: "The diff of one path: staged (index against HEAD) or unstaged (working tree against the index; an untracked file shows as all new). kind is text (unified diff), binary (sizes before and after) or unchanged.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path.",
                    },
                    ParamSchema {
                        name: "staged",
                        kind: "boolean",
                        required: false,
                        description: "Diff the staged change (default false: the unstaged one).",
                    },
                ],
                result: "{ path, staged, kind, text, old_size, new_size, truncated }",
                mutating: false,
                permission: Permission::Read,
            },
            diff_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_log",
                tool: "vcs_log",
                description: "History, newest first: of the project, or of one path following renames (the Content Browser's Show history). The panel's History tab shows the same rows.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "A project path: that file's history.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Rows at most (default 100, at most 500).",
                    },
                    ParamSchema {
                        name: "open",
                        kind: "boolean",
                        required: false,
                        description: "Also show it in the Revision Control window's History tab.",
                    },
                ],
                result: "{ path, entries: [{ id, author, email, date, summary }] }",
                mutating: false,
                permission: Permission::Read,
            },
            log_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_branch",
                tool: "vcs_branch",
                description: "Branches: list, switch (refused with dirty_tree while tracked files have changes) or create (at HEAD; switches to it unless switch is false).",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "list | switch | create.",
                    },
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "The branch (switch, create).",
                    },
                    ParamSchema {
                        name: "switch",
                        kind: "boolean",
                        required: false,
                        description: "create: switch to the new branch (default true).",
                    },
                ],
                result: "{ current, branches: [string] }",
                mutating: true,
                permission: Permission::Assets,
            },
            branch_method as Handler,
        ),
    ]
}

/// The protocol error of a revision-control failure.
#[must_use]
pub fn rpc_error(error: &VcsError) -> RpcError {
    let (code, hint) = match error.code {
        "no_project" => (UNSUPPORTED, "open a project first (editor.open_project)"),
        "no_revision_control" => (
            UNSUPPORTED,
            "the project is not in a git work tree; run git init there",
        ),
        "git_not_found" => (UNSUPPORTED, "install git, or set the preference vcs.git_command"),
        "unknown_branch" => (
            NOT_FOUND,
            "editor.vcs_branch { action: \"list\" } lists the branches",
        ),
        "invalid_path" | "invalid_branch" | "invalid_action" | "confirm_required" | "empty_message" => {
            (INVALID_PARAMS, "check the parameters against the method's schema")
        }
        "validation_failed" => (
            UNSUPPORTED,
            "fix the errors the Problems panel lists, then commit again",
        ),
        "validation_warnings" => (UNSUPPORTED, "fix the warnings, or pass allow_warnings: true"),
        "dirty_tree" => (UNSUPPORTED, "commit or discard the changes first"),
        _ => (UNSUPPORTED, "see the message"),
    };
    RpcError::new(code, error.code, error.message.clone(), hint)
}

fn paths_param(params: &Value) -> Result<Vec<String>, VcsError> {
    let items = params
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| VcsError::new("invalid_path", "`paths` (array of project paths) is required"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| VcsError::new("invalid_path", format!("{item} is not a path string")))
        })
        .collect()
}

/// The status as the protocol answers it.
#[must_use]
pub fn status_json(world: &World, state: &VcsState) -> Value {
    let counters = world.get_resource::<super::VcsStatus>();
    let (started, finished) = counters.map_or((0, 0), |c| (c.polls_started, c.polls_finished));
    match state {
        VcsState::Ready(status) => json!({
            "state": state.name(),
            "system": "git",
            "branch": status.branch,
            "detached": status.detached,
            "head": status.head,
            "files": status.files.iter().map(super::FileStatus::to_json).collect::<Vec<_>>(),
            "staged": status.staged().map(|f| f.path.clone()).collect::<Vec<_>>(),
            "unstaged": status.unstaged().map(|f| f.path.clone()).collect::<Vec<_>>(),
            "changed": status.changed_count(),
            "polls_started": started,
            "polls_finished": finished,
            "error": Value::Null,
        }),
        VcsState::Unavailable(error) => json!({
            "state": state.name(),
            "system": "git",
            "error": { "code": error.code, "message": error.message },
            "polls_started": started,
            "polls_finished": finished,
        }),
        _ => json!({
            "state": state.name(),
            "system": Value::Null,
            "error": Value::Null,
            "polls_started": started,
            "polls_finished": finished,
        }),
    }
}

fn lists(world: &mut World) -> Value {
    match super::refresh_now(world) {
        VcsState::Ready(status) => json!({
            "staged": status.staged().map(|f| f.path.clone()).collect::<Vec<_>>(),
            "unstaged": status.unstaged().map(|f| f.path.clone()).collect::<Vec<_>>(),
        }),
        _ => json!({ "staged": [], "unstaged": [] }),
    }
}

/// Stages `paths` and refreshes the badges.
///
/// # Errors
/// See [`super::RevisionControl::stage`].
pub fn stage(world: &mut World, paths: &[String]) -> Result<(), VcsError> {
    let result = super::driver(world)?.stage(paths);
    super::refresh_now(world);
    result
}

/// Unstages `paths` and refreshes the badges.
///
/// # Errors
/// See [`super::RevisionControl::unstage`].
pub fn unstage(world: &mut World, paths: &[String]) -> Result<(), VcsError> {
    let result = super::driver(world)?.unstage(paths);
    super::refresh_now(world);
    result
}

/// Discards the unstaged changes of `paths`, refreshes the badges and the content index.
///
/// # Errors
/// See [`super::RevisionControl::discard`].
pub fn discard(world: &mut World, paths: &[String]) -> Result<Vec<String>, VcsError> {
    let result = super::driver(world)?.discard(paths);
    if result.as_ref().is_ok_and(|done| !done.is_empty()) {
        crate::content_index::invalidate(world);
    }
    super::refresh_now(world);
    result
}

/// What pre-commit validation found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitCheck {
    /// Staged assets that were validated.
    pub validated: Vec<String>,
    /// `asset: validator: message` per failure.
    pub errors: Vec<String>,
    /// `asset: message` per warning.
    pub warnings: Vec<String>,
}

/// Validates the staged files that are assets of the loaded registry with the project's rules
/// (`config/validation.ron`), as the Content Browser's Validate does. The report goes to the
/// Problems panel when it found anything. Nothing is validated when no registry is loaded.
#[must_use]
pub fn check_staged(world: &mut World, staged: &[String]) -> CommitCheck {
    use regia_asset_pipeline::validators::{
        Outcome, RunSettings, Scope, Severity, Trigger, ValidatorRegistry,
    };
    let _span = tracing::info_span!("vcs_commit_validation", paths = staged.len()).entered();
    let mut check = CommitCheck::default();
    let Some(set) = crate::asset_panels::registry_set(world) else {
        return check;
    };
    let file = match crate::asset_panels::project_rules(world) {
        Ok(file) => file,
        Err(error) => {
            check.errors.push(format!("config/validation.ron: {error}"));
            return check;
        }
    };
    let registry = ValidatorRegistry::with_rules(&file);
    // The set names registry assets relative to `assets/` and some rows by project path: both.
    let mut scope: Vec<String> = staged.to_vec();
    scope.extend(
        staged
            .iter()
            .filter_map(|p| p.strip_prefix("assets/").map(str::to_string)),
    );
    let settings = RunSettings {
        scope: Scope::Changed(scope),
        trigger: Trigger::Manual,
        exclude: file.exclude.clone(),
        ..RunSettings::default()
    };
    let report = registry.run_with(&set, &settings);
    let mut validated: Vec<String> = report.results.iter().map(|r| r.asset.clone()).collect();
    validated.sort();
    validated.dedup();
    check.validated = validated;
    for result in &report.results {
        if result.outcome == Outcome::Fail {
            let why = result
                .findings
                .iter()
                .find(|f| f.severity == Severity::Error)
                .map(|f| f.text.clone())
                .or_else(|| result.messages.first().cloned())
                .unwrap_or_else(|| "failed".to_string());
            check
                .errors
                .push(format!("{}: {}: {why}", result.asset, result.validator));
        }
        for finding in result.findings.iter().filter(|f| f.severity == Severity::Warning) {
            check.warnings.push(format!("{}: {}", result.asset, finding.text));
        }
    }
    if !check.errors.is_empty() || !check.warnings.is_empty() {
        world
            .get_resource_or_insert_with(crate::asset_panels::AssetPanels::default)
            .problems
            .set_report(report);
    }
    check
}

/// Validates the staged assets, then commits. Errors block (`validation_failed`); warnings block
/// (`validation_warnings`) unless `allow_warnings`.
///
/// # Errors
/// The validation codes, or see [`super::RevisionControl::commit`].
pub fn commit(
    world: &mut World,
    message: &str,
    allow_warnings: bool,
) -> Result<(String, CommitCheck), VcsError> {
    let driver = super::driver(world)?;
    if message.trim().is_empty() {
        return Err(VcsError::new("empty_message", "a commit needs a message"));
    }
    let status = driver.status()?;
    let staged: Vec<String> = status
        .staged()
        .filter(|f| f.staged != super::Change::Deleted)
        .map(|f| f.path.clone())
        .collect();
    let check = check_staged(world, &staged);
    if !check.errors.is_empty() {
        return Err(VcsError::new(
            "validation_failed",
            format!(
                "validation failed; the commit was not made (details in the Problems panel): {}",
                check.errors.join("; ")
            ),
        ));
    }
    if !check.warnings.is_empty() && !allow_warnings {
        return Err(VcsError::new(
            "validation_warnings",
            format!("validation reported warnings: {}", check.warnings.join("; ")),
        ));
    }
    let result = driver.commit(message);
    super::refresh_now(world);
    if result.is_ok() {
        // The committed files the user still locks are offered for unlock (ADR-7864).
        super::locks::note_committed(world, &staged);
    }
    result.map(|id| (id, check))
}

fn status_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let state = super::refresh_now(world);
    Ok(status_json(world, &state))
}

fn stage_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let paths = paths_param(params).map_err(|e| rpc_error(&e))?;
    stage(world, &paths).map_err(|e| rpc_error(&e))?;
    Ok(lists(world))
}

fn unstage_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let paths = paths_param(params).map_err(|e| rpc_error(&e))?;
    unstage(world, &paths).map_err(|e| rpc_error(&e))?;
    Ok(lists(world))
}

fn discard_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let paths = paths_param(params).map_err(|e| rpc_error(&e))?;
    if params.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err(rpc_error(&VcsError::new(
            "confirm_required",
            "discarding cannot be undone: pass confirm: true",
        )));
    }
    let discarded = discard(world, &paths).map_err(|e| rpc_error(&e))?;
    let mut answer = lists(world);
    answer["discarded"] = json!(discarded);
    Ok(answer)
}

fn commit_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| rpc_error(&VcsError::new("empty_message", "`message` (string) is required")))?;
    if params.get("paths").is_some() {
        let paths = paths_param(params).map_err(|e| rpc_error(&e))?;
        stage(world, &paths).map_err(|e| rpc_error(&e))?;
    }
    let allow = params
        .get("allow_warnings")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (id, check) = commit(world, message, allow).map_err(|e| rpc_error(&e))?;
    Ok(json!({
        "commit": id,
        "validated": check.validated,
        "warnings": check.warnings,
    }))
}

fn diff_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| rpc_error(&VcsError::new("invalid_path", "`path` (string) is required")))?;
    let staged = params.get("staged").and_then(Value::as_bool).unwrap_or(false);
    let diff = super::driver(world)
        .and_then(|d| d.diff(path, staged))
        .map_err(|e| rpc_error(&e))?;
    Ok(diff.to_json())
}

fn log_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params.get("path").and_then(Value::as_str).map(str::to_string);
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LOG_LIMIT, |n| {
            usize::try_from(n).unwrap_or(super::git::LOG_LIMIT_MAX)
        });
    let entries = super::driver(world)
        .and_then(|d| d.log(path.as_deref(), limit))
        .map_err(|e| rpc_error(&e))?;
    if params.get("open").and_then(Value::as_bool) == Some(true) {
        super::panel::show_history(world, path.as_deref());
    }
    Ok(json!({
        "path": path,
        "entries": entries.iter().map(super::LogEntry::to_json).collect::<Vec<_>>(),
    }))
}

/// `editor.vcs_branch` and the panel's branch controls.
///
/// # Errors
/// `invalid_action`, or see [`super::RevisionControl::switch_branch`] and
/// [`super::RevisionControl::create_branch`].
pub fn branch(
    world: &mut World,
    action: &str,
    name: Option<&str>,
    switch: bool,
) -> Result<super::Branches, VcsError> {
    let driver = super::driver(world)?;
    let need_name = || name.ok_or_else(|| VcsError::new("invalid_branch", "`name` is required"));
    let changed = match action {
        "list" => false,
        "switch" => {
            driver.switch_branch(need_name()?)?;
            true
        }
        "create" => {
            driver.create_branch(need_name()?, switch)?;
            switch
        }
        other => {
            return Err(VcsError::new(
                "invalid_action",
                format!("action {other:?}: use list, switch or create"),
            ));
        }
    };
    if changed {
        // A switch can change every file: the browser rescans, the badges refresh.
        crate::content_index::invalidate(world);
        super::refresh_now(world);
    }
    driver.branches()
}

fn branch_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("list");
    let name = params.get("name").and_then(Value::as_str);
    let switch = params.get("switch").and_then(Value::as_bool).unwrap_or(true);
    let branches = branch(world, action, name, switch).map_err(|e| rpc_error(&e))?;
    Ok(json!({ "current": branches.current, "branches": branches.list }))
}
