// SPDX-License-Identifier: MPL-2.0
//! Local reflection captures as level content (ADR-2860, `book/agents/reflection-captures.md`):
//! `render.save_local_captures` writes the renderer's baked captures to a `.fcap` and names it in
//! the open level, `render.load_local_captures` installs a `.fcap`'s maps with no face rendered,
//! and `render.get_capture_bake` reports the level's bake, the selection and the tier budget.

use regia_app::prelude::*;
use regia_asset::local_capture_bake::{FcapCapture, FcapError, FcapFile};
use regia_framework::local_capture_level::{LevelCapturesState, bakes_of};
use regia_render::{
    LOCAL_CAPTURE_FADE_FRAMES, LOCAL_CAPTURE_HYSTERESIS_M, LOCAL_CAPTURE_LAYERS, LocalCaptureBakes,
    LocalCaptureInfo, MAX_LOCAL_CAPTURES, RuntimeRenderer, local_capture_budget,
};
use regia_scene::level::{LevelCaptures, LevelSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const HINT: &str = "see book/agents/reflection-captures.md";

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
         FrameRenderer::local_captures and regia_asset::local_capture_bake directly)",
    )
}

/// A refused call: `invalid_params` with a reason code in `data.reason`.
fn refused(reason: &str, message: &str) -> RpcError {
    RpcError {
        code: crate::protocol::INVALID_PARAMS,
        message: message.to_string(),
        data: Some(json!({ "code": "F0002", "reason": reason, "hint": HINT })),
    }
}

fn fcap_refused(path: &str, err: &FcapError) -> RpcError {
    refused(err.code(), &format!("{path}: {err}"))
}

/// The `.fcap` of the renderer's baked captures, in slot order.
#[must_use]
pub fn fcap_of(infos: &[LocalCaptureInfo]) -> FcapFile {
    let captures = infos
        .iter()
        .filter_map(|info| {
            let bake = info.desc.bake.as_ref()?;
            Some(FcapCapture {
                id: info.desc.id,
                center: info.desc.center,
                half_extent: info.desc.half_extent,
                weight: info.desc.weight,
                fade: info.desc.fade,
                coverage: bake.coverage,
                mips: bake.mips.clone(),
            })
        })
        .collect();
    FcapFile {
        width: regia_render::LOCAL_CAPTURE_WIDTH,
        captures,
    }
}

fn file_param(params: &Value) -> Result<String, RpcError> {
    let file = params
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .ok_or_else(|| {
            refused(
                "bad_file",
                "pass `file`, an asset path ending in .fcap (maps/hall.fcap)",
            )
        })?;
    if !file.ends_with(".fcap") {
        return Err(refused("bad_file", &format!("`{file}` must end in .fcap")));
    }
    Ok(file.to_string())
}

/// `render.save_local_captures { file, name_in_level? }`: writes every baked capture to
/// `assets/<file>` and, unless `name_in_level` is false, names it in the open level's
/// `look.captures` (saved with the level).
pub(crate) fn save_captures(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let file = file_param(params)?;
    // A wrong-typed `name_in_level` is refused, not read as true (ADR-2622).
    let name_in_level = crate::params::opt_bool(params, "name_in_level")?.unwrap_or(true);
    let infos = world
        .get_resource::<RuntimeRenderer>()
        .ok_or_else(no_renderer)?
        .0
        .local_captures();
    let fcap = fcap_of(&infos);
    if fcap.captures.is_empty() {
        return Err(refused(
            "captures_not_baked",
            "no local capture is baked: place captures and bake them (render.bake_local_capture) first",
        ));
    }
    let path = crate::req_path::project_path(world, "file", &format!("assets/{file}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| refused("fcap_io", &format!("{file}: {e}")))?;
    }
    fcap.write(&path).map_err(|e| fcap_refused(&file, &e))?;
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut named = false;
    if name_in_level && let Some(mut settings) = world.get_resource_mut::<LevelSettings>() {
        let want = Some(LevelCaptures { file: file.clone() });
        if settings.look.captures != want {
            settings.look.captures = want;
        }
        named = true;
    }
    Ok(json!({
        "file": file,
        "captures": fcap.captures.len(),
        "skipped_unbaked": infos.len() - fcap.captures.len(),
        "bytes": bytes,
        "named_in_level": named,
    }))
}

/// `render.load_local_captures { file }`: reads `assets/<file>` and installs its maps: every
/// placed capture baked at its position shows them from the next frame, with no face rendered.
pub(crate) fn load_captures(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let file = file_param(params)?;
    let path = crate::req_path::project_path(world, "file", &format!("assets/{file}"))?;
    let fcap = FcapFile::read(&path).map_err(|e| fcap_refused(&file, &e))?;
    if !world.contains_resource::<RuntimeRenderer>() {
        return Err(no_renderer());
    }
    let mut bakes = world.get_resource_or_insert_with(LocalCaptureBakes::default);
    bakes.by_id = bakes_of(&fcap)
        .into_iter()
        .map(|(id, at, b)| (id, (at, b)))
        .collect();
    bakes.generation += 1;
    let ids: Vec<u64> = fcap.captures.iter().map(|c| c.id).collect();
    Ok(json!({ "file": file, "captures": fcap.captures.len(), "ids": ids }))
}

fn capture_json(info: &LocalCaptureInfo) -> Value {
    json!({
        "id": info.desc.id,
        "center": info.desc.center,
        "baked": info.baked,
        "pending": info.pending,
        "bakes": info.bakes,
        "layer": info.layer,
        "blend": info.blend,
        "uploads": info.uploads,
    })
}

/// `render.get_capture_bake`: the open level's `.fcap` state, the installed bakes, the selection
/// and the tier budget.
pub(crate) fn get_capture_bake(world: &mut World) -> Result<Value, RpcError> {
    let runtime = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let infos = runtime.0.local_captures();
    // The runtime's frames read the budget of the settings' profile.
    let budget = world
        .get_resource::<regia_render::RenderSettings>()
        .map(|s| local_capture_budget(&s.quality()));
    let level = world
        .get_resource::<LevelCapturesState>()
        .map(LevelCapturesState::report)
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let installed = world
        .get_resource::<LocalCaptureBakes>()
        .map_or(0, |b| b.by_id.len());
    Ok(json!({
        "level": level,
        "installed_bakes": installed,
        "budget": budget,
        "layers": LOCAL_CAPTURE_LAYERS,
        "max": MAX_LOCAL_CAPTURES,
        "fade_frames": LOCAL_CAPTURE_FADE_FRAMES,
        "hysteresis_m": LOCAL_CAPTURE_HYSTERESIS_M,
        "selected": infos.iter().filter(|i| i.layer.is_some()).count(),
        "captures": infos.iter().map(capture_json).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn only_baked_captures_are_saved() {
        let mut baked = regia_render::LocalCaptureDesc::new(3, [1.0, 2.0, 3.0], [2.0, 2.0, 2.0]);
        baked.bake = Some(Arc::new(regia_render::LocalCaptureBake {
            width: regia_render::LOCAL_CAPTURE_WIDTH,
            coverage: 0.25,
            mips: vec![vec![1, 2, 3]],
        }));
        let info = |desc: regia_render::LocalCaptureDesc| LocalCaptureInfo {
            baked: desc.bake.is_some(),
            desc,
            pending: false,
            bakes: 1,
            coverage: 0.25,
            layer: None,
            blend: 0.0,
            uploads: 0,
        };
        let fcap = fcap_of(&[
            info(baked),
            info(regia_render::LocalCaptureDesc::new(4, [0.0; 3], [1.0; 3])),
        ]);
        assert_eq!(fcap.captures.len(), 1);
        assert_eq!(fcap.captures[0].id, 3);
        assert_eq!(fcap.captures[0].coverage, 0.25);
        assert_eq!(fcap.captures[0].center, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_file_must_be_an_fcap_asset_path() {
        let reason = |v: Value| {
            file_param(&v).expect_err("refused").data.expect("data")["reason"]
                .as_str()
                .map(str::to_string)
        };
        assert_eq!(reason(json!({})).as_deref(), Some("bad_file"));
        assert_eq!(
            reason(json!({ "file": "maps/hall.png" })).as_deref(),
            Some("bad_file")
        );
        assert_eq!(
            file_param(&json!({ "file": " maps/hall.fcap " })).expect("ok"),
            "maps/hall.fcap"
        );
    }

    /// A wrong-typed `name_in_level` is refused before anything is written, not read as true
    /// (ADR-2622, ADR-5667).
    #[test]
    fn a_wrong_typed_name_in_level_is_refused() {
        let mut world = World::new();
        let e = save_captures(
            &mut world,
            &json!({ "file": "maps/hall.fcap", "name_in_level": "no" }),
        )
        .expect_err("wrong-typed name_in_level");
        assert_eq!(
            e.data
                .as_ref()
                .and_then(|d| d.get("param"))
                .and_then(Value::as_str),
            Some("name_in_level"),
            "{e:?}"
        );
    }
}
