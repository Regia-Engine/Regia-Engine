// SPDX-License-Identifier: MPL-2.0
//! `editor.usd_inspect`: the USD importer on the agent protocol (`book/agents/fbx-import.md` § USD,
//! ADR-3235).
//!
//! Converts a `.usda`, `.usdc`, `.usd` or `.usdz` file (by `path`) or inline `.usda` text (`text`)
//! with [`regia_asset_pipeline::usd_import::convert_usd_to_glb`] and returns the import report:
//! what a cook of the file would produce, the layers read, the stage's unit and up axis, variant
//! selections, textures and warnings. Read-only: nothing is written and nothing in the world
//! changes.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::usd_import::{UsdImportSettings, UsdSource, convert_usd_to_glb};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// Largest inline `text` an agent call may send (bytes).
const MAX_TEXT_BYTES: usize = 16 << 20;

/// An `invalid_params` error carrying `data.usd_error = code`.
fn usd_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["usd_error"] = json!(code);
    }
    error
}

fn settings_from_params(params: &Value) -> Result<UsdImportSettings, RpcError> {
    let mut settings = UsdImportSettings::default();
    if let Some(scale) = params.get("scale") {
        settings.scale = scale
            .as_f64()
            .filter(|s| s.is_finite() && *s > 0.0)
            .ok_or_else(|| usd_error("`scale` must be a positive number", "usd_bad_params"))?
            as f32;
    }
    if let Some(unit) = params.get("meters_per_unit") {
        settings.meters_per_unit = Some(
            unit.as_f64()
                .filter(|s| s.is_finite() && *s > 0.0)
                .ok_or_else(|| usd_error("`meters_per_unit` must be a positive number", "usd_bad_params"))?,
        );
    }
    if let Some(axis) = params.get("up_axis") {
        match axis.as_str() {
            Some(axis @ ("Y" | "Z")) => settings.up_axis = Some(axis.to_string()),
            _ => return Err(usd_error("`up_axis` must be \"Y\" or \"Z\"", "usd_bad_params")),
        }
    }
    Ok(settings)
}

/// `editor.usd_inspect`.
fn usd_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = settings_from_params(params)?;
    let conversion = match (str_param(params, "path"), str_param(params, "text")) {
        (Some(path), None) => {
            // ADR-7388 (audit rc1-sec-paths-z finding 4): only files in the project or the temp dir.
            let path = crate::agent_ext::document_request_path(world, "path", path)?;
            convert_usd_to_glb(UsdSource::File(&path), &settings)
        }
        (None, Some(text)) => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(usd_error(
                    format!("inline `text` is limited to {MAX_TEXT_BYTES} bytes"),
                    "usd_bad_params",
                ));
            }
            convert_usd_to_glb(
                UsdSource::Bytes {
                    name: "inline.usda",
                    bytes: text.as_bytes(),
                },
                &settings,
            )
        }
        _ => {
            return Err(usd_error(
                "give exactly one of `path` (a .usda/.usdc/.usd/.usdz file) or `text` (inline .usda)",
                "usd_bad_params",
            ));
        }
    }
    .map_err(|e| usd_error(e.to_string(), e.code()))?;
    let mut out = serde_json::to_value(&conversion.report)
        .map_err(|e| RpcError::internal(format!("report serialization: {e}")))?;
    out["glb_bytes"] = json!(conversion.glb.len());
    Ok(out)
}

/// The USD importer's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.usd_inspect",
            tool: "usd_inspect",
            description: "Run the USD importer (pure-Rust reader, ADR-3235) on a .usda, .usdc, .usd or .usdz file \
            or inline .usda text and report what a cook would produce: layers read, unit and up axis, prims, \
            nodes, meshes, instanced meshes, vertices, triangles, materials, textures, cameras, lights, variant \
            selections, warnings. Read-only; writes nothing.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "The USD file (sublayers, references, payloads and textures are read inside its \
                    project, or its folder outside a project).",
                },
                ParamSchema {
                    name: "text",
                    kind: "string",
                    required: false,
                    description: "Inline .usda text instead of a file (at most 16 MiB; external files are reported \
                    missing).",
                },
                ParamSchema {
                    name: "scale",
                    kind: "number",
                    required: false,
                    description: "Uniform scale after the unit conversion to meters (default 1).",
                },
                ParamSchema {
                    name: "meters_per_unit",
                    kind: "number",
                    required: false,
                    description: "Override the stage's metersPerUnit (USD's fallback is 0.01).",
                },
                ParamSchema {
                    name: "up_axis",
                    kind: "string",
                    required: false,
                    description: "Override the stage's upAxis: \"Y\" or \"Z\".",
                },
            ],
            result: "{ format, layers, meters_per_unit, up_axis, scale, prims, nodes, meshes, instanced_meshes, \
            vertices, triangles, materials, textures, cameras, lights, variant_selections, warnings, glb_bytes }",
            mutating: false,
            permission: Permission::Read,
        },
        usd_inspect_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRI: &str = "#usda 1.0\n(\n    metersPerUnit = 1\n    upAxis = \"Z\"\n)\ndef Mesh \"Tri\"\n{\n    \
                       int[] faceVertexCounts = [3]\n    int[] faceVertexIndices = [0, 1, 2]\n    \
                       point3f[] points = [(0, 0, 0), (1, 0, 0), (0, 1, 0)]\n}\n";

    fn code(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("usd_error"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn the_agent_method_reports_inline_text_and_files() {
        let mut world = World::new();
        let inline = usd_inspect_method(&mut world, &json!({ "text": TRI })).expect("inline");
        assert_eq!(inline["format"], "usda");
        assert_eq!(inline["meshes"], 1);
        assert_eq!(inline["triangles"], 1);
        assert_eq!(inline["up_axis"], "Z");
        assert!(inline["glb_bytes"].as_u64().is_some_and(|n| n > 0));
        let overridden = usd_inspect_method(
            &mut world,
            &json!({ "text": TRI, "up_axis": "Y", "meters_per_unit": 0.01 }),
        )
        .expect("override");
        assert_eq!(overridden["up_axis"], "Y");
        assert_eq!(overridden["meters_per_unit"], 0.01);

        let dir = regia_test_utils::unique_temp_dir("usd-agent");
        let path = dir.join("tri.usda");
        std::fs::write(&path, TRI).expect("write");
        let file = usd_inspect_method(&mut world, &json!({ "path": path.to_string_lossy() })).expect("path");
        assert_eq!(file["layers"], json!(["tri.usda"]));
        let again =
            usd_inspect_method(&mut world, &json!({ "path": path.to_string_lossy() })).expect("again");
        assert_eq!(file, again, "same call, same answer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agent_method_reports_typed_error_codes() {
        let mut world = World::new();
        let cases = [
            (json!({}), "usd_bad_params"),
            (json!({ "text": TRI, "path": "x.usda" }), "usd_bad_params"),
            (json!({ "text": TRI, "scale": 0 }), "usd_bad_params"),
            (json!({ "text": TRI, "up_axis": "X" }), "usd_bad_params"),
            (json!({ "text": TRI, "meters_per_unit": -1 }), "usd_bad_params"),
            (json!({ "path": "absent-regia-doc-path.usda" }), "usd_io"),
            (json!({ "text": "plain words" }), "usd_not_usd"),
            (json!({ "text": "#usda 1.0\ndef Xform \"A\" {" }), "usd_parse"),
            (
                json!({ "text": "#usda 1.0\ndef Xform \"A\" {}\n" }),
                "usd_no_mesh",
            ),
        ];
        for (params, expected) in cases {
            let error = usd_inspect_method(&mut world, &params).expect_err("must fail");
            assert_eq!(code(&error).as_deref(), Some(expected), "{params}");
            assert_eq!(error.code, regia_agent::protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn the_schema_row_is_read_only() {
        let rows = methods();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "editor.usd_inspect");
        assert!(!rows[0].0.mutating);
        assert!(matches!(rows[0].0.permission, Permission::Read));
    }
}
