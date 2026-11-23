// SPDX-License-Identifier: MPL-2.0
//! `render.set_reference_overlay` (spec 27 § 5, ADR-2463, book `agents/plates.md`): draws a
//! reference image over the frame for look reviews, through the renderer's `reference_overlay`
//! pass at the `overlay` slot.
//!
//! The image is read from a PNG under the project or an allowed output root, decoded once and
//! cached by path, size and modification time; a later call with the same file only changes the
//! mode, opacity or split. Every error carries a lowercase `data.reason` (documented on the book
//! page): `unknown_mode`, `bad_opacity`, `bad_split`, `path_required`, `path_outside_project`,
//! `image_not_found`, `image_unreadable`, `image_too_large` and `no_renderer`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use regia_app::prelude::*;
use regia_render::runtime::RuntimeRenderer;
use regia_render::{ReferenceImage, ReferenceOverlayHandle, ReferenceOverlayMode, ReferenceOverlaySettings};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Largest reference file the agent reads, bytes (a 4096 x 4096 RGBA PNG is far below it).
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// An error with a lowercase `reason` next to its `F` code.
pub(crate) fn reasoned(
    rpc_code: i64,
    f_code: &str,
    reason: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> RpcError {
    let mut error = RpcError::new(rpc_code, f_code, message, hint);
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn bad_params(reason: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    reasoned(protocol::INVALID_PARAMS, "F0002", reason, message, hint)
}

/// A loaded file's identity: path, size and modification time.
type FileKey = (PathBuf, u64, Option<SystemTime>);

/// The overlay's host state: the shared handle and what was loaded.
#[derive(Resource, Default)]
pub(crate) struct ReferenceOverlayState {
    handle: ReferenceOverlayHandle,
    /// The pass is registered on the runtime renderer.
    registered: bool,
    /// The file the cached image came from, with its size and modification time.
    loaded: Option<FileKey>,
    /// Files decoded so far (a repeat call with the same file does not add to it).
    loads: u64,
}

/// `render.set_reference_overlay { mode, path?, opacity?, split? }`.
pub(crate) fn set_reference_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode_name = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
        bad_params(
            "unknown_mode",
            "`mode` (string) is required",
            "mode is off, blend, wipe, difference or flip",
        )
    })?;
    let mode = ReferenceOverlayMode::parse(mode_name).ok_or_else(|| {
        bad_params(
            "unknown_mode",
            format!("unknown mode {mode_name:?}"),
            "mode is off, blend, wipe, difference or flip (lowercase)",
        )
    })?;
    let unit = |key: &str, reason: &str, default: f32| -> Result<f32, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_f64()
                .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
                .map(|f| f as f32)
                .ok_or_else(|| {
                    bad_params(
                        reason,
                        format!("`{key}` must be a number in 0..=1"),
                        "give a fraction",
                    )
                }),
        }
    };
    // `blend` defaults to half opacity so both images show; the other modes to full strength.
    let opacity = unit(
        "opacity",
        "bad_opacity",
        if mode == ReferenceOverlayMode::Blend {
            0.5
        } else {
            1.0
        },
    )?;
    let split = unit("split", "bad_split", 0.5)?;

    if world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(reasoned(
            protocol::UNSUPPORTED,
            "F0011",
            "no_renderer",
            "no renderer in this app",
            "add regia_render::RenderPlugin",
        ));
    }
    let path_text = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| {
            bad_params(
                "path_required",
                "`path` must be a string",
                "a project-relative PNG path",
            )
        })?),
    };
    let image = match path_text {
        Some(text) => Some(load_image(world, text)?),
        None => None,
    };
    let has_image = image.is_some()
        || world
            .get_resource::<ReferenceOverlayState>()
            .is_some_and(|s| s.handle.image().is_some());
    if mode != ReferenceOverlayMode::Off && !has_image {
        return Err(bad_params(
            "path_required",
            "this mode needs a reference image and none is loaded",
            "pass `path` (a PNG) with the first call",
        ));
    }

    let (handle, register) = {
        let state = world.get_resource_or_insert_with(ReferenceOverlayState::default);
        (state.handle.clone(), !state.registered)
    };
    if let Some((image, key)) = image {
        // A different file replaces the cached image; the same file keeps it.
        let mut state = world.resource_mut::<ReferenceOverlayState>();
        if state.loaded.as_ref() != Some(&key) {
            handle.set_image(Some(image));
            state.loaded = Some(key);
            state.loads += 1;
        }
    }
    if register {
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .enable_reference_overlay(&handle);
        world.resource_mut::<ReferenceOverlayState>().registered = true;
    }
    handle.set_settings(ReferenceOverlaySettings { mode, opacity, split });

    let state = world.resource::<ReferenceOverlayState>();
    let stats = handle.stats();
    Ok(json!({
        "mode": mode.as_str(),
        "opacity": opacity,
        "split": split,
        "image": handle.image().map(|i| json!({ "width": i.width, "height": i.height })),
        "loaded_from": state.loaded.as_ref().map(|(p, _, _)| p.file_name().map(|n| n.to_string_lossy().into_owned())),
        "loads": state.loads,
        "draws": stats.draws,
        "uploads": stats.uploads,
        "failures": stats.failures,
        "fit": "an aspect within 1 % of the frame covers it; any other is letterboxed with the frame showing around it",
    }))
}

/// Reads and decodes the PNG at `text` (confined like `render.out_start`'s `out`), unless the
/// cached image came from the same file with the same size and modification time.
fn load_image(world: &mut World, text: &str) -> Result<(Arc<ReferenceImage>, FileKey), RpcError> {
    let path = crate::render_out::confine_path(world, "path", std::path::Path::new(text))?;
    let meta = std::fs::metadata(&path).map_err(|e| {
        reasoned(
            protocol::NOT_FOUND,
            "F0003",
            "image_not_found",
            format!("cannot read `path`: {e}"),
            "give a PNG under the project or an allowed output root",
        )
    })?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(reasoned(
            protocol::INVALID_PARAMS,
            "F0002",
            "image_too_large",
            "`path` is not a regular file, or is over 128 MiB",
            "give a PNG of at most 4096 x 4096 pixels",
        ));
    }
    let key = (path.clone(), meta.len(), meta.modified().ok());
    if let Some(state) = world.get_resource::<ReferenceOverlayState>()
        && state.loaded.as_ref() == Some(&key)
        && let Some(image) = state.handle.image()
    {
        return Ok((image, key));
    }
    let bytes = std::fs::read(&path).map_err(|e| {
        reasoned(
            protocol::NOT_FOUND,
            "F0003",
            "image_not_found",
            format!("cannot read `path`: {e}"),
            "give a PNG under the project or an allowed output root",
        )
    })?;
    let decoded =
        crate::image::decode_png_rgba_within(&bytes, crate::image::MAX_INPUT_SIDE).map_err(|e| {
            let too_large = e.contains("each side must be");
            reasoned(
                protocol::INVALID_PARAMS,
                "F0002",
                if too_large {
                    "image_too_large"
                } else {
                    "image_unreadable"
                },
                format!("`path` is not a usable PNG: {e}"),
                "give an 8-bit PNG of at most 4096 x 4096 pixels",
            )
        })?;
    let image = ReferenceImage::new(decoded.width, decoded.height, decoded.rgba).map_err(|e| {
        reasoned(
            protocol::INVALID_PARAMS,
            "F0002",
            "image_unreadable",
            e,
            "give a valid PNG",
        )
    })?;
    Ok((Arc::new(image), key))
}
