// SPDX-License-Identifier: MPL-2.0
//! The agent-protocol surface of the asset validators (`regia_asset_pipeline::validators`, ADR-1568,
//! `book/agents/asset-validators.md`): `editor.asset_validate` runs the registered checks over an
//! asset set handed in as RON, and `editor.asset_graph` answers dependency, reverse-lookup and
//! cooked-size questions about the same set.
//!
//! Nothing is written to the disk or the world. The set comes in the request (`set`), or, with
//! `source: "registry"`, from the editor's loaded registry plus every `.ui.ron` under the open
//! project's `assets/` (those files are read; an inline `set` does not walk the disk, ADR-3147).
//! Failures carry stable lowercase codes (`asset_set_invalid`, `asset_set_version`,
//! `asset_set_duplicate_path`, `unknown_validator`, `unknown_asset`, `invalid_config`).

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::registry::LoadedAssetRegistry;
use regia_asset_pipeline::asset_audit::{
    AuditError, Combine, GraphQuery, GroupBy, Side, SizeMapQuery, audit_csv, graph_query, reference_tree,
    size_map,
};
use regia_asset_pipeline::asset_graph::{AssetKind, AssetSet, AssetSetError, DependencyGraph};
use regia_asset_pipeline::validation_rules::ValidationFile;
use regia_asset_pipeline::validators::{
    RunSettings, Scope, Trigger, ValidationConfig, ValidatorError, ValidatorRegistry,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

fn set_error(e: &AssetSetError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        e.code(),
        e.to_string(),
        "`set` is RON: (format_version: 2, roots: [..], assets: [(path: \"a.png\", kind: texture, ..)])",
    )
}

fn validator_error(e: &ValidatorError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        e.code(),
        e.to_string(),
        "editor.asset_validate { list: true } names the registered validators",
    )
}

fn audit_error(e: &AuditError) -> RpcError {
    let code = if matches!(e, AuditError::UnknownAsset(_)) {
        NOT_FOUND
    } else {
        INVALID_PARAMS
    };
    RpcError::new(
        code,
        e.code(),
        e.to_string(),
        "take paths from the set's `assets`; depth is 0..=8, width 1..=500",
    )
}

/// Adds the open project's `.ui.ron` layouts to a registry set (ADR-3147).
///
/// Called only for `source: "registry"`. Reads `assets/**/*.ui.ron` when a project is open and
/// writes nothing. An inline `set` is not passed here.
fn with_project_layouts(world: &World, set: AssetSet) -> AssetSet {
    match world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    {
        Some(root) => set.with_ui_layouts(&root.join("assets")),
        None => set,
    }
}

fn request_set(world: &World, params: &Value) -> Result<AssetSet, RpcError> {
    match params.get("source").and_then(Value::as_str) {
        None | Some("set") => set_param(params),
        Some("registry") => {
            let loaded = world.get_resource::<LoadedAssetRegistry>().ok_or_else(|| {
                RpcError::new(
                    NOT_FOUND,
                    "registry_not_loaded",
                    "no asset registry is loaded in this editor",
                    "cook the project first, or pass `set`",
                )
            })?;
            Ok(with_project_layouts(
                world,
                AssetSet::from_registry(&loaded.registry),
            ))
        }
        Some(other) => Err(RpcError::invalid_params(format!(
            "`source` is \"set\" or \"registry\", not \"{other}\""
        ))),
    }
}

fn string_list(params: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be an array of strings")))?
            .iter()
            .map(|n| {
                n.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be an array of strings")))
            })
            .collect(),
    }
}

fn u32_param(params: &Value, key: &str, default: u32) -> Result<u32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn bad_scope(message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "validate_bad_scope",
        message,
        "scope is one of {assets:[..]}, {folder:\"p\", recursive:bool}, {with_dependencies:[..]}, {changed:[..]}",
    )
}

fn scope_param(params: &Value) -> Result<Scope, RpcError> {
    let Some(scope) = params.get("scope").filter(|v| !v.is_null()) else {
        return Ok(Scope::Project);
    };
    let object = scope
        .as_object()
        .ok_or_else(|| bad_scope("`scope` must be an object"))?;
    let names = |key: &str| -> Result<Vec<String>, RpcError> {
        let list = string_list(scope, key).map_err(|e| bad_scope(e.message))?;
        if list.is_empty() {
            Err(bad_scope(format!("`scope.{key}` is empty")))
        } else {
            Ok(list)
        }
    };
    let keys: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "recursive")
        .collect();
    match keys.as_slice() {
        ["assets"] => Ok(Scope::Assets(names("assets")?)),
        ["with_dependencies"] => Ok(Scope::WithDependencies(names("with_dependencies")?)),
        ["changed"] => Ok(Scope::Changed(
            string_list(scope, "changed").map_err(|e| bad_scope(e.message))?,
        )),
        ["folder"] => {
            let path = object
                .get("folder")
                .and_then(Value::as_str)
                .filter(|f| !f.is_empty())
                .ok_or_else(|| bad_scope("`scope.folder` must be a non-empty string"))?;
            Ok(Scope::Folder {
                path: path.to_owned(),
                recursive: object.get("recursive").and_then(Value::as_bool).unwrap_or(false),
            })
        }
        [] => Err(bad_scope("`scope` is empty")),
        _ => Err(bad_scope(
            "`scope` must name exactly one of assets, folder, with_dependencies, changed",
        )),
    }
}

fn set_param(params: &Value) -> Result<AssetSet, RpcError> {
    let text = params
        .get("set")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`set` (an asset set as RON text) is required"))?;
    AssetSet::from_ron(text).map_err(|e| set_error(&e))
}

fn config_param(params: &Value) -> Result<ValidationConfig, RpcError> {
    match params.get("config") {
        None | Some(Value::Null) => Ok(ValidationConfig::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                "invalid_config",
                format!("`config` is not a validation config: {e}"),
                "keys: max_texture_dimension, texture_power_of_two, mesh_triangle_budget, max_path_len",
            )
        }),
    }
}

fn rules_param(params: &Value) -> Result<ValidationFile, RpcError> {
    let mut file = match params.get("rules") {
        None | Some(Value::Null) => ValidationFile::default(),
        Some(v) => {
            let text = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`rules` must be validation.ron text"))?;
            ValidationFile::from_ron(text).map_err(|e| {
                RpcError::new(
                    INVALID_PARAMS,
                    e.code(),
                    e.to_string(),
                    "see book/agents/asset-validators.md for the format",
                )
            })?
        }
    };
    if matches!(params.get("config"), Some(v) if !v.is_null()) {
        // The inline config overrides the file's limits.
        file.limits = config_param(params)?;
    }
    Ok(file)
}

/// `editor.asset_validate { set?, source?, validators?, config?, rules?, scope?, trigger?, max_assets?, exclude?,
/// warnings_fail?, strict?, limit?, list?, include_ron? }`.
///
/// This call does not parse glTF sources. A `.gltf` or `.glb` save queues one coalesced
/// source-lint scan (`asset.content_lint_start`) that parses off the editor thread.
fn asset_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let file = rules_param(params)?;
    let registry = ValidatorRegistry::with_rules(&file);
    if params.get("list").and_then(Value::as_bool).unwrap_or(false) {
        let rows: Vec<Value> = registry
            .describe()
            .into_iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect();
        return Ok(json!({ "validators": rows }));
    }
    let mut set = request_set(world, params)?;
    // `rules.roots` are path globs over the set (the same expansion as
    // `AssetSet::from_registry_with_roots`). Empty globs leave the set's roots, so scenes stay
    // the default. On `source: "registry"` the set already includes the project's UI layouts.
    if !file.roots.is_empty() {
        set.apply_root_globs(&file.roots);
    }
    let only = match params.get("validators") {
        None | Some(Value::Null) => None,
        Some(_) => Some(
            string_list(params, "validators")
                .map_err(|_| RpcError::invalid_params("`validators` must be an array of names"))?,
        ),
    };
    let trigger = match params.get("trigger") {
        None | Some(Value::Null) => Trigger::Agent,
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|_| RpcError::invalid_params("`trigger` is manual, save, cook, ci or agent"))?,
    };
    let mut exclude = file.exclude.clone();
    exclude.extend(string_list(params, "exclude")?);
    let settings = RunSettings {
        scope: scope_param(params)?,
        trigger,
        max_assets: match params.get("max_assets") {
            None | Some(Value::Null) => None,
            Some(_) => Some(u32_param(params, "max_assets", 0)? as usize),
        },
        exclude,
        warnings_fail: params
            .get("warnings_fail")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        only,
    };
    let strict = params.get("strict").and_then(Value::as_bool).unwrap_or(false);
    let limit = u32_param(params, "limit", 500)? as usize;
    let report = registry
        .run_checked(&set, &settings)
        .map_err(|e| validator_error(&e))?;
    let mut value = serde_json::to_value(&report).map_err(|e| RpcError::internal(e.to_string()))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("ok".into(), json!(report.is_ok()));
        object.insert("exit_failed".into(), json!(report.exit_failed(strict)));
        object.insert("rows_total".into(), json!(report.results.len()));
        if report.results.len() > limit
            && let Some(rows) = object.get_mut("results").and_then(Value::as_array_mut)
        {
            rows.truncate(limit);
        }
        if params
            .get("include_ron")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let text = report.to_ron().map_err(|e| validator_error(&e))?;
            object.insert("ron".into(), json!(text));
        }
    }
    Ok(value)
}

/// `editor.asset_graph { set, asset? }`.
fn asset_graph(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let set = request_set(world, params)?;
    let graph = DependencyGraph::build(&set);
    if params.get("assets").is_some_and(|v| !v.is_null()) {
        return graph_columns(&set, &graph, params);
    }
    match params.get("asset").and_then(Value::as_str) {
        Some(asset) => {
            // One asset: its own rollup only, not the whole set's (audit-ext-b High).
            let Some(size) = graph.size_of(asset) else {
                return Err(RpcError::new(
                    NOT_FOUND,
                    "unknown_asset",
                    format!("`{asset}` is not in the asset set"),
                    "take paths from the set's `assets`",
                ));
            };
            Ok(json!({
                "asset": asset,
                "dependencies": graph.dependencies(asset),
                "referrers": graph.referrers(asset),
                "transitive_dependencies": graph.transitive_dependencies(asset),
                "transitive_referrers": graph.transitive_referrers(asset),
                "missing_references": graph.missing_references(asset),
                "in_cycle": graph.in_cycle(asset),
                "size": size,
            }))
        }
        None => {
            let sizes = graph.size_rollup();
            let rows: Vec<Value> = sizes
                .iter()
                .map(|(path, size)| {
                    json!({
                        "asset": path,
                        "own_bytes": size.own_bytes,
                        "total_bytes": size.total_bytes,
                        "dependency_count": size.dependency_count,
                        "referrer_count": graph.referrers(path).len(),
                    })
                })
                .collect();
            Ok(json!({ "assets": rows, "count": graph.len(), "roots": set.effective_roots() }))
        }
    }
}

fn graph_columns(set: &AssetSet, graph: &DependencyGraph, params: &Value) -> Result<Value, RpcError> {
    let mut query = GraphQuery {
        roots: string_list(params, "assets")?,
        referrer_depth: u32_param(params, "referrer_depth", 1)?,
        dependency_depth: u32_param(params, "dependency_depth", 1)?,
        max_per_column: u32_param(params, "max_per_column", 50)?,
        path_glob: params.get("path").and_then(Value::as_str).map(str::to_owned),
        ..GraphQuery::default()
    };
    query.combine = match params.get("combine").and_then(Value::as_str) {
        None | Some("union") => Combine::Union,
        Some("intersect") => Combine::Intersect,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`combine` is \"union\" or \"intersect\"",
            ));
        }
    };
    for kind in string_list(params, "kinds")? {
        let k: AssetKind = serde_json::from_value(json!(kind))
            .map_err(|_| RpcError::invalid_params(format!("`{kind}` is not an asset kind")))?;
        query.kinds.push(k);
    }
    for tag in string_list(params, "tags")? {
        query.tag_filters.push(match tag.split_once('=') {
            Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
            None => (tag, None),
        });
    }
    if let Some(pages) = params.get("pages").and_then(Value::as_array) {
        for p in pages {
            let side = match p.get("side").and_then(Value::as_str) {
                Some("referrers") => Side::Referrers,
                Some("dependencies") => Side::Dependencies,
                _ => {
                    return Err(RpcError::invalid_params(
                        "`pages[].side` is \"referrers\" or \"dependencies\"",
                    ));
                }
            };
            let num = |k: &str| {
                p.get(k)
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("`pages[].{k}` must be a non-negative integer"))
                    })
            };
            query.pages.push((side, num("hop")?, num("page")?));
        }
    }
    let result = graph_query(set, graph, &query).map_err(|e| audit_error(&e))?;
    serde_json::to_value(&result).map_err(|e| RpcError::internal(e.to_string()))
}

/// `editor.asset_tree { set?, source?, asset, side, max_depth?, max_nodes?, include_sub_assets? }`.
fn asset_tree(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let set = request_set(world, params)?;
    let graph = DependencyGraph::build(&set);
    let asset = params
        .get("asset")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`asset` is required"))?;
    let side = match params.get("side").and_then(Value::as_str) {
        Some("referrers") => Side::Referrers,
        Some("dependencies") => Side::Dependencies,
        _ => {
            return Err(RpcError::invalid_params(
                "`side` is \"referrers\" or \"dependencies\"",
            ));
        }
    };
    let include = params
        .get("include_sub_assets")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let tree = reference_tree(
        &set,
        &graph,
        asset,
        side,
        u32_param(params, "max_depth", 8)?,
        u32_param(params, "max_nodes", 2_000)? as usize,
        include,
    )
    .map_err(|e| audit_error(&e))?;
    serde_json::to_value(&tree).map_err(|e| RpcError::internal(e.to_string()))
}

/// `editor.asset_size_map { set?, source?, assets, group_by?, levels?, tag?, top? }`.
fn asset_size_map(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let set = request_set(world, params)?;
    let graph = DependencyGraph::build(&set);
    let group_by = match params.get("group_by").and_then(Value::as_str) {
        None | Some("asset") => GroupBy::Asset,
        Some("kind") => GroupBy::Kind,
        Some("folder") => GroupBy::Folder(u32_param(params, "levels", 1)?.max(1)),
        Some("tag") => GroupBy::Tag(
            params
                .get("tag")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| audit_error(&AuditError::BadGroup("tag (give `tag`)".into())))?
                .to_owned(),
        ),
        Some(other) => return Err(audit_error(&AuditError::BadGroup(other.to_owned()))),
    };
    let query = SizeMapQuery {
        roots: string_list(params, "assets")?,
        group_by,
        top: u32_param(params, "top", 200)?,
    };
    let map = size_map(&set, &graph, &query).map_err(|e| audit_error(&e))?;
    serde_json::to_value(&map).map_err(|e| RpcError::internal(e.to_string()))
}

/// `editor.asset_audit { set?, source?, assets?, folder? }`.
fn asset_audit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let set = request_set(world, params)?;
    let graph = DependencyGraph::build(&set);
    let mut selection = string_list(params, "assets")?;
    if let Some(folder) = params.get("folder").and_then(Value::as_str) {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        selection.extend(
            set.assets
                .iter()
                .filter(|a| a.path.starts_with(&prefix))
                .map(|a| a.path.clone()),
        );
        if selection.is_empty() {
            return Ok(json!({ "csv": audit_csv(&AssetSet::new(), &graph, &[]), "rows": 0 }));
        }
    }
    for s in &selection {
        if set.get(s).is_none() && !set.assets.iter().any(|a| a.id.as_deref() == Some(s.as_str())) {
            return Err(audit_error(&AuditError::UnknownAsset(s.clone())));
        }
    }
    let csv = audit_csv(&set, &graph, &selection);
    let rows = csv.lines().count().saturating_sub(1);
    Ok(json!({ "csv": csv, "rows": rows }))
}

const P_SOURCE: ParamSchema = ParamSchema {
    name: "source",
    kind: "string",
    required: false,
    description: "\"set\" (default: use `set`) or \"registry\" (the editor's loaded asset registry, plus every .ui.ron under the open project's assets/; registry_not_loaded when no registry is loaded). An inline set is not merged with those files.",
};

/// `editor.save_validation { enabled?, hold?, wait? }`: the save trigger (spec 32 § 4.5, ADR-2755).
fn save_validation(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::save_validation::{SaveValidation, finish_save_validation, tick_save_validation};
    let bool_param = |key: &str| -> Result<Option<bool>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
        }
    };
    let (enabled, hold, wait) = (bool_param("enabled")?, bool_param("hold")?, bool_param("wait")?);
    if world.get_resource::<SaveValidation>().is_none() {
        let on = crate::asset_panels::project_rules(world).map_or(true, |file| file.validate_on_save);
        world.insert_resource(SaveValidation::new(on));
    }
    if let Some(mut state) = world.get_resource_mut::<SaveValidation>() {
        if let Some(on) = enabled {
            state.gate.set_enabled(on);
        }
        if let Some(on) = hold {
            state.set_hold(on);
        }
    }
    if wait == Some(true) {
        finish_save_validation(world);
    } else {
        tick_save_validation(world);
    }
    let panels = world.get_resource::<crate::asset_panels::AssetPanels>();
    let state = world
        .get_resource::<SaveValidation>()
        .ok_or_else(|| RpcError::internal("save validation state vanished"))?;
    Ok(json!({
        "enabled": state.gate.enabled(),
        "held": state.held(),
        "holds": state.gate.holds(),
        "pending": state.gate.pending(),
        "running": state.running(),
        "runs": state.runs,
        "last_batch": state.last_batch,
        "last_error": state.last_error,
        "summary": panels.and_then(|p| p.problems.report.as_ref()).map(|r| &r.summary),
        "rows_total": panels.map_or(0, |p| p.problems.rows.len()),
    }))
}

const P_SET: ParamSchema = ParamSchema {
    name: "set",
    kind: "string",
    required: false,
    description: "The asset set as RON: (format_version: 2, roots: [..], assets: [(path, kind, id, references, cooked_bytes, texture_size, triangles)]). A v1 set still loads (upgraded in memory to the current version). Required unless `list` is true or `source` is \"registry\".",
};

/// The asset-validator methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.save_validation",
                tool: "save_validation",
                description: "The validators' save trigger. Every level save (editor or agent) queues its path; one run per tick validates the batch (trigger save, scope changed, plus the dialogue_assets checks) and fills the Problems panel (read editor.problems_panel). Saving is never blocked: results arrive on a later tick. A .gltf or .glb save also queues one coalesced source-lint scan (asset.content_lint_start) that parses off the editor thread. `hold` defers runs while a batch tool saves many files (they run once after `hold: false`); `wait` blocks until the run it starts has finished.",
                params: &[
                    ParamSchema {
                        name: "enabled",
                        kind: "boolean",
                        required: false,
                        description: "Turn the trigger on or off (default: validate_on_save of config/validation.ron). Turning it off drops the queue.",
                    },
                    ParamSchema {
                        name: "hold",
                        kind: "boolean",
                        required: false,
                        description: "true: defer runs; false: release the hold.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "Block until the run started by this call (if any) has finished.",
                    },
                ],
                result: "{ enabled, held, holds, pending, running, runs, last_batch, last_error, summary, rows_total }",
                mutating: true,
                permission: Permission::Assets,
            },
            save_validation as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_validate",
                tool: "asset_validate",
                description: "Run the asset validators (missing references, naming rules, texture size limits, duplicate ids, orphaned assets, mesh triangle budget) over an asset set given as RON, or over the loaded registry when source is \"registry\". Each row is pass, fail or not_validated with messages. An inline set reads nothing from disk. source \"registry\" also reads every .ui.ron under the open project's assets/.",
                params: &[
                    P_SET,
                    ParamSchema {
                        name: "validators",
                        kind: "array",
                        required: false,
                        description: "Names to run (default: all).",
                    },
                    ParamSchema {
                        name: "config",
                        kind: "object",
                        required: false,
                        description: "Limits: max_texture_dimension (4096), texture_power_of_two (false), mesh_triangle_budget (200000), max_path_len (200).",
                    },
                    ParamSchema {
                        name: "list",
                        kind: "boolean",
                        required: false,
                        description: "List the validators instead of running them.",
                    },
                    ParamSchema {
                        name: "include_ron",
                        kind: "boolean",
                        required: false,
                        description: "Also return the report as RON text in `ron`.",
                    },
                    P_SOURCE,
                    ParamSchema {
                        name: "rules",
                        kind: "string",
                        required: false,
                        description: "config/validation.ron text: rules (PathStyle, SizeBudget, Forbid, MaxDependencies, NoCycles), exclude, roots and limits. Errors are invalid_config with the line.",
                    },
                    ParamSchema {
                        name: "scope",
                        kind: "object",
                        required: false,
                        description: "What to check: {assets:[..]}, {folder, recursive}, {with_dependencies:[..]} or {changed:[..]}; default the whole set. Conflicting or empty scopes are validate_bad_scope.",
                    },
                    ParamSchema {
                        name: "trigger",
                        kind: "string",
                        required: false,
                        description: "manual, save, cook, ci or agent (default agent); labels the run.",
                    },
                    ParamSchema {
                        name: "max_assets",
                        kind: "integer",
                        required: false,
                        description: "Stop gathering at this many assets (sets summary.cap_reached).",
                    },
                    ParamSchema {
                        name: "exclude",
                        kind: "array",
                        required: false,
                        description: "Path globs left out of gathered (not directly named) assets.",
                    },
                    ParamSchema {
                        name: "warnings_fail",
                        kind: "boolean",
                        required: false,
                        description: "A warning finding also fails the asset.",
                    },
                    ParamSchema {
                        name: "strict",
                        kind: "boolean",
                        required: false,
                        description: "exit_failed is also true when any asset has a warning.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Most rows returned (default 500); rows_total has the full count.",
                    },
                ],
                result: "{ ok, exit_failed, rows_total, format_version, summary: { passed, failed, not_validated, warnings, requested, checked, skipped_excluded, cap_reached }, results: [{ validator, asset, outcome, messages, findings: [{ code, severity, text, hint? }] }], ron? } or { validators: [{ name, description }] }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_validate as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_graph",
                tool: "asset_graph",
                description: "Dependency graph of an asset set given as RON: for one asset its references, referrers (reverse lookup), transitive closure both ways, missing references and cooked-size roll-up; without `asset` the size map of every asset.",
                params: &[
                    ParamSchema {
                        name: "set",
                        kind: "string",
                        required: false,
                        description: "The asset set as RON (see editor.asset_validate). Required unless `source` is \"registry\".",
                    },
                    P_SOURCE,
                    ParamSchema {
                        name: "asset",
                        kind: "string",
                        required: false,
                        description: "Path of one asset to describe.",
                    },
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: false,
                        description: "Root paths or ids (1..=64): answers the column view instead of the single-asset form.",
                    },
                    ParamSchema {
                        name: "combine",
                        kind: "string",
                        required: false,
                        description: "With several roots: \"union\" (default) or \"intersect\" (only nodes every root reaches).",
                    },
                    ParamSchema {
                        name: "referrer_depth",
                        kind: "integer",
                        required: false,
                        description: "Hops of referrers, 0..=8 (default 1); more is graph_bad_depth.",
                    },
                    ParamSchema {
                        name: "dependency_depth",
                        kind: "integer",
                        required: false,
                        description: "Hops of dependencies, 0..=8 (default 1).",
                    },
                    ParamSchema {
                        name: "max_per_column",
                        kind: "integer",
                        required: false,
                        description: "Nodes listed per column, 1..=500 (default 50); the rest is `hidden`.",
                    },
                    ParamSchema {
                        name: "kinds",
                        kind: "array",
                        required: false,
                        description: "Only these kinds (mesh, texture, material, scene, audio, other); roots are never filtered.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Glob restricting every node but the roots.",
                    },
                    ParamSchema {
                        name: "tags",
                        kind: "array",
                        required: false,
                        description: "Tag filters, \"key\" or \"key=value\"; all must hold.",
                    },
                    ParamSchema {
                        name: "pages",
                        kind: "array",
                        required: false,
                        description: "Chunk per column: [{side, hop, page}] (0-based page).",
                    },
                ],
                result: "{ asset, dependencies, referrers, transitive_dependencies, transitive_referrers, missing_references, in_cycle, size: { own_bytes, total_bytes, dependency_count } } or { assets: [{ asset, own_bytes, total_bytes, dependency_count, referrer_count }], count, roots } or, with `assets`, { roots, columns: [{ side, hop, nodes, hidden, total, page, pages }], edges: [{ from, to, kind }], types_present, truncated, missing }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_graph as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_tree",
                tool: "asset_tree",
                description: "Indented reference tree of one asset on one side (`dep <path>` per line; `(cycle)` marks a node already on the chain; `(truncated)` ends a cut tree).",
                params: &[
                    P_SET,
                    P_SOURCE,
                    ParamSchema {
                        name: "asset",
                        kind: "string",
                        required: true,
                        description: "Path or id of the root.",
                    },
                    ParamSchema {
                        name: "side",
                        kind: "string",
                        required: true,
                        description: "\"dependencies\" or \"referrers\".",
                    },
                    ParamSchema {
                        name: "max_depth",
                        kind: "integer",
                        required: false,
                        description: "Levels below the root (default 8, at most 64).",
                    },
                    ParamSchema {
                        name: "max_nodes",
                        kind: "integer",
                        required: false,
                        description: "Lines before the tree is cut (default 2000).",
                    },
                    ParamSchema {
                        name: "include_sub_assets",
                        kind: "boolean",
                        required: false,
                        description: "List `file#fragment` sub-assets (default true).",
                    },
                ],
                result: "{ text, nodes, truncated }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_tree as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_size_map",
                tool: "asset_size_map",
                description: "Footprint of the closure (the assets plus everything they pull in): total bytes with shared assets counted once, exclusive bytes (what deleting the roots frees), shared bytes, and rows grouped by asset, kind, folder or tag.",
                params: &[
                    P_SET,
                    P_SOURCE,
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: true,
                        description: "Root paths or ids.",
                    },
                    ParamSchema {
                        name: "group_by",
                        kind: "string",
                        required: false,
                        description: "asset (default), kind, folder or tag; anything else is size_map_bad_group.",
                    },
                    ParamSchema {
                        name: "levels",
                        kind: "integer",
                        required: false,
                        description: "With group_by folder: path segments to group by (default 1).",
                    },
                    ParamSchema {
                        name: "tag",
                        kind: "string",
                        required: false,
                        description: "With group_by tag: the tag key.",
                    },
                    ParamSchema {
                        name: "top",
                        kind: "integer",
                        required: false,
                        description: "With group_by asset: rows kept before the rest folds into `(other)` (default 200).",
                    },
                ],
                result: "{ closure_assets, total_bytes, roots_bytes, shared_bytes, exclusive_bytes, rows: [{ name, assets, bytes, exclusive_bytes, shared_bytes }], missing }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_size_map as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_audit",
                tool: "asset_audit",
                description: "The audit table as CSV text (RFC 4180): path, kind, size, dependency_count, referrer_count, closure_assets, closure_bytes, exclusive_bytes, tags. The CLI writes the file; this returns the text.",
                params: &[
                    P_SET,
                    P_SOURCE,
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: false,
                        description: "Only these paths or ids (default all).",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "Only assets under this folder.",
                    },
                ],
                result: "{ csv, rows }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_audit as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = r##"#![enable(implicit_some)]
    (
        assets: [
            (path: "scenes/main.scene", kind: scene, references: ["meshes/rock.mesh", "textures/gone.png"], cooked_bytes: 10),
            (path: "meshes/rock.mesh", kind: mesh, references: ["textures/rock.png"], cooked_bytes: 100, triangles: 500000),
            (path: "textures/rock.png", kind: texture, cooked_bytes: 1000, texture_size: (8192, 8192)),
            (path: "textures/loose.png", kind: texture, cooked_bytes: 5),
        ],
    )"##;

    fn call(method: &str, params: &Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == method)
            .expect("method registered");
        handler(&mut world, params)
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn validate_reports_every_builtin() {
        let out = call("editor.asset_validate", &json!({ "set": SET })).expect("runs");
        assert_eq!(out["ok"], false);
        assert_eq!(out["summary"]["failed"], 4);
        let failing: Vec<(String, String)> = out["results"]
            .as_array()
            .expect("rows")
            .iter()
            .filter(|r| r["outcome"] == "fail")
            .map(|r| {
                (
                    r["validator"].as_str().unwrap_or_default().to_owned(),
                    r["asset"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        assert!(failing.contains(&("missing_references".into(), "scenes/main.scene".into())));
        assert!(failing.contains(&("mesh_triangle_budget".into(), "meshes/rock.mesh".into())));
        assert!(failing.contains(&("texture_size_limits".into(), "textures/rock.png".into())));
        assert!(failing.contains(&("orphaned_assets".into(), "textures/loose.png".into())));
    }

    /// `rules.roots` replaces the scene roots: a glob that names `audio/wind.wav` makes that file
    /// the root, so a texture only the scene references is orphaned and the named file is not.
    #[test]
    fn rules_roots_replace_the_scene_roots() {
        let set = r##"#![enable(implicit_some)]
        (
            assets: [
                (path: "scenes/main.scene", kind: scene, references: ["a.png"]),
                (path: "a.png", kind: texture),
                (path: "audio/wind.wav", kind: audio),
                (path: "orphan.png", kind: texture),
            ],
        )"##;
        let out = call(
            "editor.asset_validate",
            &json!({
                "set": set,
                "rules": "(roots: [\"audio/wind.wav\"])",
                "validators": ["orphaned_assets"],
            }),
        )
        .expect("runs");
        let failing: Vec<String> = out["results"]
            .as_array()
            .expect("rows")
            .iter()
            .filter(|row| row["validator"] == "orphaned_assets" && row["outcome"] == "fail")
            .map(|row| row["asset"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            !failing.iter().any(|asset| asset == "audio/wind.wav"),
            "the named root is reachable: {failing:?}"
        );
        assert!(
            failing.iter().any(|asset| asset == "a.png"),
            "a.png is only referenced by a scene that is not a root: {failing:?}"
        );
    }

    #[test]
    fn validate_select_config_list_and_ron() {
        let only = call(
            "editor.asset_validate",
            &json!({ "set": SET, "validators": ["mesh_triangle_budget"], "config": { "mesh_triangle_budget": 1_000_000 }, "include_ron": true }),
        )
        .expect("runs");
        assert_eq!(only["summary"]["failed"], 0);
        assert!(only["ron"].as_str().expect("ron text").contains("format_version"));
        let list = call("editor.asset_validate", &json!({ "list": true })).expect("lists");
        let names: Vec<_> = list["validators"]
            .as_array()
            .expect("validator rows")
            .iter()
            .map(|row| row["name"].as_str().expect("validator name"))
            .collect();
        assert_eq!(
            names,
            [
                "missing_references",
                "naming_rules",
                "texture_size_limits",
                "duplicate_ids",
                "orphaned_assets",
                "mesh_triangle_budget",
                "path_style",
                "size_budget",
                "dependency_rules",
                "localized_twin",
                "ui_image_missing",
            ]
        );
    }

    /// A posted v1 set loads (upgraded in memory, ADR-5632) and validates. The answer's
    /// `format_version` is the *report's* version (`VALIDATION_REPORT_FORMAT_VERSION`), not an
    /// echo of the set's version: review sprint-rev-spec-6-k finding 2 expected the upgraded set
    /// version (2); the pinned value records what the protocol actually answers, so the report
    /// version and the set version can never be confused again.
    #[test]
    fn a_v1_set_loads_and_the_answer_keeps_the_report_version() {
        use regia_asset_pipeline::validators::VALIDATION_REPORT_FORMAT_VERSION;
        let v1 = r##"#![enable(implicit_some)]
        (
            format_version: 1,
            assets: [
                (path: "scenes/main.scene", kind: scene, references: ["a.png"], cooked_bytes: 1),
                (path: "a.png", kind: texture, cooked_bytes: 5),
            ],
        )"##;
        let out = call("editor.asset_validate", &json!({ "set": v1 })).expect("a v1 set loads");
        assert_eq!(
            out["format_version"], VALIDATION_REPORT_FORMAT_VERSION,
            "the answer's format_version is the report's version, independent of the set's"
        );
    }

    #[test]
    fn validate_error_codes() {
        let e = call("editor.asset_validate", &json!({})).expect_err("no set");
        assert_eq!(code(&e), "F0002");
        let e = call("editor.asset_validate", &json!({ "set": "(assets: [" })).expect_err("bad ron");
        assert_eq!(code(&e), "asset_set_invalid");
        let e = call("editor.asset_validate", &json!({ "set": "(format_version: 9)" })).expect_err("newer");
        assert_eq!(code(&e), "asset_set_version");
        let e = call(
            "editor.asset_validate",
            &json!({ "set": "(assets: [(path: \"a\"), (path: \"a\")])" }),
        )
        .expect_err("dup");
        assert_eq!(code(&e), "asset_set_duplicate_path");
        let e = call(
            "editor.asset_validate",
            &json!({ "set": SET, "validators": ["nope"] }),
        )
        .expect_err("unknown");
        assert_eq!(code(&e), "unknown_validator");
        let e = call("editor.asset_validate", &json!({ "set": SET, "validators": [3] })).expect_err("type");
        assert_eq!(code(&e), "F0002");
        let e = call(
            "editor.asset_validate",
            &json!({ "set": SET, "config": { "max_path_len": "x" } }),
        )
        .expect_err("config");
        assert_eq!(code(&e), "invalid_config");
    }

    #[test]
    fn graph_one_asset() {
        let out = call(
            "editor.asset_graph",
            &json!({ "set": SET, "asset": "textures/rock.png" }),
        )
        .expect("runs");
        assert_eq!(out["referrers"], json!(["meshes/rock.mesh"]));
        assert_eq!(
            out["transitive_referrers"],
            json!(["meshes/rock.mesh", "scenes/main.scene"])
        );
        assert_eq!(out["size"]["total_bytes"], 1000);
        let scene = call(
            "editor.asset_graph",
            &json!({ "set": SET, "asset": "scenes/main.scene" }),
        )
        .expect("runs");
        assert_eq!(scene["missing_references"], json!(["textures/gone.png"]));
        assert_eq!(scene["size"]["total_bytes"], 1110);
    }

    #[test]
    fn graph_size_map_and_errors() {
        let out = call("editor.asset_graph", &json!({ "set": SET })).expect("runs");
        assert_eq!(out["count"], 4);
        assert_eq!(out["roots"], json!(["scenes/main.scene"]));
        assert_eq!(out["assets"][0]["asset"], "meshes/rock.mesh");
        let e = call("editor.asset_graph", &json!({ "set": SET, "asset": "nope" })).expect_err("unknown");
        assert_eq!(code(&e), "unknown_asset");
        let e = call("editor.asset_graph", &json!({})).expect_err("no set");
        assert_eq!(code(&e), "F0002");
    }

    const SET2: &str = r##"#![enable(implicit_some)]
    (
        assets: [
            (path: "assets/levels/a.scene", kind: scene, references: ["assets/tex/rock.png", "assets/tex/Bad.png", "assets/tex/gone.png"], cooked_bytes: 10),
            (path: "assets/tex/rock.png", kind: texture, cooked_bytes: 1000, tags: {"team": "art"}),
            (path: "assets/tex/Bad.png", kind: texture, cooked_bytes: 500),
            (path: "assets/dev/x.png", kind: texture, cooked_bytes: 7),
        ],
    )"##;

    #[test]
    fn validate_scope_exclude_cap_and_findings() {
        // ADR-7961: `assets/tex/Bad.png` is uppercase only. The built-in naming rule allows that.
        // Letter case is the existing PathStyle rule (`validate_name_style`), not `validate_name_invalid`.
        let out = call(
            "editor.asset_validate",
            &json!({
                "set": SET2,
                "scope": { "folder": "assets", "recursive": true },
                "exclude": ["assets/dev/**"],
                "validators": ["naming_rules", "missing_references", "path_style"],
                "rules": "(rules: [PathStyle(id: \"lowercase\", glob: \"assets/**\", style: LowerAscii, severity: Error)])",
            }),
        )
        .expect("runs");
        assert_eq!(out["summary"]["checked"], 3);
        assert_eq!(out["summary"]["skipped_excluded"], 1);
        let codes: Vec<&str> = out["results"]
            .as_array()
            .expect("rows")
            .iter()
            .flat_map(|r| r["findings"].as_array().into_iter().flatten())
            .filter_map(|f| f["code"].as_str())
            .collect();
        assert!(
            codes.contains(&"validate_name_style"),
            "uppercase stem is a PathStyle finding: {codes:?}"
        );
        assert!(
            !codes.contains(&"validate_name_invalid"),
            "uppercase alone is not validate_name_invalid: {codes:?}"
        );
        assert!(codes.contains(&"validate_reference_missing"), "{codes:?}");
        // A directly named asset bypasses exclude.
        let named = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "scope": { "assets": ["assets/dev/x.png"] }, "exclude": ["assets/dev/**"] }),
        )
        .expect("runs");
        assert_eq!(named["summary"]["checked"], 1);
        let capped = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "max_assets": 2, "limit": 1 }),
        )
        .expect("runs");
        assert_eq!(capped["summary"]["cap_reached"], true);
        assert_eq!(capped["results"].as_array().map(Vec::len), Some(1));
        assert!(capped["rows_total"].as_u64().unwrap_or(0) > 1);
        let unknown = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "scope": { "assets": ["nope"] } }),
        )
        .expect("a finding, not an error");
        assert_eq!(
            unknown["results"][0]["findings"][0]["code"],
            "validate_unknown_asset"
        );
        assert_eq!(unknown["exit_failed"], true);
    }

    #[test]
    fn validate_rules_and_strict() {
        let rules = r#"(rules: [SizeBudget(id: "tex", kind: Texture, max_bytes: 600, severity: Warning), Forbid(id: "dev", from: "assets/levels/**", to: "assets/dev/**")])"#;
        let out = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "rules": rules, "validators": ["size_budget", "dependency_rules"] }),
        )
        .expect("runs");
        assert_eq!(out["summary"]["warnings"], 1);
        assert_eq!(out["exit_failed"], false);
        let strict = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "rules": rules, "validators": ["size_budget"], "strict": true }),
        )
        .expect("runs");
        assert_eq!(strict["exit_failed"], true);
        let fails = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "rules": rules, "validators": ["size_budget"], "warnings_fail": true }),
        )
        .expect("runs");
        assert_eq!(fails["summary"]["failed"], 1);
        let e = call(
            "editor.asset_validate",
            &json!({ "set": SET2, "rules": "(bogus: 1)" }),
        )
        .expect_err("config");
        assert_eq!(code(&e), "invalid_config");
    }

    #[test]
    fn validate_scope_errors() {
        for scope in [
            json!({}),
            json!({ "assets": [] }),
            json!({ "assets": ["a"], "folder": "x" }),
            json!("project"),
        ] {
            let e = call("editor.asset_validate", &json!({ "set": SET2, "scope": scope }))
                .expect_err("bad scope");
            assert_eq!(code(&e), "validate_bad_scope", "{scope}");
        }
    }

    #[test]
    fn graph_columns_tree_size_map_and_audit() {
        let g = call(
            "editor.asset_graph",
            &json!({ "set": SET2, "assets": ["assets/levels/a.scene"], "max_per_column": 1, "kinds": ["texture"] }),
        )
        .expect("runs");
        assert_eq!(g["columns"][0]["hidden"], 1);
        assert_eq!(g["truncated"], true);
        let page1 = call(
            "editor.asset_graph",
            &json!({ "set": SET2, "assets": ["assets/levels/a.scene"], "max_per_column": 1, "pages": [{ "side": "dependencies", "hop": 1, "page": 1 }] }),
        )
        .expect("runs");
        assert_eq!(page1["columns"][0]["nodes"], json!(["assets/tex/rock.png"]));
        let e = call(
            "editor.asset_graph",
            &json!({ "set": SET2, "assets": ["assets/levels/a.scene"], "dependency_depth": 9 }),
        )
        .expect_err("depth");
        assert_eq!(code(&e), "graph_bad_depth");
        let e = call("editor.asset_graph", &json!({ "set": SET2, "assets": ["nope"] })).expect_err("unknown");
        assert_eq!(code(&e), "unknown_asset");
        let many: Vec<String> = (0..65).map(|i| format!("a{i}")).collect();
        let e = call("editor.asset_graph", &json!({ "set": SET2, "assets": many })).expect_err("roots");
        assert_eq!(code(&e), "graph_too_many_roots");

        let tree = call(
            "editor.asset_tree",
            &json!({ "set": SET2, "asset": "assets/tex/rock.png", "side": "referrers" }),
        )
        .expect("tree");
        assert_eq!(tree["text"], "assets/tex/rock.png\n  dep assets/levels/a.scene\n");

        let map = call(
            "editor.asset_size_map",
            &json!({ "set": SET2, "assets": ["assets/levels/a.scene"], "group_by": "tag", "tag": "team" }),
        )
        .expect("map");
        assert_eq!(map["total_bytes"], 1510);
        assert_eq!(map["missing"], json!(["assets/tex/gone.png"]));
        let e = call(
            "editor.asset_size_map",
            &json!({ "set": SET2, "assets": ["assets/levels/a.scene"], "group_by": "bogus" }),
        )
        .expect_err("group");
        assert_eq!(code(&e), "size_map_bad_group");

        let audit = call(
            "editor.asset_audit",
            &json!({ "set": SET2, "folder": "assets/tex" }),
        )
        .expect("audit");
        assert_eq!(audit["rows"], 2);
        assert!(audit["csv"].as_str().expect("csv").starts_with("path,kind,size,"));
        let again = call(
            "editor.asset_audit",
            &json!({ "set": SET2, "folder": "assets/tex" }),
        )
        .expect("audit");
        assert_eq!(audit, again);
    }

    #[test]
    fn registry_source_without_a_registry_is_an_error() {
        let e = call(
            "editor.asset_graph",
            &json!({ "source": "registry", "assets": ["a"] }),
        )
        .expect_err("none");
        assert_eq!(code(&e), "registry_not_loaded");
        let e = call("editor.asset_audit", &json!({ "source": "bogus", "set": SET2 })).expect_err("source");
        assert_eq!(code(&e), "F0002");
    }

    #[test]
    fn save_validation_reports_state_holds_and_waits() {
        let mut world = World::new();
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.save_validation")
            .expect("registered");
        let fresh = handler(&mut world, &json!({})).expect("state");
        assert_eq!(fresh["enabled"], true);
        assert_eq!(fresh["pending"], 0);
        assert_eq!(fresh["runs"], 0);
        // Saves queue; a hold keeps them queued, and `wait` after the release runs one batch.
        let held = handler(&mut world, &json!({ "hold": true })).expect("hold");
        assert_eq!(held["held"], true);
        crate::save_validation::note_saved(&mut world, std::path::Path::new("assets/a.level.ron"));
        crate::save_validation::note_saved(&mut world, std::path::Path::new("assets/b.level.ron"));
        let still = handler(&mut world, &json!({ "wait": true })).expect("held wait");
        assert_eq!(still["pending"], 2);
        assert_eq!(still["runs"], 0);
        let done = handler(&mut world, &json!({ "hold": false, "wait": true })).expect("release");
        assert_eq!(done["held"], false);
        assert_eq!(done["pending"], 0);
        assert_eq!(done["runs"], 1);
        assert_eq!(
            done["last_batch"],
            json!(["assets/a.level.ron", "assets/b.level.ron"])
        );
        // Off drops the queue.
        let off = handler(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(off["enabled"], false);
        crate::save_validation::note_saved(&mut world, std::path::Path::new("assets/c.level.ron"));
        let after = handler(&mut world, &json!({ "wait": true })).expect("wait");
        assert_eq!(after["runs"], 1);
        let e = handler(&mut world, &json!({ "hold": "yes" })).expect_err("bad type");
        assert_eq!(code(&e), "F0002");
    }

    #[test]
    fn registry_source_reads_ui_layouts_and_an_inline_set_does_not() {
        let root = regia_test_utils::unique_temp_dir("ui-layout-registry");
        std::fs::create_dir_all(root.join("assets/ui")).expect("assets");
        std::fs::write(
            root.join("assets/ui/hud.ui.ron"),
            r#"(use: "ui/health.ui.ron", mask: (asset: "textures/btn.png"))"#,
        )
        .expect("layout");
        let cache = root.join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        regia_asset::registry::AssetRegistry::default()
            .write_to(&regia_asset::registry::cache_registry_path(&cache))
            .expect("registry");
        let loaded = regia_asset::registry::LoadedAssetRegistry::load(&cache, None)
            .expect("load")
            .expect("registry file");
        let mut world = World::new();
        world.insert_resource(loaded);
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let (_, handler) = methods()
            .into_iter()
            .find(|(method, _)| method.name == "editor.asset_validate")
            .expect("method");
        let out = handler(
            &mut world,
            &json!({ "source": "registry", "validators": ["missing_references"] }),
        )
        .expect("validate");
        let layout = out["results"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["asset"] == "ui/hud.ui.ron")
            .expect("the layout is added even though the registry is empty");
        assert!(
            layout["findings"]
                .as_array()
                .expect("findings")
                .iter()
                .any(|finding| finding["code"] == "validate_reference_missing"),
            "{layout}"
        );
        let inline = handler(
            &mut world,
            &json!({
                "set": "(assets: [(path: \"textures/rock.png\", kind: texture)])",
                "validators": ["missing_references"]
            }),
        )
        .expect("inline set");
        assert!(
            inline["results"]
                .as_array()
                .expect("rows")
                .iter()
                .all(|row| row["asset"] != "ui/hud.ui.ron"),
            "an inline set does not walk .ui.ron files"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
