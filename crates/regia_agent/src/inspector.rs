// SPDX-License-Identifier: MPL-2.0
//! `seeing.list_resources` and `seeing.dump_resource`: the GPU resource inspector
//! (book `agents/resource-inspector.md`). The registry and the readback live in
//! [`regia_render::resource_inspector`]; this module is the protocol surface: parameter
//! checks, the output directory and the error codes.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Maps a render-side failure to the protocol error (lowercase `data.code`).
fn rpc_error(error: &regia_render::InspectError) -> RpcError {
    use regia_render::InspectError as E;
    let (code, hint) = match error {
        E::NotFound(_) => (
            protocol::NOT_FOUND,
            "list the labels with seeing.list_resources (a target exists once a frame has rendered it)",
        ),
        E::NotCopyable(_) => (
            protocol::UNSUPPORTED,
            "start the app with the agent plugin (it turns on debug readback before targets are created); \
             Depth24Plus and multisampled targets cannot be copied at all",
        ),
        E::FormatUnsupported(_) => (
            protocol::UNSUPPORTED,
            "float formats are written as exr, 8-bit formats as png; seeing.list_resources shows dump_format",
        ),
        E::OutOfRange(_) => (
            protocol::INVALID_PARAMS,
            "mips and layers are listed by seeing.list_resources",
        ),
        E::Readback(_) => (protocol::INTERNAL_ERROR, "see logs.read for details"),
    };
    RpcError::new(code, error.code(), error.message(), hint)
}

fn resource_json(info: &regia_render::ResourceInfo) -> Value {
    json!({
        "label": info.label,
        "kind": info.kind.name(),
        "format": info.format,
        "width": info.width,
        "height": info.height,
        "layers": info.layers,
        "mips": info.mips,
        "samples": info.samples,
        "bytes": info.bytes,
        "category": info.category.name(),
        "copyable": info.copyable,
        "dump_format": info.dump_format.map(regia_render::DumpFormat::name),
    })
}

/// `seeing.list_resources`: the renderer's registered persistent targets, optionally filtered
/// by a case-insensitive `label` substring or an exact memory `category`.
pub(crate) fn list(params: &Value) -> Result<Value, RpcError> {
    let text = |key: &str| -> Result<Option<String>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a string"))),
        }
    };
    let label = text("label")?.map(|s| s.to_ascii_lowercase());
    let category = text("category")?;
    let resources: Vec<_> = regia_render::list_resources()
        .into_iter()
        .filter(|r| {
            label
                .as_ref()
                .is_none_or(|needle| r.label.to_ascii_lowercase().contains(needle))
        })
        .filter(|r| category.as_ref().is_none_or(|c| r.category.name() == c))
        .collect();
    let total: u64 = resources.iter().map(|r| r.bytes).sum();
    Ok(json!({
        "debug_readback": regia_render::debug_readback(),
        "count": resources.len(),
        "total_bytes": total,
        "resources": resources.iter().map(resource_json).collect::<Vec<_>>(),
    }))
}

/// A file-name-safe form of a label: ASCII letters and digits stay, anything else becomes `_`.
fn file_stem(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn index_param(params: &Value, key: &str) -> Result<u32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

/// `seeing.dump_resource {label, mip?, layer?, format?, out_dir?}`: reads one mip and layer of a
/// registered target back and writes it (tmp + rename) as EXR (float formats) or PNG (8-bit)
/// under `out_dir` (default `<agent dir>/resources`, else the system temp directory).
pub(crate) fn dump(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`label` (string) is required"))?;
    let mip = index_param(params, "mip")?;
    let layer = index_param(params, "layer")?;
    let requested = match params.get("format") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(
            regia_render::DumpFormat::parse(name)
                .ok_or_else(|| RpcError::invalid_params("`format` must be \"exr\" or \"png\""))?,
        ),
        Some(_) => return Err(RpcError::invalid_params("`format` must be a string")),
    };
    let out_dir = match params.get("out_dir") {
        None | Some(Value::Null) => None,
        Some(Value::String(dir)) => Some(crate::audio::confined_out_dir(world, dir)?),
        Some(_) => return Err(RpcError::invalid_params("`out_dir` must be a string")),
    }
    .unwrap_or_else(|| {
        world.get_resource::<crate::AgentEndpoint>().map_or_else(
            || std::env::temp_dir().join("regia-agent-resources"),
            |endpoint| endpoint.dir.join("resources"),
        )
    });
    let Some(device) = world.get_resource::<regia_render::RenderDevice>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no render device yet",
            "open a window or take an offscreen screenshot first (seeing.screenshot target=offscreen)",
        ));
    };
    let image =
        regia_render::dump_resource(device, label, mip, layer, requested).map_err(|e| rpc_error(&e))?;
    let file = format!("{}_m{mip}_l{layer}.{}", file_stem(label), image.format.name());
    let path = crate::audio::write_atomic(&out_dir, &file, &image.bytes)
        .map_err(|e| RpcError::internal(format!("{}: {e}", out_dir.join(&file).display())))?;
    Ok(json!({
        "path": path.display().to_string(),
        "label": label,
        "format": image.format.name(),
        "width": image.width,
        "height": image.height,
        "mip": mip,
        "layer": layer,
        "bytes": image.bytes.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref().and_then(|d| d["code"].as_str())
    }

    #[test]
    fn list_filters_validate_their_types() {
        let err = list(&json!({ "label": 5 })).expect_err("label type");
        assert_eq!(code(&err), Some("F0002"));
        let ok = list(&json!({ "category": "no_such_category" })).expect("filter");
        assert_eq!(ok["count"], 0);
        assert!(ok["debug_readback"].is_boolean());
    }

    #[test]
    fn dump_checks_parameters_before_touching_the_gpu() {
        let mut world = World::new();
        for params in [
            json!({}),
            json!({ "label": 3 }),
            json!({ "label": "x", "mip": -1 }),
            json!({ "label": "x", "layer": "a" }),
            json!({ "label": "x", "format": "tga" }),
            json!({ "label": "x", "format": 1 }),
            json!({ "label": "x", "out_dir": "../escape" }),
        ] {
            let err = dump(&mut world, &params).expect_err("bad params");
            assert_eq!(code(&err), Some("F0002"), "{params}");
        }
        // Valid parameters but no device: the renderer is not up.
        let err = dump(&mut world, &json!({ "label": "x" })).expect_err("no device");
        assert_eq!(code(&err), Some("F0011"));
    }

    #[test]
    fn render_errors_keep_their_lowercase_codes() {
        use regia_render::InspectError as E;
        for (error, wire) in [
            (E::NotFound("m".into()), protocol::NOT_FOUND),
            (E::NotCopyable("m".into()), protocol::UNSUPPORTED),
            (E::FormatUnsupported("m".into()), protocol::UNSUPPORTED),
            (E::OutOfRange("m".into()), protocol::INVALID_PARAMS),
        ] {
            let rpc = rpc_error(&error);
            assert_eq!(rpc.code, wire);
            assert_eq!(code(&rpc), Some(error.code()));
        }
    }

    #[test]
    fn file_stems_are_path_safe() {
        assert_eq!(file_stem("frame hdr"), "frame_hdr");
        assert_eq!(file_stem("../a/b"), "___a_b");
    }
}
