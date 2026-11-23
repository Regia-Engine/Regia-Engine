// SPDX-License-Identifier: MPL-2.0
//! `asset.gltf_extensions`: what a glTF's extensions amount to, before anything is cooked
//! (`regia_asset_pipeline::gltf_normalize`, ADR-3016, `book/agents/gltf-extensions.md`).

use regia_agent::protocol::{self, RpcError};
use regia_agent::req_path;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::ImportError;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::export_glb::MethodHandler;

const HINT: &str = "see book/agents/gltf-extensions.md";

/// The `asset.gltf_extensions` schema row and handler, registered by `agent_ext::install` (the
/// runtime-safe agent crate cannot depend on the asset pipeline).
#[must_use]
pub fn agent_method() -> (MethodSchema, MethodHandler) {
    const PARAMS: &[ParamSchema] = &[ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "Project-relative .gltf or .glb path.",
    }];
    (
        MethodSchema {
            name: "asset.gltf_extensions",
            tool: "asset_gltf_extensions",
            description: "Opens a .gltf or .glb the way the importer does, without decoding images or cooking anything, and reports what its extensions amount to: each extension's support (cooked, side_table, or null when the importer cannot read it), the optional ones it falls back from, compressed views decoded, integer attributes read as floats, instanced nodes with their instance counts, KHR_materials_variants names, and KHR_lights_punctual lights and light nodes. A required extension the importer cannot read is the error gltf_ext_unsupported, with data.extension naming it. Errors carry data.reason: gltf_ext_params, gltf_ext_unsupported, gltf_meshopt_invalid, gltf_quantization_invalid, gltf_instancing_invalid, gltf_unreadable, path_outside_project.",
            params: PARAMS,
            result: "{ ok, path, extensions: [{name, required, support}], fallbacks, meshopt_views, quantized_accessors, baked_uv_materials, instanced_nodes: [[node, count]], variants, variant_primitives, lights, light_nodes }",
            mutating: false,
            permission: Permission::Assets,
        },
        gltf_extensions,
    )
}

/// `F0002` with `data.reason` (and the extension, when one is at fault).
fn refused(reason: &str, message: impl Into<String>, extension: Option<&str>) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
        if let Some(extension) = extension {
            data.insert("extension".into(), json!(extension));
        }
    }
    error
}

fn import_error(error: &ImportError) -> RpcError {
    match error {
        ImportError::GltfExtension { code, extension, .. } => {
            refused(code, error.to_string(), Some(extension))
        }
        other => refused("gltf_unreadable", other.to_string(), None),
    }
}

/// `asset.gltf_extensions { path }`.
fn gltf_extensions(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_gltf_extensions").entered();
    let root = req_path::project_root(world);
    let text = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("gltf_ext_params", "`path` (a .gltf or .glb) is required", None))?;
    let path = req_path::project_path_in(&root, "path", text)?;
    if !path.is_file() {
        return Err(RpcError::not_found(format!("no file at {text}"), HINT));
    }
    let report = regia_asset_pipeline::gltf_extension_report(&path).map_err(|e| import_error(&e))?;
    let mut answer = serde_json::to_value(&report).map_err(|e| {
        refused(
            "gltf_unreadable",
            format!("the report does not encode: {e}"),
            None,
        )
    })?;
    if let Value::Object(fields) = &mut answer {
        fields.insert("ok".into(), json!(true));
        fields.insert("path".into(), json!(text));
    }
    Ok(answer)
}
