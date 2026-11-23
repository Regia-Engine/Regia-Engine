// SPDX-License-Identifier: MPL-2.0
//! Read-only `asset.gltf_lint` adapter (ADR-8068).

use regia_agent::protocol::{self, RpcError};
use regia_agent::req_path;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::gltf_lint::{GltfLintSettings, lint_gltf};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::export_glb::MethodHandler;

const HINT: &str = "see book/agents/gltf-lint.md";

/// Schema and handler installed by the editor, which owns the desktop pipeline dependency.
#[must_use]
pub fn agent_method() -> (MethodSchema, MethodHandler) {
    const PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "path",
            kind: "string",
            required: true,
            description: "Project-relative .gltf or .glb source path.",
        },
        ParamSchema {
            name: "settings",
            kind: "object",
            required: false,
            description: "Optional shared input_limits, GeometrySettings in geometry, mode (legacy or robust), robust tolerances (tolerance_meters, tolerance_relative, uv_tolerance, attribute_tolerance, min_area_square_meters) and max_candidate_pairs/max_attribute_elements budgets, and max_vertices, max_triangles, max_channels, max_animation_elements, max_findings budgets. See the guide for defaults and units.",
        },
    ];
    (
        MethodSchema {
            name: "asset.gltf_lint",
            tool: "asset_gltf_lint",
            description: "Read-only source-local glTF geometry and rig lint: duplicate mesh definitions, UV-aware coincident triangles and animation channel warnings. Never rewrites sources or cooked assets. Non-triangle-list topology is explicitly skipped; settings.mode robust expands strips and fans, compares neighboring tolerance cells under a candidate budget and reports exact versus tolerance agreement apart from material, UV-set, normal, skin and morph equivalence. Errors carry data.reason: gltf_lint_settings, gltf_lint_geometry, gltf_lint_limit, content_lint_limit, content_lint_cancelled, content_lint_changed, gltf_unreadable, gltf_lint_params, path_outside_project.",
            params: PARAMS,
            result: "{ ok, path, input_limits, input_usage, meshes, primitives, triangles, duplicate_meshes: [{original, duplicate}], coincident_triangles: [{mesh, primitive, pair: {keeper, suspect}}], rig_channels: [{code, animation, channel, node}], skipped_primitives: [{mesh, primitive, mode}], mode, robust_coincidences: [{mesh, keeper, suspect, agreement, max_separation_meters, tolerance_meters, equivalence, differences, unavailable, uv_sets_compared, safe_delete_candidate, fingerprint}], mesh_evidence: [{mesh, primitive, mode, triangles, non_finite, repeated_index, zero_area, first_non_finite, first_repeated_index, first_zero_area}], robust_work: {eligible_triangles, cells, candidate_pairs} }",
            mutating: false,
            permission: Permission::Read,
        },
        gltf_lint,
    )
}

fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn gltf_lint(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = req_path::project_root(world);
    let text = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("gltf_lint_params", "`path` is required"))?;
    let settings = params
        .get("settings")
        .map(|v| serde_json::from_value::<GltfLintSettings>(v.clone()))
        .transpose()
        .map_err(|e| refused("gltf_lint_settings", e.to_string()))?
        .unwrap_or_default();
    settings
        .validate()
        .map_err(|e| refused(e.code(), e.to_string()))?;
    let path = req_path::project_path_in(&root, "path", text)?;
    if !path.is_file() {
        return Err(RpcError::not_found(format!("no file at {text}"), HINT));
    }
    if let Some((reason, message)) = crate::content_lint_jobs::sync_limit_error(&path) {
        return Err(refused(reason, message));
    }
    let report = lint_gltf(&path, &settings).map_err(|e| refused(e.code(), e.to_string()))?;
    let mut answer = serde_json::to_value(&report).map_err(|e| refused("gltf_unreadable", e.to_string()))?;
    if let Value::Object(fields) = &mut answer {
        fields.insert("ok".into(), json!(true));
        fields.insert("path".into(), json!(text));
    }
    Ok(answer)
}
