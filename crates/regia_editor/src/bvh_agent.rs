// SPDX-License-Identifier: MPL-2.0
//! `editor.bvh_inspect`: the BVH motion-capture importer on the agent protocol
//! (`book/agents/bvh-import.md`, ADR-2532).
//!
//! Parses a `.bvh` file (by `path`) or inline text (`text`) with the importer's settings and returns what a
//! cook would produce: joints, frames, duration, tracks, keys before and after reduction, the effective
//! settings. Read-only: nothing is written and nothing in the world changes.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::bvh_import::{BvhImportSettings, FIXTURE_BVH, convert_bvh};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// Largest inline `text` an agent call may send (bytes).
const MAX_TEXT_BYTES: usize = 16 << 20;

/// An `invalid_params` error carrying `data.bvh_error = code`.
fn bvh_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["bvh_error"] = json!(code);
    }
    error
}

/// `editor.bvh_inspect`.
fn bvh_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = match params.get("settings") {
        None | Some(Value::Null) => BvhImportSettings::default(),
        Some(Value::String(text)) => BvhImportSettings::try_from_meta_string(Some(text))
            .map_err(|e| bvh_error(e.to_string(), e.code()))?,
        Some(_) => {
            return Err(bvh_error(
                "`settings` must be a RON string (the `.meta` settings text)",
                "bvh_bad_params",
            ));
        }
    };
    let (bytes, name) = match (
        str_param(params, "path"),
        str_param(params, "text"),
        str_param(params, "sample"),
    ) {
        (Some(given), None, None) => {
            // ADR-7388: the editor's one document-path rule, as `editor.usd_inspect`.
            let resolved = crate::agent_ext::document_request_path(world, "path", given)?;
            let path = resolved.as_path();
            let meta = std::fs::metadata(path).map_err(|e| bvh_error(format!("{given}: {e}"), "bvh_io"))?;
            if meta.len() > settings.limits.max_file_bytes {
                return Err(bvh_error(
                    format!("{given} is {} bytes, more than the limit", meta.len()),
                    "bvh_too_large",
                ));
            }
            let bytes = std::fs::read(path).map_err(|e| bvh_error(format!("{given}: {e}"), "bvh_io"))?;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("clip")
                .to_string();
            (bytes, stem)
        }
        (None, Some(text), None) => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(bvh_error(
                    format!("inline `text` is limited to {MAX_TEXT_BYTES} bytes"),
                    "bvh_bad_params",
                ));
            }
            (text.as_bytes().to_vec(), "inline".to_string())
        }
        (None, None, Some("fixture")) => (FIXTURE_BVH.as_bytes().to_vec(), "fixture".to_string()),
        _ => {
            return Err(bvh_error(
                "give exactly one of `path` (a .bvh file), `text` (inline BVH) or `sample: \"fixture\"`",
                "bvh_bad_params",
            ));
        }
    };
    let import = convert_bvh(&bytes, &name, &settings).map_err(|e| bvh_error(e.to_string(), e.code()))?;
    let roots: Vec<&str> = import
        .skeleton
        .parents
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_none())
        .map(|(i, _)| import.skeleton.joint_names[i].as_str())
        .collect();
    Ok(json!({
        "clip": import.clip.name,
        "joints": import.skeleton.len(),
        "roots": roots,
        "joint_names": import.skeleton.joint_names,
        "duration_seconds": import.clip.duration,
        "sample_rate": import.clip.sample_rate,
        "tracks": import.clip.tracks.len(),
        "keys_before": import.keys_before,
        "keys_after": import.keys_after,
        "root_motion": import.clip.root_motion.map(|r| format!("{:?}", r.mode)),
        "meters_per_unit": settings.meters_per_unit(),
        "retarget": settings.retarget.as_ref().map(|r| r.target.clone()),
    }))
}

/// The BVH importer's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.bvh_inspect",
            tool: "bvh_inspect",
            description: "Run the BVH motion-capture importer (pure Rust, ADR-2532) on a .bvh file or inline BVH \
            text with the given importer settings and report what a cook would produce: joints, roots, frames' \
            duration, tracks, keys before and after reduction, the effective unit scale and root motion mode. \
            Read-only; writes nothing.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "The .bvh file.",
                },
                ParamSchema {
                    name: "text",
                    kind: "string",
                    required: false,
                    description: "Inline BVH text instead of a file (at most 16 MiB).",
                },
                ParamSchema {
                    name: "sample",
                    kind: "string",
                    required: false,
                    description: "\"fixture\": the built-in two-joint file (a smoke check).",
                },
                ParamSchema {
                    name: "settings",
                    kind: "string",
                    required: false,
                    description: "The `.meta` importer settings as RON, e.g. `(unit: Centimeters, root_motion: XZ)`.",
                },
            ],
            result: "{ clip, joints, roots, joint_names, duration_seconds, sample_rate, tracks, keys_before, \
            keys_after, root_motion, meters_per_unit, retarget }",
            mutating: false,
            permission: Permission::Read,
        },
        bvh_inspect_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("bvh_error"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn the_fixture_reports_and_settings_apply() {
        let mut world = World::new();
        let ok = bvh_inspect_method(&mut world, &json!({ "sample": "fixture" })).expect("fixture");
        assert_eq!(ok["joints"], 2);
        assert_eq!(ok["roots"], json!(["Hips"]));
        let ok = bvh_inspect_method(
            &mut world,
            &json!({ "sample": "fixture", "settings": "(unit: Centimeters, root_motion: XZ)" }),
        )
        .expect("settings");
        assert_eq!(ok["root_motion"], "XZ");
        assert!((ok["meters_per_unit"].as_f64().unwrap_or(0.0) - 0.01).abs() < 1e-9);
    }

    #[test]
    fn errors_carry_documented_codes() {
        let mut world = World::new();
        let cases = [
            (json!({}), "bvh_bad_params"),
            (json!({ "sample": "fixture", "text": "x" }), "bvh_bad_params"),
            (json!({ "sample": "fixture", "settings": 3 }), "bvh_bad_params"),
            (
                json!({ "sample": "fixture", "settings": "(unit_scale: 0.0)" }),
                "bvh_bad_settings",
            ),
            (json!({ "path": "absent-regia-doc-path.bvh" }), "bvh_io"),
            (
                json!({ "text": "HIERARCHY\nROOT A\n{\nOFFSET 0 0\n" }),
                "bvh_syntax",
            ),
            (
                json!({ "text": "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 2 Xposition Xposition\n}\nMOTION\n" }),
                "bvh_channels",
            ),
        ];
        for (params, expected) in cases {
            let error = bvh_inspect_method(&mut world, &params).expect_err("must fail");
            assert_eq!(code(&error).as_deref(), Some(expected), "{params}");
        }
    }

    #[test]
    fn the_schema_row_is_read_only() {
        let rows = methods();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "editor.bvh_inspect");
        assert!(!rows[0].0.mutating);
    }
}
