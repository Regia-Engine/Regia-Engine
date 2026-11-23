// SPDX-License-Identifier: MPL-2.0
//! `editor.ply_inspect`, `editor.stl_inspect` and `editor.psd_inspect`: the PLY and STL mesh
//! importers and the PSD image importer on the agent protocol (`book/agents/mesh-image-import.md`,
//! ADR-3244).
//!
//! Each reads a file (by `path`) or its built-in fixture (`sample: "fixture"`) with the importer's
//! settings and returns what a cook would produce. Read-only: nothing is written and nothing in the
//! world changes.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::ply_import::{
    PlyEncoding, PlyImportSettings, convert_ply_to_glb, cube_fixture, read_ply_file,
};
use regia_asset_pipeline::psd_import::{PsdImportSettings, decode_psd, fixture, read_psd_file};
use regia_asset_pipeline::stl_import::{
    StlImportSettings, convert_stl_to_glb, cube_fixture_binary, read_stl_file,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// An `invalid_params` error carrying `data.<key> = code`.
fn coded(key: &str, message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data[key] = json!(code);
    }
    error
}

/// The `settings` RON text, if any.
fn settings_text<'a>(params: &'a Value, key: &str, bad: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get("settings") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(coded(
            key,
            "`settings` must be a RON string (the `.meta` settings text)",
            bad,
        )),
    }
}

/// Exactly one of `path` and `sample: "fixture"`; a `path` goes through the editor's one
/// document-path rule (ADR-7388, audit rc1-sec-paths-z finding 5): only files in the project or the
/// temp dir, `path_outside_project` otherwise.
fn source(
    world: &World,
    params: &Value,
    key: &str,
    bad: &str,
) -> Result<Option<std::path::PathBuf>, RpcError> {
    match (str_param(params, "path"), str_param(params, "sample")) {
        (Some(path), None) => crate::agent_ext::document_request_path(world, "path", path).map(Some),
        (None, Some("fixture")) => Ok(None),
        _ => Err(coded(
            key,
            "give exactly one of `path` (the file) or `sample: \"fixture\"`",
            bad,
        )),
    }
}

fn ply_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let err = |e: regia_asset_pipeline::ply_import::PlyError| coded("ply_error", e.to_string(), e.code());
    let settings =
        PlyImportSettings::try_from_meta_string(settings_text(params, "ply_error", "ply_bad_params")?)
            .map_err(err)?;
    let bytes = match source(world, params, "ply_error", "ply_bad_params")? {
        Some(path) => read_ply_file(&path, &settings.limits).map_err(err)?,
        None => cube_fixture(PlyEncoding::BinaryLittleEndian),
    };
    let conversion = convert_ply_to_glb(&bytes, &settings).map_err(err)?;
    Ok(json!(conversion.report))
}

fn stl_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let err = |e: regia_asset_pipeline::stl_import::StlError| coded("stl_error", e.to_string(), e.code());
    let settings =
        StlImportSettings::try_from_meta_string(settings_text(params, "stl_error", "stl_bad_params")?)
            .map_err(err)?;
    let bytes = match source(world, params, "stl_error", "stl_bad_params")? {
        Some(path) => read_stl_file(&path, &settings.limits).map_err(err)?,
        None => cube_fixture_binary(10.0),
    };
    let conversion = convert_stl_to_glb(&bytes, &settings).map_err(err)?;
    Ok(json!(conversion.report))
}

fn psd_inspect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let err = |e: regia_asset_pipeline::psd_import::PsdError| coded("psd_error", e.to_string(), e.code());
    let settings =
        PsdImportSettings::try_from_meta_string(settings_text(params, "psd_error", "psd_bad_params")?)
            .map_err(err)?;
    let bytes = match source(world, params, "psd_error", "psd_bad_params")? {
        Some(path) => read_psd_file(&path, &settings.limits).map_err(err)?,
        None => fixture::small(),
    };
    let image = decode_psd(&bytes, &settings).map_err(err)?;
    Ok(json!({
        "width": image.width,
        "height": image.height,
        "bits_per_channel": image.depth,
        "grayscale": image.grayscale,
        "has_alpha": image.has_alpha,
        "layers": image.layers,
        "source": image.source,
    }))
}

fn params() -> &'static [ParamSchema] {
    &[
        ParamSchema {
            name: "path",
            kind: "string",
            required: false,
            description: "The file to read.",
        },
        ParamSchema {
            name: "sample",
            kind: "string",
            required: false,
            description: "\"fixture\": a built-in file (a smoke check).",
        },
        ParamSchema {
            name: "settings",
            kind: "string",
            required: false,
            description: "The `.meta` importer settings as RON, e.g. `(scale: 0.01)`.",
        },
    ]
}

/// The PLY, STL and PSD importers' agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.ply_inspect",
                tool: "ply_inspect",
                description: "Run the PLY mesh importer (pure Rust, ADR-3244) on a .ply file with the given importer \
                settings and report what a cook would produce: encoding, vertices, triangles, whether normals, \
                colors and UVs are present, skipped faces. Read-only; writes nothing.",
                params: params(),
                result: "{ encoding, vertices, triangles, has_normals, has_colors, has_uvs, faces_skipped, warnings }",
                mutating: false,
                permission: Permission::Read,
            },
            ply_inspect_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.stl_inspect",
                tool: "stl_inspect",
                description: "Run the STL mesh importer (pure Rust, ADR-3244) on a .stl file with the given importer \
                settings and report what a cook would produce: encoding, triangles and vertices before and after \
                welding, dropped degenerate triangles, recomputed normals. Read-only; writes nothing.",
                params: params(),
                result: "{ encoding, triangles_in, triangles, vertices_in, vertices, welded_positions, \
                degenerate_removed, normals_recomputed, warnings }",
                mutating: false,
                permission: Permission::Read,
            },
            stl_inspect_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.psd_inspect",
                tool: "psd_inspect",
                description: "Run the PSD image importer (pure Rust, ADR-3244) on a .psd file with the given importer \
                settings (`layer`, `alpha`) and report the decoded image: size, bit depth, alpha, layer count and \
                what was decoded. Read-only; writes nothing.",
                params: params(),
                result: "{ width, height, bits_per_channel, grayscale, has_alpha, layers, source }",
                mutating: false,
                permission: Permission::Read,
            },
            psd_inspect_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError, key: &str) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn the_fixtures_report() {
        let mut world = World::new();
        let ply = ply_inspect_method(&mut world, &json!({ "sample": "fixture" })).expect("ply");
        assert_eq!(
            (ply["vertices"].as_u64(), ply["triangles"].as_u64()),
            (Some(8), Some(12))
        );
        let stl = stl_inspect_method(&mut world, &json!({ "sample": "fixture" })).expect("stl");
        assert_eq!(stl["vertices_in"], 36);
        let psd = psd_inspect_method(&mut world, &json!({ "sample": "fixture" })).expect("psd");
        assert_eq!(
            (psd["width"].as_u64(), psd["height"].as_u64()),
            (Some(3), Some(2))
        );
        let layer = psd_inspect_method(
            &mut world,
            &json!({ "sample": "fixture", "settings": "(layer: Some(\"Sprite\"))" }),
        )
        .expect("layer");
        assert_eq!(layer["source"], "Sprite");
    }

    #[test]
    fn errors_carry_documented_codes() {
        let mut world = World::new();
        let ply = ply_inspect_method(&mut world, &json!({})).expect_err("params");
        assert_eq!(code(&ply, "ply_error").as_deref(), Some("ply_bad_params"));
        let ply =
            ply_inspect_method(&mut world, &json!({ "path": "absent-regia-doc-path.ply" })).expect_err("io");
        assert_eq!(code(&ply, "ply_error").as_deref(), Some("ply_io"));
        let stl = stl_inspect_method(
            &mut world,
            &json!({ "sample": "fixture", "settings": "(unit: Parsecs)" }),
        )
        .expect_err("settings");
        assert_eq!(code(&stl, "stl_error").as_deref(), Some("stl_bad_settings"));
        let psd = psd_inspect_method(
            &mut world,
            &json!({ "sample": "fixture", "settings": "(layer: Some(\"Nope\"))" }),
        )
        .expect_err("layer");
        assert_eq!(code(&psd, "psd_error").as_deref(), Some("psd_layer_not_found"));
        let psd =
            psd_inspect_method(&mut world, &json!({ "sample": "fixture", "settings": 3 })).expect_err("type");
        assert_eq!(code(&psd, "psd_error").as_deref(), Some("psd_bad_params"));
    }

    #[test]
    fn the_registered_methods_answer_by_name() {
        let mut world = World::new();
        let table = methods();
        for (name, field) in [
            ("editor.ply_inspect", "vertices"),
            ("editor.stl_inspect", "triangles"),
            ("editor.psd_inspect", "width"),
        ] {
            let (_, handler) = table
                .iter()
                .find(|(schema, _)| schema.name == name)
                .unwrap_or_else(|| panic!("{name} is not registered"));
            let reply = handler(&mut world, &json!({ "sample": "fixture" }))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert!(reply.get(field).is_some(), "{name}: {reply}");
        }
    }

    #[test]
    fn every_method_is_read_only() {
        for (schema, _) in methods() {
            assert!(!schema.mutating, "{}", schema.name);
        }
    }
}
