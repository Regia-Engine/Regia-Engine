// SPDX-License-Identifier: MPL-2.0
//! `editor.svg_report`: the SVG importer on the agent protocol (`book/agents/svg-import.md`, ADR-3350).
//!
//! Reads an `.svg` file (by `path`) or inline text (`text`) through the importer's checks and reports what a cook would
//! do with it: the document's size, its node count, the raster sizes the `.meta` settings ask for and the UI scales an
//! atlas sprite is drawn at. A refused document is an `invalid_params` error whose `data.svg_error` is the importer's
//! code. Read-only: nothing is written and nothing in the world changes.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::svg_import::{SVG_MAX_BYTES, SvgSettings, report};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// An `invalid_params` error carrying `data.svg_error = code`.
fn svg_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["svg_error"] = json!(code);
    }
    error
}

/// `editor.svg_report`.
fn svg_report_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = match params.get("settings") {
        None | Some(Value::Null) => SvgSettings::default(),
        Some(Value::String(text)) => {
            SvgSettings::from_meta_string(Some(text)).map_err(|e| svg_error(e.to_string(), e.code()))?
        }
        Some(_) => {
            return Err(svg_error(
                "`settings` must be a RON string (the `.meta` settings text)",
                "svg_bad_params",
            ));
        }
    };
    let (label, bytes) = match (str_param(params, "path"), str_param(params, "text")) {
        (Some(given), None) => {
            // ADR-7388: the editor's one document-path rule, as `editor.usd_inspect`.
            let resolved = crate::agent_ext::document_request_path(world, "path", given)?;
            let path = resolved.as_path();
            let meta = std::fs::metadata(path).map_err(|e| svg_error(format!("{given}: {e}"), "svg_io"))?;
            if meta.len() > SVG_MAX_BYTES as u64 {
                return Err(svg_error(
                    format!("{given} is {} bytes, the limit is {SVG_MAX_BYTES}", meta.len()),
                    "svg_too_large",
                ));
            }
            let bytes = std::fs::read(path).map_err(|e| svg_error(format!("{given}: {e}"), "svg_io"))?;
            (given.to_string(), bytes)
        }
        (None, Some(text)) => ("inline".to_string(), text.as_bytes().to_vec()),
        _ => {
            return Err(svg_error(
                "give exactly one of `path` (an .svg file) or `text` (inline SVG)",
                "svg_bad_params",
            ));
        }
    };
    let r = report(&label, &bytes, &settings);
    if let (Some(code), Some(message)) = (&r.error, &r.message) {
        return Err(svg_error(message.clone(), code));
    }
    serde_json::to_value(&r).map_err(|e| svg_error(e.to_string(), "svg_bad_params"))
}

/// The SVG importer's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.svg_report",
            tool: "svg_report",
            description: "Run the SVG importer's checks (ADR-3350) on an .svg file or inline SVG text and report what a \
            cook would do: document bytes and XML nodes, the document's own size, the raster sizes the .meta settings \
            (`sizes_px` or `scale`) ask for, the cooked texture size and the UI scales an atlas sprite is rasterized \
            at. A refused document (external reference, script, text, image, over a limit) is an error whose \
            `data.svg_error` is a stable code. Read-only; writes nothing.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "The .svg file.",
                },
                ParamSchema {
                    name: "text",
                    kind: "string",
                    required: false,
                    description: "Inline SVG text instead of a file (at most 1 MiB).",
                },
                ParamSchema {
                    name: "settings",
                    kind: "string",
                    required: false,
                    description: "The `.meta` importer settings as RON, e.g. `(sizes_px: [32, 64])` or `(scale: Some(2.0))`.",
                },
            ],
            result: "{ path, bytes, nodes, intrinsic_size, raster_sizes, cooked_size, atlas_scales, error, message }",
            mutating: false,
            permission: Permission::Read,
        },
        svg_report_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="4" height="4"/></svg>"##;

    fn code(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("svg_error"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn a_document_reports_its_sizes() {
        let mut world = World::new();
        let ok = svg_report_method(
            &mut world,
            &json!({ "text": ICON, "settings": "(sizes_px: [16, 32])" }),
        )
        .expect("reports");
        assert_eq!(ok["intrinsic_size"], json!([8.0, 8.0]));
        assert_eq!(ok["raster_sizes"], json!([[32, 32], [16, 16]]));
        assert_eq!(ok["cooked_size"], json!([32, 32]));
        assert_eq!(ok["atlas_scales"], json!([1, 2]));
        let ok = svg_report_method(&mut world, &json!({ "text": ICON })).expect("defaults");
        assert_eq!(ok["cooked_size"], json!([8, 8]));
    }

    #[test]
    fn errors_carry_documented_codes() {
        let mut world = World::new();
        let cases = [
            (json!({}), "svg_bad_params"),
            (json!({ "text": ICON, "path": "a.svg" }), "svg_bad_params"),
            (json!({ "text": ICON, "settings": 3 }), "svg_bad_params"),
            (
                json!({ "text": ICON, "settings": "(sizes_px: oops" }),
                "svg_settings",
            ),
            (
                json!({ "text": ICON, "settings": "(sizes_px: [0])" }),
                "svg_settings",
            ),
            (
                json!({ "text": ICON, "settings": "(sizes_px: [100000])" }),
                "svg_raster_too_large",
            ),
            (json!({ "path": "absent-regia-doc-path.svg" }), "svg_io"),
            (json!({ "text": "<svg" }), "svg_xml"),
            (
                json!({ "text": r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><script/></svg>"## }),
                "svg_script",
            ),
            (
                json!({ "text": r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><a href="http://x.example/"/></svg>"## }),
                "svg_external_ref",
            ),
        ];
        for (params, expected) in cases {
            let error = svg_report_method(&mut world, &params).expect_err("refused");
            assert_eq!(code(&error).as_deref(), Some(expected), "{params}: {error:?}");
        }
    }

    #[test]
    fn the_method_is_registered_read_only() {
        let methods = methods();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].0.name, "editor.svg_report");
        assert!(!methods[0].0.mutating);
    }
}
