// SPDX-License-Identifier: MPL-2.0
//! `editor.fbx_inspect`: the FBX importer on the agent protocol (`book/agents/fbx-import.md`,
//! ADR-1602).
//!
//! Converts an FBX file (by `path`) or inline ASCII FBX (`text`) with
//! [`regia_asset_pipeline::fbx_import::convert_fbx_to_glb`] and returns the import report: what a
//! cook of the file would produce, the file's unit and up axis, texture lookups and warnings.
//! Read-only: nothing is written and nothing in the world changes.
//!
//! `editor.obj_inspect` does the same for Wavefront OBJ files (the pure-Rust OBJ reader).

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::fbx_import::{FbxImportSettings, TRIANGLE_FBX, convert_fbx_to_glb, read_fbx_file};
use regia_asset_pipeline::obj_import::{ObjImportSettings, convert_obj_to_glb, read_obj_file};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// Largest inline `text` an agent call may send (bytes).
const MAX_TEXT_BYTES: usize = 16 << 20;

/// An `invalid_params` error carrying `data.fbx_error = code`.
fn fbx_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["fbx_error"] = json!(code);
    }
    error
}

fn settings_from_params(params: &Value) -> Result<FbxImportSettings, RpcError> {
    let mut settings = FbxImportSettings::default();
    if let Some(scale) = params.get("scale") {
        let scale = scale
            .as_f64()
            .filter(|s| s.is_finite() && *s > 0.0)
            .ok_or_else(|| fbx_error("`scale` must be a positive number", "fbx_bad_params"))?;
        settings.scale = scale as f32;
    }
    if let Some(rate) = params.get("sample_rate") {
        let rate = rate
            .as_f64()
            .filter(|r| (1.0..=240.0).contains(r))
            .ok_or_else(|| fbx_error("`sample_rate` must be in 1..=240", "fbx_bad_params"))?;
        settings.sample_rate = rate as f32;
    }
    if let Some(animation) = params.get("import_animation") {
        settings.import_animation = animation
            .as_bool()
            .ok_or_else(|| fbx_error("`import_animation` must be a boolean", "fbx_bad_params"))?;
    }
    if let Some(set) = str_param(params, "uv_set") {
        settings.uv_set = Some(set.to_string());
    }
    Ok(settings)
}

/// `editor.fbx_inspect`.
fn fbx_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = settings_from_params(params)?;
    let (bytes, base_dir) = match (
        str_param(params, "path"),
        str_param(params, "text"),
        str_param(params, "sample"),
    ) {
        (Some(path), None, None) => {
            // ADR-7388: the editor's one document-path rule, as `editor.usd_inspect`.
            let path = crate::agent_ext::document_request_path(world, "path", path)?;
            let path = path.as_path();
            let bytes =
                read_fbx_file(path, &settings.limits).map_err(|e| fbx_error(e.to_string(), e.code()))?;
            (bytes, path.parent().map(Path::to_path_buf))
        }
        (None, Some(text), None) => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(fbx_error(
                    format!("inline `text` is limited to {MAX_TEXT_BYTES} bytes"),
                    "fbx_bad_params",
                ));
            }
            (text.as_bytes().to_vec(), None)
        }
        (None, None, Some("triangle")) => (TRIANGLE_FBX.as_bytes().to_vec(), None),
        (None, None, Some(other)) => {
            return Err(fbx_error(
                format!("unknown sample {other:?} (triangle)"),
                "fbx_bad_params",
            ));
        }
        _ => {
            return Err(fbx_error(
                "give exactly one of `path` (an .fbx file), `text` (inline ASCII FBX) or `sample`",
                "fbx_bad_params",
            ));
        }
    };
    let conversion = convert_fbx_to_glb(&bytes, base_dir.as_deref(), &settings)
        .map_err(|e| fbx_error(e.to_string(), e.code()))?;
    let mut out = serde_json::to_value(&conversion.report)
        .map_err(|e| RpcError::internal(format!("report serialization: {e}")))?;
    out["glb_bytes"] = json!(conversion.glb.len());
    Ok(out)
}

/// An `invalid_params` error carrying `data.obj_error = code`.
fn obj_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["obj_error"] = json!(code);
    }
    error
}

/// `editor.obj_inspect`.
fn obj_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut settings = ObjImportSettings::default();
    if let Some(scale) = params.get("scale") {
        settings.scale = scale
            .as_f64()
            .filter(|s| s.is_finite() && *s > 0.0)
            .ok_or_else(|| obj_error("`scale` must be a positive number", "obj_bad_params"))?
            as f32;
    }
    let (bytes, base_dir) = match (str_param(params, "path"), str_param(params, "text")) {
        (Some(path), None) => {
            // ADR-7388: the editor's one document-path rule, as `editor.usd_inspect`.
            let path = crate::agent_ext::document_request_path(world, "path", path)?;
            let path = path.as_path();
            let bytes =
                read_obj_file(path, &settings.limits).map_err(|e| obj_error(e.to_string(), e.code()))?;
            (bytes, path.parent().map(Path::to_path_buf))
        }
        (None, Some(text)) => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(obj_error(
                    format!("inline `text` is limited to {MAX_TEXT_BYTES} bytes"),
                    "obj_bad_params",
                ));
            }
            (text.as_bytes().to_vec(), None)
        }
        _ => {
            return Err(obj_error(
                "give exactly one of `path` (an .obj file) or `text` (inline OBJ)",
                "obj_bad_params",
            ));
        }
    };
    let conversion = convert_obj_to_glb(&bytes, base_dir.as_deref(), &settings)
        .map_err(|e| obj_error(e.to_string(), e.code()))?;
    let mut out = serde_json::to_value(&conversion.report)
        .map_err(|e| RpcError::internal(format!("report serialization: {e}")))?;
    out["glb_bytes"] = json!(conversion.glb.len());
    Ok(out)
}

/// The FBX importer's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.fbx_inspect",
                tool: "fbx_inspect",
                description: "Run the FBX importer (ufbx, pipeline only; ADR-1602) on an .fbx file or inline ASCII \
                FBX and report what a cook would produce: nodes, meshes, vertices, triangles, skins, joints, \
                morph targets, materials, texture lookups, clips, the file's unit and up axis, warnings. \
                Read-only; writes nothing.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The .fbx file (textures are looked up beside it).",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "Inline ASCII FBX instead of a file (at most 16 MiB; no external textures).",
                    },
                    ParamSchema {
                        name: "sample",
                        kind: "string",
                        required: false,
                        description: "\"triangle\": the built-in one-triangle file (a smoke check).",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "number",
                        required: false,
                        description: "Uniform scale after the unit conversion to meters (default 1).",
                    },
                    ParamSchema {
                        name: "import_animation",
                        kind: "boolean",
                        required: false,
                        description: "Convert animation stacks (default true).",
                    },
                    ParamSchema {
                        name: "sample_rate",
                        kind: "number",
                        required: false,
                        description: "Samples per second for non-linear curves (1..=240, default 30).",
                    },
                    ParamSchema {
                        name: "uv_set",
                        kind: "string",
                        required: false,
                        description: "The UV set that becomes UV0 (default: the first).",
                    },
                ],
                result: "{ ascii, version, creator, source_unit_meters, source_up_axis, scale, nodes, meshes, \
                vertices, triangles, skins, joints, morph_targets, materials, uv_sets, textures: [{material, \
                role, file, status}], clips: [{name, duration_seconds, channels}], warnings, glb_bytes }",
                mutating: false,
                permission: Permission::Read,
            },
            fbx_inspect_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.obj_inspect",
                tool: "obj_inspect",
                description: "Run the OBJ importer (pure Rust) on an .obj file or inline OBJ text and report what a \
                cook would produce: objects, vertices, triangles, materials, textures found, warnings. Read-only; \
                writes nothing.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The .obj file (`mtllib` files and textures are looked up beside it).",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "Inline OBJ text instead of a file (at most 16 MiB; no `mtllib`).",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "number",
                        required: false,
                        description: "Uniform scale (OBJ has no unit; default 1 = meters).",
                    },
                ],
                result: "{ objects, vertices, triangles, materials, textures, warnings, glb_bytes }",
                mutating: false,
                permission: Permission::Read,
            },
            obj_inspect_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("fbx_error"))
            .and_then(Value::as_str)
    }

    #[test]
    fn the_agent_method_reports_the_sample_file_and_inline_text() {
        let mut world = World::new();
        let sample = fbx_inspect_method(&mut world, &json!({ "sample": "triangle" })).expect("sample");
        assert_eq!(sample["meshes"], 1);
        assert_eq!(sample["triangles"], 1);
        assert_eq!(sample["source_up_axis"], "+Y");
        assert!(sample["glb_bytes"].as_u64().is_some_and(|n| n > 0));
        let inline =
            fbx_inspect_method(&mut world, &json!({ "text": TRIANGLE_FBX, "scale": 2.0 })).expect("text");
        assert_eq!(inline["scale"], 2.0);
        assert_eq!(inline["vertices"], 3);

        let dir = regia_test_utils::unique_temp_dir("fbx-agent");
        let path = dir.join("triangle.fbx");
        std::fs::write(&path, TRIANGLE_FBX).expect("write");
        let file = fbx_inspect_method(&mut world, &json!({ "path": path.to_string_lossy() })).expect("path");
        assert_eq!(file["triangles"], 1);
        // Same call, same answer.
        let again =
            fbx_inspect_method(&mut world, &json!({ "path": path.to_string_lossy() })).expect("again");
        assert_eq!(file, again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agent_method_reports_typed_error_codes() {
        let mut world = World::new();
        let cases = [
            (json!({}), "fbx_bad_params"),
            (json!({ "sample": "teapot" }), "fbx_bad_params"),
            (json!({ "sample": "triangle", "text": "x" }), "fbx_bad_params"),
            (json!({ "sample": "triangle", "scale": -1.0 }), "fbx_bad_params"),
            (
                json!({ "sample": "triangle", "sample_rate": 0 }),
                "fbx_bad_params",
            ),
            (
                json!({ "sample": "triangle", "import_animation": "yes" }),
                "fbx_bad_params",
            ),
            (json!({ "path": "absent-regia-doc-path.fbx" }), "fbx_io"),
            (json!({ "text": "just words" }), "fbx_not_fbx"),
            (json!({ "text": "A: { B: { } }" }), "fbx_parse"),
            (
                json!({ "text": format!("{}{}", "A: {".repeat(200), "}".repeat(200)) }),
                "fbx_too_deep",
            ),
        ];
        for (params, expected) in cases {
            let error = fbx_inspect_method(&mut world, &params).expect_err("must fail");
            assert_eq!(code(&error), Some(expected), "{params}");
            assert_eq!(error.code, regia_agent::protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn the_schema_rows_are_read_only() {
        let rows = methods();
        let names: Vec<&str> = rows.iter().map(|r| r.0.name).collect();
        assert_eq!(names, vec!["editor.fbx_inspect", "editor.obj_inspect"]);
        for row in &rows {
            assert!(!row.0.mutating);
            assert!(matches!(row.0.permission, Permission::Read));
        }
    }

    #[test]
    fn the_obj_method_reports_inline_text_and_typed_errors() {
        let mut world = World::new();
        let ok = obj_inspect_method(
            &mut world,
            &json!({ "text": "o Quad\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n" }),
        )
        .expect("quad");
        assert_eq!(ok["objects"], 1);
        assert_eq!(ok["triangles"], 2);
        assert_eq!(ok["vertices"], 4);
        let code = |error: &RpcError| {
            error
                .data
                .as_ref()
                .and_then(|data| data.get("obj_error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let cases = [
            (json!({}), "obj_bad_params"),
            (json!({ "text": "v 0 0 0\n", "path": "x.obj" }), "obj_bad_params"),
            (json!({ "text": "v 0 0 0\n", "scale": 0 }), "obj_bad_params"),
            (json!({ "path": "absent-regia-doc-path.obj" }), "obj_io"),
            (json!({ "text": "v 0 0 0\nf 1 2 3\n" }), "obj_parse"),
            (json!({ "text": "v 0 0 0\n" }), "obj_no_mesh"),
        ];
        for (params, expected) in cases {
            let error = obj_inspect_method(&mut world, &params).expect_err("must fail");
            assert_eq!(code(&error).as_deref(), Some(expected), "{params}");
        }
    }
}
