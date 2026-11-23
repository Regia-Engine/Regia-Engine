// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of extensions (`docs/specs/57-plugin-extension-api.md` § 5, ADR-1885):
//! the `plugin.*` methods over `regia_framework::extension::ExtensionRegistry`, and the bridge
//! that mounts extension-registered agent methods into the method table.
//!
//! - `plugin.list`, `plugin.info`, `plugin.check` (read) read the registry; `check` re-runs the
//!   resolver as a dry run with the engine's reserved namespaces.
//! - `plugin.enable` / `plugin.disable` (permission `build`, not in the default profile) edit the
//!   `[extensions]` table of `Regia.toml` in place. Code changes take effect at the next start
//!   (`restart_required: true`); a content-only extension mounts or unmounts at once.
//! - `plugin.tool_run` (permission `build`) runs a `[[tool]]` helper as a bounded child process.
//! - `plugin.reload` swaps a dynamic extension; the editor host registers the handler on the
//!   world's `AgentExtension` (consulted before this table). Without it: `not_supported`.
//!
//! Extension methods (`<id>.<verb>`) appear in `agent.schema` under `extension_methods` with
//! `provider: "extension:<id>"` and run through the same permission, rate-limit and audit path as
//! built-in ones. A method in the namespace of a failed or disabled extension answers
//! `method_unavailable`.
//! Their params are checked against the method's `params_schema` before the handler runs; a call
//! that breaks it answers `invalid_params` (`regia_framework::extension::params`, ADR-2136).
//!
//! Errors carry `data: { code, id, detail, hint }` where `code` is the spec § 5.3 code. Documented
//! on `book/agents/plugins.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};

use regia_app::prelude::*;
use regia_framework::extension::manifest::IMPLICIT_CAPABILITIES;
use regia_framework::extension::params::check_params;
use regia_framework::extension::project::{self, ExtensionsConfig};
use regia_framework::extension::tool::{ToolError, ToolOutput, ToolRun, run_tool};
use regia_framework::extension::{ExtensionRegistry, ExtensionState, MethodAccess, ResolveInput, resolve};

use crate::extension::DeferredReply;
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, Permission};

const HINT: &str = "see book/agents/plugins.md";

fn fail(code: i64, reason: &str, id: &str, detail: impl Into<String>) -> RpcError {
    let detail = detail.into();
    RpcError {
        code,
        message: format!("{reason}: {detail}"),
        data: Some(json!({ "code": reason, "id": id, "detail": detail, "hint": HINT })),
    }
}

fn not_found(id: &str) -> RpcError {
    fail(
        protocol::NOT_FOUND,
        "not_found",
        id,
        format!("no extension `{id}` was discovered; see plugin.list"),
    )
}

fn refused(reason: &str, id: &str, detail: impl Into<String>) -> RpcError {
    fail(protocol::INVALID_PARAMS, reason, id, detail)
}

fn unsupported(reason: &str, id: &str, detail: impl Into<String>) -> RpcError {
    fail(protocol::UNSUPPORTED, reason, id, detail)
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (a string) is required")))
}

fn opt_str<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be a string"))),
    }
}

fn str_list(params: &Value, name: &str) -> Result<Option<Vec<String>>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be an array of strings")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{name}` must be an array of strings"
        ))),
    }
}

fn no_registry() -> Value {
    json!({ "available": false, "extensions": [], "resolved_hash": "", "order": [] })
}

/// Every engine namespace (the first segment of a schema method name): ids an extension may not use.
#[must_use]
pub fn reserved_namespaces() -> Vec<String> {
    let mut out: BTreeSet<String> = crate::schema::METHODS
        .iter()
        .filter_map(|m| m.name.split('.').next())
        .map(str::to_owned)
        .collect();
    out.extend(
        regia_framework::extension::registry::DEFAULT_RESERVED_IDS
            .iter()
            .map(|s| (*s).to_owned()),
    );
    out.into_iter().collect()
}

/// `plugin.list`.
///
/// # Errors
///
/// `F0002` for a bad `state`.
pub fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "list").entered();
    let state = opt_str(params, "state")?;
    if let Some(s) = state
        && !matches!(s, "active" | "failed" | "filtered" | "disabled" | "available")
    {
        return Err(RpcError::invalid_params(
            "`state` must be active, failed, filtered, disabled or available",
        ));
    }
    Ok(world
        .get_resource::<ExtensionRegistry>()
        .map_or_else(no_registry, |r| r.list_json(state)))
}

/// `plugin.info`.
///
/// # Errors
///
/// `not_found` for an unknown id.
pub fn info(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "info").entered();
    let id = str_param(params, "id")?;
    let registry = world
        .get_resource::<ExtensionRegistry>()
        .ok_or_else(|| not_found(id))?;
    registry.info_json(id).ok_or_else(|| not_found(id))
}

/// The resolver as a dry run over `registry`'s discovered manifests, with `config` in place of
/// the project's table and every protocol namespace reserved (`plugin.check`, `regia plugin`).
#[must_use]
pub fn dry_run(
    registry: &ExtensionRegistry,
    config: &ExtensionsConfig,
    platform: &str,
    profile: &str,
) -> regia_framework::extension::ResolvedSet {
    let inputs = registry.inputs();
    let (enabled, disabled) = config.effective_lists(platform);
    let grants = config.grant_map();
    let mut reserved = reserved_namespaces();
    reserved.extend(inputs.reserved_ids.iter().cloned());
    reserved.sort();
    reserved.dedup();
    let dynamic_code = platform != "android" && inputs.dynamic_code;
    resolve(&ResolveInput {
        discovered: &inputs.discovery.manifests,
        enabled: &enabled,
        disabled: &disabled,
        grants: &grants,
        engine_version: &inputs.engine_version,
        platform,
        profile,
        dynamic_code,
        reserved_ids: &reserved,
    })
}

/// Diagnostics as `plugin.check` answers them.
#[must_use]
pub fn diagnostics_json(diags: &[regia_framework::extension::Diagnostic]) -> Value {
    Value::Array(
        diags
            .iter()
            .map(|d| {
                json!({
                    "id": d.id, "code": d.code,
                    "severity": format!("{:?}", d.severity).to_lowercase(), "message": d.message,
                })
            })
            .collect(),
    )
}

/// `plugin.check`: the resolver as a dry run (nothing changes).
///
/// # Errors
///
/// `F0002` for bad params.
pub fn check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "check").entered();
    let enabled = str_list(params, "enabled")?;
    let platform = opt_str(params, "platform")?;
    let profile = opt_str(params, "profile")?;
    if let Some(p) = platform
        && !matches!(p, "linux" | "windows" | "android" | "macos")
    {
        return Err(RpcError::invalid_params(
            "`platform` must be linux, windows, android or macos",
        ));
    }
    if let Some(p) = profile
        && !matches!(p, "dev" | "shipping")
    {
        return Err(RpcError::invalid_params("`profile` must be dev or shipping"));
    }
    let Some(registry) = world.get_resource::<ExtensionRegistry>() else {
        return Ok(json!({ "available": false, "order": [], "diagnostics": [] }));
    };
    let mut config = registry.config().clone();
    if let Some(enabled) = enabled {
        config.enabled = enabled;
    }
    let platform = platform.unwrap_or(&registry.inputs().platform).to_owned();
    let profile = profile.unwrap_or(&registry.inputs().profile).to_owned();
    let set = dry_run(registry, &config, &platform, &profile);
    let states: BTreeMap<&str, &str> = set.states.iter().map(|(k, v)| (k.as_str(), v.name())).collect();
    Ok(json!({
        "order": set.order.iter().map(|e| json!({ "id": e.id, "version": e.version.to_string(), "phase": e.phase.name() })).collect::<Vec<_>>(),
        "diagnostics": diagnostics_json(&set.diagnostics),
        "states": states,
        "resolved_hash": set.resolved_hash,
        "platform": platform,
        "profile": profile,
    }))
}

fn project_file(registry: &ExtensionRegistry, id: &str) -> Result<std::path::PathBuf, RpcError> {
    registry
        .inputs()
        .project_dir
        .as_ref()
        .map(|d| d.join("Regia.toml"))
        .ok_or_else(|| {
            unsupported(
                "not_supported",
                id,
                "this app was started without a project directory, so Regia.toml cannot be edited",
            )
        })
}

fn write_project(path: &std::path::Path, text: &str, id: &str) -> Result<(), RpcError> {
    let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, text.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            fail(
                protocol::INTERNAL_ERROR,
                "io_failed",
                id,
                format!("{}: {e}", path.display()),
            )
        })
}

fn read_project(path: &std::path::Path, id: &str) -> Result<String, RpcError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(fail(
            protocol::INTERNAL_ERROR,
            "io_failed",
            id,
            format!("{}: {e}", path.display()),
        )),
    }
}

fn platform_param(params: &Value) -> Result<Option<&str>, RpcError> {
    let platform = opt_str(params, "platform")?;
    if let Some(p) = platform
        && !matches!(p, "linux" | "windows" | "android" | "macos")
    {
        return Err(RpcError::invalid_params(
            "`platform` must be linux, windows, android or macos",
        ));
    }
    Ok(platform)
}

/// `plugin.enable`: adds the id to `enabled` (and the grants), checks the result as a dry run.
///
/// # Errors
///
/// `not_found`; `capability_not_granted` (with `data.missing`) when a requested capability is
/// neither granted in the project nor passed in `grant`; `manifest_invalid` when `Regia.toml`
/// cannot be edited in place; `not_supported` without a project directory.
pub fn enable(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "enable").entered();
    let id = str_param(params, "id")?;
    let grant = str_list(params, "grant")?.unwrap_or_default();
    let platform = platform_param(params)?;
    let Some(mut registry) = world.remove_resource::<ExtensionRegistry>() else {
        return Err(not_found(id));
    };
    let result = enable_in(&mut registry, id, &grant, platform);
    world.insert_resource(registry);
    result
}

fn enable_in(
    registry: &mut ExtensionRegistry,
    id: &str,
    grant: &[String],
    platform: Option<&str>,
) -> Result<Value, RpcError> {
    let found = registry.discovered(id).cloned().ok_or_else(|| not_found(id))?;
    let known = regia_framework::extension::manifest::KNOWN_CAPABILITIES;
    if let Some(bad) = grant.iter().find(|g| !known.contains(&g.as_str())) {
        return Err(refused(
            "unknown_capability",
            id,
            format!("`{bad}` is not a capability"),
        ));
    }
    // Only what the manifest asks for is stored: a grant waiting for a later manifest would let
    // that manifest pass with no review.
    let requested = &found.manifest.capabilities;
    let grant: Vec<String> = grant.iter().filter(|g| requested.contains(g)).cloned().collect();
    let grant = grant.as_slice();
    let already = registry.config().grant_map().remove(id).unwrap_or_default();
    let missing: Vec<&String> = found
        .manifest
        .load_capabilities
        .iter()
        .filter(|c| {
            !IMPLICIT_CAPABILITIES.contains(&c.as_str()) && !already.contains(c) && !grant.contains(c)
        })
        .collect();
    if !missing.is_empty() {
        let mut e = refused(
            "capability_not_granted",
            id,
            format!(
                "`{id}` requests {} which the project has not granted; review them (plugin.info) and pass them in `grant`",
                missing
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        if let Some(Value::Object(data)) = e.data.as_mut() {
            data.insert("missing".into(), json!(missing));
        }
        return Err(e);
    }
    let path = project_file(registry, id)?;
    let text = read_project(&path, id)?;
    let edited = project::set_enabled(&text, id, true, platform)
        .and_then(|t| {
            if grant.is_empty() {
                Ok(t)
            } else {
                project::set_grants(&t, id, grant)
            }
        })
        .map_err(|e| refused("manifest_invalid", id, e.0))?;
    let config =
        ExtensionsConfig::from_project_toml(&edited).map_err(|e| refused("manifest_invalid", id, e.0))?;
    let platform_name = platform.unwrap_or(&registry.inputs().platform).to_owned();
    let profile = registry.inputs().profile.clone();
    let set = dry_run(registry, &config, &platform_name, &profile);
    write_project(&path, &edited, id)?;
    let content_only = found.manifest.code.is_empty();
    let activates = set.states.get(id) == Some(&ExtensionState::Active);
    let mut mounted = Vec::new();
    let mut mount_errors = Vec::new();
    if content_only && activates && platform.is_none_or(|p| p == registry.inputs().platform) {
        for root in &found.manifest.content {
            match registry_mount(registry, id, std::path::Path::new(&found.path), root) {
                Ok(mount) => mounted.push(mount),
                Err(error) => mount_errors.push(error),
            }
        }
    }
    let restart_required = registry.note_toggled(id, true);
    Ok(json!({
        "enabled": true,
        "restart_required": restart_required,
        "state_after_restart": set.states.get(id).map(|s| s.name()),
        "diagnostics": diagnostics_json(&set.diagnostics),
        "mounted": mounted,
        "mount_errors": mount_errors,
    }))
}

fn registry_mount(
    registry: &mut ExtensionRegistry,
    id: &str,
    dir: &std::path::Path,
    root: &str,
) -> Result<Value, String> {
    let mount = registry.mount_content_live(id, dir, root)?;
    Ok(
        json!({ "root": mount.root.display().to_string(), "files": mount.files, "collisions": mount.collisions }),
    )
}

/// `plugin.disable`: adds the id to `disabled`; lists the active extensions that would fail.
///
/// # Errors
///
/// As [`enable`] (without the grant checks).
pub fn disable(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "disable").entered();
    let id = str_param(params, "id")?;
    let platform = platform_param(params)?;
    let Some(mut registry) = world.remove_resource::<ExtensionRegistry>() else {
        return Err(not_found(id));
    };
    let result = disable_in(&mut registry, id, platform);
    world.insert_resource(registry);
    result
}

fn disable_in(registry: &mut ExtensionRegistry, id: &str, platform: Option<&str>) -> Result<Value, RpcError> {
    let found = registry.discovered(id).cloned().ok_or_else(|| not_found(id))?;
    let path = project_file(registry, id)?;
    let text = read_project(&path, id)?;
    let edited =
        project::set_enabled(&text, id, false, platform).map_err(|e| refused("manifest_invalid", id, e.0))?;
    let config =
        ExtensionsConfig::from_project_toml(&edited).map_err(|e| refused("manifest_invalid", id, e.0))?;
    let platform_name = platform.unwrap_or(&registry.inputs().platform).to_owned();
    let profile = registry.inputs().profile.clone();
    let before = dry_run(registry, registry.config(), &platform_name, &profile);
    let after = dry_run(registry, &config, &platform_name, &profile);
    let dependents: Vec<&String> = before
        .states
        .iter()
        .filter(|(k, v)| {
            k.as_str() != id
                && **v == ExtensionState::Active
                && after.states.get(*k) != Some(&ExtensionState::Active)
        })
        .map(|(k, _)| k)
        .collect();
    write_project(&path, &edited, id)?;
    let mut unmounted = 0;
    if found.manifest.code.is_empty() && platform.is_none_or(|p| p == registry.inputs().platform) {
        unmounted = registry.unmount_content_live(id);
    }
    let restart_required = registry.note_toggled(id, false);
    Ok(json!({
        "disabled": true,
        "restart_required": restart_required,
        "dependents_affected": dependents,
        "unmounted": unmounted,
    }))
}

/// `plugin.reload` without an editor host: `not_found`, or `not_supported` (a source extension
/// reloads with the game's hot reload; a dynamic one needs the editor's loader).
///
/// # Errors
///
/// Always.
pub fn reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = str_param(params, "id")?;
    let registry = world
        .get_resource::<ExtensionRegistry>()
        .ok_or_else(|| not_found(id))?;
    let found = registry.discovered(id).ok_or_else(|| not_found(id))?;
    let detail = if found.manifest.kind() == "source" && registry.dynamic(id).is_none() {
        format!("`{id}` is a source extension; it reloads with the game's hot reload (specs/20)")
    } else if found.manifest.kind() == "content" {
        format!("`{id}` is content-only; disable and enable it to remount")
    } else {
        "dynamic reload runs in the editor host; this app has no extension loader".to_owned()
    };
    Err(unsupported("not_supported", id, detail))
}

/// The run of tool `name` of extension `id`, after the checks every host makes: the extension is
/// discovered, the tool exists, `process` is requested and granted, and a tool that lists
/// `network` has that grant too (informational, since nothing blocks sockets; spec § 2.7).
///
/// # Errors
///
/// `not_found`, `capability_not_granted`.
pub fn prepare_tool(
    registry: &ExtensionRegistry,
    id: &str,
    name: &str,
    args: Vec<String>,
    stdin: Vec<u8>,
) -> Result<ToolRun, RpcError> {
    let found = registry.discovered(id).ok_or_else(|| not_found(id))?;
    let tool = found
        .manifest
        .tools
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| {
            fail(
                protocol::NOT_FOUND,
                "not_found",
                id,
                format!("`{id}` has no tool `{name}`; see plugin.info"),
            )
        })?;
    let grants = registry.config().grant_map().remove(id).unwrap_or_default();
    let needed = std::iter::once("process").chain(tool.capabilities.iter().map(String::as_str));
    for capability in needed {
        if !grants.iter().any(|g| g == capability) {
            return Err(refused(
                "capability_not_granted",
                id,
                format!("tool `{name}` needs `{capability}`, which the project has not granted"),
            ));
        }
    }
    let mut run = ToolRun::for_entry(std::path::Path::new(&found.path), tool, args);
    run.stdin = stdin;
    Ok(run)
}

/// A finished tool run as JSON (`{ status, stdout, stderr }`, text lossily decoded).
#[must_use]
pub fn tool_json(output: &ToolOutput) -> Value {
    json!({
        "status": output.status,
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    })
}

/// `plugin.tool_run`: runs the tool on its own thread and answers when it ends, so a slow tool
/// never holds a frame. The child gets a cleared environment, a timeout and an output bound.
///
/// # Errors
///
/// As [`prepare_tool`]; a run that fails later answers `tool_timeout`,
/// `tool_output_too_large` or `load_failed`.
pub fn tool_run(world: &mut World, params: &Value, reply: DeferredReply) -> Result<(), RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "tool_run").entered();
    let id = str_param(params, "id")?.to_owned();
    let name = str_param(params, "name")?;
    let args = str_list(params, "args")?.unwrap_or_default();
    let stdin = opt_str(params, "stdin")?.unwrap_or_default().as_bytes().to_vec();
    let registry = world
        .get_resource::<ExtensionRegistry>()
        .ok_or_else(|| not_found(&id))?;
    let run = prepare_tool(registry, &id, name, args, stdin)?;
    std::thread::Builder::new()
        .name("extension-tool".to_owned())
        .spawn(move || {
            let result = run_tool(&run).map(|output| tool_json(&output)).map_err(|error| {
                let code = if matches!(error, ToolError::Spawn(_)) {
                    protocol::INTERNAL_ERROR
                } else {
                    protocol::TIMEOUT
                };
                fail(code, error.code(), &id, error.to_string())
            });
            let _ = reply.send(result);
        })
        .map(|_| ())
        .map_err(|e| RpcError::internal(format!("cannot start the tool thread: {e}")))
}

/// Schema rows of extension methods, built once per name (names are leaked so the row can use
/// the `&'static str` fields every other method has; bounded by the distinct names registered).
#[derive(Resource, Default)]
pub struct ExtensionMethodSchemas(HashMap<String, MethodSchema>);

fn permission_of(access: MethodAccess) -> Permission {
    match access {
        MethodAccess::Read => Permission::Read,
        MethodAccess::Play => Permission::Play,
        MethodAccess::EditScene => Permission::EditScene,
    }
}

/// The schema row of an extension method, when an active extension registered `name`.
#[must_use]
pub fn extension_schema(world: &mut World, name: &str) -> Option<MethodSchema> {
    let (access, summary) = {
        let registry = world.get_resource::<ExtensionRegistry>()?;
        let (_, desc) = registry.agent_method(name)?;
        (desc.access, desc.summary.clone())
    };
    let mut cache = world.get_resource_or_insert_with(ExtensionMethodSchemas::default);
    if let Some(row) = cache.0.get(name)
        && row.permission == permission_of(access)
    {
        return Some(row.clone());
    }
    let row = MethodSchema {
        name: Box::leak(name.to_owned().into_boxed_str()),
        tool: "",
        description: Box::leak(summary.into_boxed_str()),
        params: &[],
        result: "see the extension's guide page",
        mutating: access != MethodAccess::Read,
        permission: permission_of(access),
    };
    cache.0.insert(name.to_owned(), row.clone());
    Some(row)
}

/// Runs an extension method, when an active extension registered `name`.
pub fn route_extension(world: &mut World, name: &str, params: &Value) -> Option<Result<Value, RpcError>> {
    let (owner, handler, schema) = {
        let registry = world.get_resource::<ExtensionRegistry>()?;
        let (owner, desc) = registry.agent_method(name)?;
        (
            owner.to_owned(),
            std::sync::Arc::clone(&desc.handler),
            desc.params_schema.clone(),
        )
    };
    let _span = tracing::info_span!("agent_extension_method", method = name, extension = %owner).entered();
    // The params meet the method's `params_schema` before the handler runs (ADR-2136).
    if let Err(violation) = check_params(&schema, params) {
        return Some(Err(RpcError {
            code: protocol::INVALID_PARAMS,
            message: format!("{name}: invalid_params: {violation}"),
            data: Some(json!({
                "code": "invalid_params", "id": owner, "path": violation.path, "rule": violation.rule,
                "detail": violation.detail, "provider": format!("extension:{owner}"), "hint": HINT,
            })),
        }));
    }
    Some(handler(world, params.clone()).map_err(|fault| {
        let valid = !fault.code.is_empty()
            && fault
                .code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        let reason = if valid {
            fault.code.clone()
        } else {
            "extension_error".to_owned()
        };
        RpcError {
            code: protocol::INVALID_PARAMS,
            message: format!("{name}: {reason}"),
            data: Some(json!({
                "code": reason, "id": owner, "detail": fault.data,
                "provider": format!("extension:{owner}"), "hint": HINT,
            })),
        }
    }))
}

/// `method_unavailable` for a method in the namespace of a discovered extension that is not
/// active (spec § 5.2).
#[must_use]
pub fn unavailable(world: &World, name: &str) -> Option<RpcError> {
    let registry = world.get_resource::<ExtensionRegistry>()?;
    let (id, state) = registry.unavailable_provider(name)?;
    let mut e = fail(
        protocol::NOT_FOUND,
        "method_unavailable",
        id,
        format!(
            "`{name}` belongs to extension `{id}`, which is {}; see plugin.info",
            state.name()
        ),
    );
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("state".into(), json!(state.name()));
    }
    Some(e)
}

/// The names of every active extension method (`agent.hello` lists them).
#[must_use]
pub fn method_names(world: &World) -> Vec<String> {
    world
        .get_resource::<ExtensionRegistry>()
        .map_or_else(Vec::new, |registry| {
            registry
                .agent_methods()
                .filter(|(owner, _)| registry.state(owner) == Some(ExtensionState::Active))
                .map(|(_, m)| m.name.clone())
                .collect()
        })
}

/// The `extension_methods` array `agent.schema` adds: every active extension method with its
/// provider, access and schemas.
#[must_use]
pub fn schema_rows(world: &World) -> Value {
    let Some(registry) = world.get_resource::<ExtensionRegistry>() else {
        return json!([]);
    };
    Value::Array(
        registry
            .agent_methods()
            .filter(|(owner, _)| registry.state(owner) == Some(ExtensionState::Active))
            .map(|(owner, m)| {
                json!({
                    "name": m.name, "description": m.summary, "provider": format!("extension:{owner}"),
                    "permission": m.access.name(), "mutating": m.access != MethodAccess::Read,
                    "params_schema": m.params_schema, "result_schema": m.result_schema,
                })
            })
            .collect(),
    )
}
