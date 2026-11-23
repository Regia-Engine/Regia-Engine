// SPDX-License-Identifier: MPL-2.0
//! `looks.*`: the product viewer's cheap looks from the agent protocol (`book/agents/product-looks.md`,
//! ADR-2538).
//!
//! The data methods (`looks.list`, `looks.material`, `looks.cut`) answer from the built-in stand-in
//! material library and cuts and touch no GPU. `looks.render_board` renders the look board through the
//! product viewer's renderer (`regia_viewer`) with the looks hooks, on its own headless device (not the
//! game's frame), and answers the image hash; it may write a PNG. Refusals are `F0002` with
//! `data.reason` starting `looks_`; a machine without any adapter answers `F0003` with
//! `looks_no_adapter`.

use regia_app::prelude::World;
use regia_looks::Library;
use regia_viewer::gpu::sample_count_for;
use regia_viewer::{ViewerError, capture_still};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// The largest still `looks.render_board` makes, per side.
const MAX_SIDE: u64 = 4096;

fn refusal(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": reason,
            "hint": "see book/agents/product-looks.md",
        })),
    }
}

fn library() -> Result<Library, RpcError> {
    Library::builtin().map_err(|e| refusal("looks_library_parse", e.to_string()))
}

/// One data method, answered by `regia_looks::agent` (its codes become `looks_<code>` reasons).
fn data_method(method: &str, params: &Value) -> Result<Value, RpcError> {
    let lib = library()?;
    regia_looks::agent::handle(&lib, method, params)
        .map_err(|e| refusal(&format!("looks_{}", e.code), e.message))
}

/// `looks.list {}`: every material of the built-in library, the stand-in cuts and the look kinds.
pub fn list(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    data_method("looks.list", params)
}

/// `looks.material { material_id, finish?, faceted? }`: the look a library entry draws with.
pub fn material(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    data_method("looks.material", params)
}

/// `looks.cut { cut_type, width_mm? }`: a stand-in cut's facets, size and triangle count.
pub fn cut(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    data_method("looks.cut", params)
}

fn side(params: &Value, key: &str, fallback: u32) -> Result<u32, RpcError> {
    match params.get(key) {
        None => Ok(fallback),
        Some(v) => v
            .as_u64()
            .filter(|s| (1..=MAX_SIDE).contains(s))
            .map(|s| s as u32)
            .ok_or_else(|| {
                refusal(
                    "looks_bad_size",
                    format!("`{key}` must be a whole number from 1 to {MAX_SIDE}"),
                )
            }),
    }
}

fn finite(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|v| v as f32)
            .filter(|v| v.is_finite())
            .map(Some)
            .ok_or_else(|| refusal("looks_bad_value", format!("`{key}` must be a finite number"))),
    }
}

fn render_failed(e: &ViewerError) -> RpcError {
    refusal("looks_render_failed", e.to_string())
}

/// `looks.render_board { width?, height?, tilt_deg?, close_up?: { x_mm, y_mm, fov_deg }, path? }`: the
/// look board through the viewer's forward pass with the looks hooks.
pub fn render_board(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("looks_render_board").entered();
    if let Some(map) = params.as_object()
        && let Some(unknown) = map
            .keys()
            .find(|k| !["width", "height", "tilt_deg", "close_up", "path"].contains(&k.as_str()))
    {
        return Err(refusal(
            "looks_unknown_field",
            format!("unknown field `{unknown}`"),
        ));
    }
    let width = side(params, "width", 960)?;
    let height = side(params, "height", 600)?;
    let tilt = finite(params, "tilt_deg")?.unwrap_or(22.0);
    if !(-60.0..=60.0).contains(&tilt) {
        return Err(refusal("looks_bad_value", "`tilt_deg` must be from -60 to 60"));
    }
    let close_up = match params.get("close_up") {
        None => None,
        Some(c) => {
            if !c.is_object() {
                return Err(refusal("looks_bad_value", "`close_up` must be an object"));
            }
            let x = finite(c, "x_mm")?.unwrap_or(0.0);
            let y = finite(c, "y_mm")?.unwrap_or(0.0);
            let fov = finite(c, "fov_deg")?.unwrap_or(7.0);
            if !(0.5..=90.0).contains(&fov) {
                return Err(refusal(
                    "looks_bad_value",
                    "`close_up.fov_deg` must be from 0.5 to 90",
                ));
            }
            Some((x, y, fov))
        }
    };
    // A refused path renders nothing.
    let out = params
        .get("path")
        .map(|v| {
            let path = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`path` is a string"))?;
            crate::req_path::allowed_path(world, "path", path)
        })
        .transpose()?;
    let board =
        regia_looks::board::look_board(tilt).map_err(|e| refusal("looks_render_failed", e.to_string()))?;
    // A host that opened no GPU on purpose keeps it that way (ADR-5180).
    crate::seeing::GpuDisabled::refuse(world)?;
    let (adapter, device, queue) = crate::seeing::viewer_device().map_err(|e| match e {
        ViewerError::NoAdapter(_) => {
            let mut error = RpcError::new(
                NOT_FOUND,
                "F0003",
                e.to_string(),
                "the looks render on a GPU adapter; on a headless machine install the lavapipe driver",
            );
            if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                data.insert("reason".into(), json!("looks_no_adapter"));
            }
            error
        }
        other => render_failed(&other),
    })?;
    let info = adapter.get_info();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let samples = sample_count_for(&adapter, format);
    let mut viewer =
        regia_looks::viewer::looks_renderer(device, queue, format, samples, &regia_looks::Studio::default())
            .map_err(|e| render_failed(&e))?;
    regia_looks::viewer::draw_scene(&mut viewer, &board, close_up).map_err(|e| render_failed(&e))?;
    let still = capture_still(&mut viewer, width, height).map_err(|e| render_failed(&e))?;
    let mut bytes = Value::Null;
    if let Some(path) = &out {
        let png = still.encode_png().map_err(|e| render_failed(&e))?;
        crate::viewer::write_atomic(path, &png).map_err(|e| {
            refusal(
                "looks_write_failed",
                format!("cannot write `{}`: {e}", path.display()),
            )
        })?;
        bytes = json!(png.len());
    }
    let stats = viewer.stats();
    Ok(json!({
        "width": width,
        "height": height,
        "hash": format!("{:016x}", still.hash()),
        "items": board.items.len(),
        "adapter": { "name": info.name, "backend": format!("{:?}", info.backend) },
        "sample_count": samples,
        "stats": {
            "pipelines_created": stats.pipelines_created,
            "draw_calls": stats.draw_calls,
            "triangles_drawn": stats.triangles_drawn,
            "material_writes": stats.material_writes,
        },
        "path": out.map(|p| p.display().to_string()),
        "bytes": bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("reason")?.as_str()
    }

    #[test]
    fn data_methods_answer_from_the_builtin_library() {
        let mut world = World::new();
        let listed = list(&mut world, &json!({})).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(listed["materials"].as_array().is_some_and(|m| m.len() > 20));
        let gem = material(&mut world, &json!({ "material_id": "diamond-white" }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(gem["kind"], "gem");
        let round = cut(
            &mut world,
            &json!({ "cut_type": "round_brilliant", "width_mm": 2.0 }),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(round["facets"]["crown"], 32);
    }

    #[test]
    fn refusals_carry_looks_reasons() {
        let mut world = World::new();
        let e = material(&mut world, &json!({ "material_id": "nope" })).expect_err("unknown");
        assert_eq!(reason(&e), Some("looks_material_unknown"));
        let e = material(&mut world, &json!({})).expect_err("missing");
        assert_eq!(reason(&e), Some("looks_missing_param"));
        let e = cut(&mut world, &json!({ "cut_type": "rose" })).expect_err("cut");
        assert_eq!(reason(&e), Some("looks_unknown_cut"));
        let e = cut(&mut world, &json!({ "cut_type": "pear", "width_mm": 99.0 })).expect_err("size");
        assert_eq!(reason(&e), Some("looks_bad_size"));
        for (params, expected) in [
            (json!({ "width": 0 }), "looks_bad_size"),
            (json!({ "tilt_deg": 80.0 }), "looks_bad_value"),
            (json!({ "close_up": { "fov_deg": 0.0 } }), "looks_bad_value"),
            (json!({ "close_up": 5 }), "looks_bad_value"),
            (json!({ "nonsense": 1 }), "looks_unknown_field"),
        ] {
            let e = render_board(&mut world, &params).expect_err("refused");
            assert_eq!(reason(&e), Some(expected), "{params}");
        }
        let e = render_board(&mut world, &json!({ "path": "../board.png" })).expect_err("path");
        assert_eq!(reason(&e), Some("path_outside_project"));
    }

    #[test]
    fn the_board_renders_with_a_stable_hash() {
        let mut world = World::new();
        let params = json!({ "width": 96, "height": 60 });
        let first = match render_board(&mut world, &params) {
            Ok(result) => result,
            Err(error) => {
                assert_eq!(reason(&error), Some("looks_no_adapter"), "{error:?}");
                return;
            }
        };
        let second = render_board(&mut world, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(first["hash"], second["hash"]);
        assert_eq!(first["stats"]["pipelines_created"], 11);
        assert!(first["items"].as_u64().is_some_and(|n| n > 30));
    }

    #[test]
    fn the_board_png_lands_in_a_new_folder() {
        let mut world = World::new();
        let dir = regia_test_utils::unique_temp_dir("looks-board-png");
        let path = dir.join("new").join("folder").join("board.png");
        let params = json!({ "width": 32, "height": 20, "path": path.to_string_lossy() });
        match render_board(&mut world, &params) {
            Ok(result) => {
                let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{e}"));
                assert_eq!(result["bytes"], bytes.len());
                assert_eq!(&bytes[1..4], b"PNG");
                let left: Vec<_> = std::fs::read_dir(path.parent().unwrap_or(&dir))
                    .unwrap_or_else(|e| panic!("{e}"))
                    .flatten()
                    .map(|entry| entry.file_name())
                    .collect();
                assert_eq!(left.len(), 1, "a temporary file stayed: {left:?}");
            }
            Err(error) => assert_eq!(reason(&error), Some("looks_no_adapter"), "{error:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn headless_world() -> World {
        let mut world = World::new();
        world.insert_resource(crate::seeing::GpuDisabled {
            code: "editor_headless".into(),
            reason: "the headless editor has no GPU adapter (started with --no-gpu)".into(),
            hint: "start it without --no-gpu".into(),
        });
        world
    }

    fn data_code(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    /// ADR-5180: a host that opened no GPU on purpose refuses the board with its own code.
    #[test]
    fn a_host_without_a_gpu_refuses_the_board() {
        let mut world = headless_world();
        let error = render_board(&mut world, &json!({ "width": 96, "height": 60 })).expect_err("refused");
        assert_eq!(data_code(&error), Some("editor_headless"), "{error:?}");
    }
}
