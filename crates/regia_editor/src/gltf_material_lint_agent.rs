// SPDX-License-Identifier: MPL-2.0
//! Editor-hosted source material lint; fix is a separately permission-gated method.
use regia_agent::extension::AgentHandler;
use regia_agent::protocol::{self, RpcError};
use regia_agent::req_path;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::gltf_material_lint::{MaterialLintSettings, lint_material_file};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

const HINT: &str = "see book/agents/gltf-material-lint.md";
const PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "Project-relative .gltf or .glb source.",
    },
    ParamSchema {
        name: "settings",
        kind: "object",
        required: false,
        description: "MaterialLintSettings JSON. profile is standard (default: preserves valid glTF factor multiplication, missing-factor semantics and surface tint) or compatibility (explicit appearance-changing ORM reset, dielectric missing-metal and volume-tint migration). A name rule or classify_untextured is an explicit artistic selection on every profile. input_limits are the shared admission ceilings. Omitted fields use defaults.",
    },
];

pub(crate) fn methods() -> [(MethodSchema, AgentHandler); 2] {
    [
        (
            MethodSchema {
                name: "asset.gltf_material_lint",
                tool: "asset_gltf_material_lint",
                description: "Check source material metadata without writing. The default profile standard preserves valid glTF appearance. compatibility, a name rule, or classify_untextured selects appearance-changing repairs. The result names the effective profile. Findings use lowercase rule codes and effect preserves_appearance or changes_appearance. Errors carry data.reason: gltf_material_params, gltf_material_settings, gltf_material_invalid, gltf_material_io, content_lint_limit, content_lint_cancelled, content_lint_changed, path_outside_project.",
                params: PARAMS,
                result: "{ok, path, profile, compatible, extensions, texture_roles, extension_nodes, extension_bytes, findings:[{code, material, pointer, message, effect}], remaining, changed, input_limits, input_usage}",
                mutating: false,
                permission: Permission::Read,
            },
            check,
        ),
        (
            MethodSchema {
                name: "asset.gltf_material_fix",
                tool: "asset_gltf_material_fix",
                description: "Atomically repair source material metadata. Default profile standard keeps valid factor products, omitted factors and surface tint. Appearance-changing ORM, dielectric and volume-tint repairs require settings.profile compatibility or an explicit name rule. Geometry, texture bytes and unknown JSON are preserved; a valid unchanged file is not rewritten. Errors carry data.reason: gltf_material_params, gltf_material_settings, gltf_material_invalid, gltf_material_io, content_lint_limit, content_lint_cancelled, content_lint_changed, path_outside_project.",
                params: PARAMS,
                result: "{ok, path, profile, compatible, extensions, texture_roles, extension_nodes, extension_bytes, findings:[{code, material, pointer, message, effect}], remaining, changed, input_limits, input_usage}",
                mutating: true,
                permission: Permission::Assets,
            },
            fix,
        ),
    ]
}

fn error(reason: &str, message: impl Into<String>) -> RpcError {
    let mut e = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = &mut e.data {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    run(world, params, false)
}
fn fix(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    run(world, params, true)
}

fn run(world: &mut World, params: &Value, fix: bool) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| error("gltf_material_params", "expected an object"))?;
    if fields.keys().any(|k| k != "path" && k != "settings") {
        return Err(error(
            "gltf_material_params",
            "only path and settings are accepted; fixing uses asset.gltf_material_fix",
        ));
    }
    let text = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| error("gltf_material_params", "path is required"))?;
    let root = req_path::project_root(world);
    let path = req_path::project_path_in(&root, "path", text)?;
    let settings: MaterialLintSettings = params
        .get("settings")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| error("gltf_material_settings", e.to_string()))?
        .unwrap_or_default();
    if !fix && let Some((reason, message)) = crate::content_lint_jobs::sync_limit_error(&path) {
        return Err(error(reason, message));
    }
    let report = lint_material_file(&path, &settings, fix).map_err(|e| error(e.code, e.message))?;
    let mut result =
        serde_json::to_value(&report).map_err(|e| error("gltf_material_invalid", e.to_string()))?;
    result["ok"] = json!(report.remaining.is_empty() && report.compatible);
    result["path"] = json!(text);
    Ok(result)
}
