// SPDX-License-Identifier: MPL-2.0
//! Asset surface (specs/23 § 3.3) over the running [`AssetServer`]: list the cooked manifest,
//! describe, load and reload, and check a cooked skin against its skeleton. Importing, creating, editing and deleting asset files belong to
//! the pipeline/editor surface (specs/12) and are deliberately not here.

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetManifest, AssetServer, LoadState, cooked_file_name};
use regia_core::math::{Mat4, Vec3};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The manifest of the running server, read from its cooked cache (`asset_manifest.bin`).
fn manifest(server: &AssetServer) -> Result<AssetManifest, RpcError> {
    let path = server.cache_dir().join("asset_manifest.bin");
    let bytes = std::fs::read(&path).map_err(|e| {
        tracing::warn!(target: "regia_agent", "cannot read {}: {e}", path.display());
        RpcError::new(
            crate::protocol::NOT_FOUND,
            "F0016",
            "cannot read the asset manifest (asset_manifest.bin in the cache)",
            "the asset cache appears empty — cook the project first (regia cook)",
        )
    })?;
    AssetManifest::from_postcard(&bytes)
        .map_err(|e| RpcError::internal(format!("cannot parse asset manifest: {e}")))
}

/// Resolves the `asset` parameter: a project-relative path or an asset id as text.
pub(crate) fn resolve_asset(server: &AssetServer, params: &Value) -> Result<(String, AssetId), RpcError> {
    let Some(asset) = crate::params::opt_str(params, "asset")?.filter(|s| !s.is_empty()) else {
        return Err(RpcError::invalid_params(
            "`asset` (a project-relative path or an asset id) is required",
        ));
    };
    if let Ok(id) = asset.parse::<AssetId>() {
        let entry = manifest(server)?
            .entries
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| {
                RpcError::new(
                    crate::protocol::NOT_FOUND,
                    "F0016",
                    format!("no manifest entry for asset id {asset}"),
                    "call assets.list to see the known ids",
                )
            })?;
        return Ok((entry.path, id));
    }
    let path = asset.replace('\\', "/");
    let path = path.strip_prefix("assets/").map(str::to_string).unwrap_or(path);
    let full = format!("assets/{path}");
    let Some(id) = server.id_for_path(&full) else {
        return Err(RpcError::new(
            crate::protocol::NOT_FOUND,
            "F0016",
            format!("no manifest entry for {full}"),
            "call assets.list to see the known paths",
        ));
    };
    Ok((full, id))
}

/// The load state as a stable string.
fn state_name(state: LoadState) -> Value {
    match state {
        LoadState::NotLoaded => json!("not_loaded"),
        LoadState::Loading => json!("loading"),
        LoadState::Loaded => json!("loaded"),
        LoadState::Failed(_) => json!("failed"),
    }
}

/// Size of the cooked file, when it exists.
fn cooked_size(server: &AssetServer, id: AssetId) -> Option<u64> {
    std::fs::metadata(server.cache_dir().join(cooked_file_name(id)))
        .ok()
        .map(|m| m.len())
}

/// `assets.list`: manifest entries filtered by path substring, kind and folder.
pub(crate) fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(server) = world.get_resource::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let query = crate::params::opt_str(params, "query")?.unwrap_or("");
    let kind = crate::params::opt_str(params, "kind")?.unwrap_or("");
    let folder = crate::params::opt_str(params, "folder")?.unwrap_or("");
    let folder = folder
        .replace('\\', "/")
        .trim_start_matches("assets/")
        .trim_end_matches('/')
        .to_string();
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(200) as usize;
    let manifest = manifest(server)?;
    let mut assets = Vec::new();
    for entry in &manifest.entries {
        if !query.is_empty() && !entry.path.to_lowercase().contains(&query.to_lowercase()) {
            continue;
        }
        if !kind.is_empty() && !entry.type_path.contains(kind) {
            continue;
        }
        if !folder.is_empty() {
            // Manifest paths carry the `assets/` prefix; the folder filter is project-relative.
            let rel = entry.path.strip_prefix("assets/").unwrap_or(&entry.path);
            if !folder_matches(rel, &folder) {
                continue;
            }
        }
        assets.push(json!({ "path": entry.path, "id": entry.id.to_string(), "type": entry.type_path }));
        if assets.len() >= limit {
            break;
        }
    }
    let count = assets.len();
    Ok(json!({ "assets": assets, "count": count }))
}

/// True when the project-relative `rel` lies under `folder`, ignoring ASCII case (the filter
/// must behave the same on case-insensitive volumes).
fn folder_matches(rel: &str, folder: &str) -> bool {
    let prefix = format!("{folder}/");
    rel.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
}

/// `assets.describe`: manifest entry plus state and cooked-file info.
///
/// `state` is `not_loaded`, `loading`, `loaded` or `failed`. The failure string is
/// `assets.status`'s `error` field. This object does not include it.
pub(crate) fn describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (path, id) = {
        let Some(server) = world.get_resource::<AssetServer>() else {
            return Err(no_asset_server());
        };
        let (path, id) = resolve_asset(server, params)?;
        (path, id)
    };
    let Some(server) = world.get_resource_mut::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let entry_type = manifest(&server)?
        .entries
        .iter()
        .find(|e| e.id == id)
        .map(|e| e.type_path.clone());
    Ok(json!({
        "path": path,
        "id": id.to_string(),
        "type": entry_type,
        "state": state_name(server.state(id)),
        "cooked_bytes": cooked_size(&server, id),
        "cooked_file": cooked_file_name(id),
    }))
}

/// A watched root as shown to the caller: relative to the project root, or just its last
/// component when it lies elsewhere (no absolute host paths in answers).
fn shown_root(root: &std::path::Path, project: Option<&std::path::Path>) -> String {
    match project.and_then(|p| root.strip_prefix(p).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Some(rel) => rel.display().to_string(),
        None => root
            .file_name()
            .map_or_else(|| ".".to_string(), |n| n.to_string_lossy().into_owned()),
    }
}

/// `assets.load`: load a mesh, texture or image (tried in that order) into the app.
pub(crate) fn load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let (path, id) = resolve_asset(&server, params)?;
    let cooked = server.cache_dir().join(cooked_file_name(id));
    if !cooked.is_file() {
        tracing::warn!(target: "regia_agent", "no cooked file {}", cooked.display());
        return Err(RpcError::new(
            crate::protocol::NOT_FOUND,
            "F0016",
            format!("no cooked file for {path}"),
            "the asset was never cooked — run regia cook / assets.import on the pipeline side",
        ));
    }
    let bytes = std::fs::read(&cooked).map_err(|e| {
        tracing::warn!(target: "regia_agent", "cannot read cooked file {}: {e}", cooked.display());
        RpcError::internal("cannot read the cooked file")
    })?;
    let mesh = regia_asset::load_mesh_bytes(&bytes)
        .ok()
        .map(|loaded| mesh_summary(&loaded.mesh));
    let kind = if mesh.is_some() {
        "mesh"
    } else if regia_asset::load_texture_bytes(&bytes).is_ok() {
        "texture"
    } else if regia_asset::load_image_bytes(&bytes).is_ok() {
        "image"
    } else {
        return Err(RpcError::new(
            crate::protocol::UNSUPPORTED,
            "F0013",
            format!("cooked file for {path} is not a mesh, texture or image"),
            "the type-specific loaders of this asset kind are not compiled in",
        ));
    };
    let result = match kind {
        "mesh" => server.load_mesh(id).map(|_| ()),
        "texture" => server.load_texture(id).map(|_| ()),
        _ => server.load_image(id).map(|_| ()),
    };
    result.map_err(|e| {
        RpcError::new(
            crate::protocol::INTERNAL_ERROR,
            "F0016",
            format!("cannot load {path}: {e}"),
            "see assets.status for the recorded failure",
        )
    })?;
    let mut reply =
        json!({ "path": path, "id": id.to_string(), "kind": kind, "state": state_name(server.state(id)) });
    if let Some(mesh) = mesh {
        reply["mesh"] = mesh;
    }
    Ok(reply)
}

/// What a loaded mesh carries (cooked mesh format v2): vertex and triangle counts, the skin
/// (joint count, influences per vertex), whether it has `COLOR_0`, and its morph target names.
fn mesh_summary(mesh: &regia_asset::CpuMesh) -> Value {
    json!({
        "vertices": mesh.positions.len(),
        "triangles": mesh.indices.len() / 3,
        "skin": mesh.skin.as_ref().map(|skin| json!({
            "joints": skin.joint_names.len(),
            "influences": skin.influences_per_vertex(),
        })),
        "colors": !mesh.colors.is_empty(),
        "morph_targets": mesh.morph_targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
    })
}

/// Cooked bytes of the asset named by `params[key]` (a project-relative path or asset id).
pub(crate) fn cooked_bytes(
    server: &AssetServer,
    params: &Value,
    key: &str,
) -> Result<(String, Vec<u8>), RpcError> {
    let Some(asset) = params
        .get(key)
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    else {
        return Err(RpcError::invalid_params(format!(
            "`{key}` (a project-relative path or an asset id) is required"
        )));
    };
    let (path, id) = resolve_asset(server, &json!({ "asset": asset }))?;
    let cooked = server.cache_dir().join(cooked_file_name(id));
    let bytes = std::fs::read(&cooked).map_err(|e| {
        tracing::warn!(target: "regia_agent", "cannot read cooked file {}: {e}", cooked.display());
        RpcError::new(
            crate::protocol::NOT_FOUND,
            "F0016",
            format!("no cooked file for {path}"),
            "the asset was never cooked — run regia cook / assets.import on the pipeline side",
        )
    })?;
    Ok((path, bytes))
}

/// `assets.skin_check`: skin a cooked mesh on the CPU with a cooked skeleton, in the bind pose
/// and optionally in a cooked clip's pose at `time`, the way the renderer does (palette
/// `global · inverse_bind`, the 4 strongest influences). Reports whether the skin's joint names
/// match the skeleton, how far bind-pose skinning moves the cooked vertices (≈ 0 when the mesh
/// and the skeleton agree on their frame), and the skinned bounds in the skeleton's object space.
pub(crate) fn skin_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(server) = world.get_resource::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let (mesh_path, mesh_bytes) = cooked_bytes(server, params, "mesh")?;
    let (skeleton_path, skeleton_bytes) = cooked_bytes(server, params, "skeleton")?;
    let clip = match params.get("clip") {
        None | Some(Value::Null) => None,
        Some(_) => Some(cooked_bytes(server, params, "clip")?),
    };
    let time = match params.get("time") {
        None | Some(Value::Null) => 0.0,
        Some(value) => value
            .as_f64()
            .filter(|t| t.is_finite())
            .ok_or_else(|| RpcError::invalid_params("`time` must be a finite number of seconds"))?
            as f32,
    };
    let undecodable = |what: &str, path: &str, e: &dyn std::fmt::Display| {
        RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0013",
            format!("{path} is not a cooked {what}: {e}"),
            "pass the mesh, skeleton and clip sub-assets cooked from the same glTF (assets.list)",
        )
    };
    let mesh = regia_asset::load_mesh_bytes(&mesh_bytes)
        .map_err(|e| undecodable("mesh", &mesh_path, &e))?
        .mesh;
    let (_, skeleton) = regia_animation::cooked::load_cooked_skeleton(&skeleton_bytes)
        .map_err(|e| undecodable("skeleton", &skeleton_path, &e))?;
    let clip = clip
        .map(|(path, bytes)| {
            regia_animation::cooked::load_cooked_clip(&bytes)
                .map(|(_, clip)| (path.clone(), clip))
                .map_err(|e| undecodable("clip", &path, &e))
        })
        .transpose()?;
    let Some(skin) = mesh.skin.as_ref() else {
        return Err(RpcError::invalid_params(format!(
            "{mesh_path} has no skin (a rigid mesh, or cooked before mesh format v2: re-cook it)"
        )));
    };

    // Skin joint → skeleton joint, by name.
    let to_skeleton: Vec<Option<usize>> = skin
        .joint_names
        .iter()
        .map(|name| skeleton.joint_names.iter().position(|n| n == name))
        .collect();
    let missing: Vec<&str> = skin
        .joint_names
        .iter()
        .zip(&to_skeleton)
        .filter(|(_, j)| j.is_none())
        .map(|(name, _)| name.as_str())
        .take(16)
        .collect();
    let bind_pose = skeleton.bind_pose_pose();
    let bind_palette = skeleton.skin_palette(&skeleton.globals(&bind_pose, None));
    let palette = match &clip {
        Some((_, clip)) => {
            let mut pose = bind_pose.clone();
            clip.sample_pose(time, &mut pose);
            skeleton.skin_palette(&skeleton.globals(&pose, None))
        }
        None => bind_palette.clone(),
    };
    let skin_vertex = |palette: &[Mat4], v: usize, p: Vec3| -> Vec3 {
        let Some((joints, weights)) = skin.top4(v) else {
            return p;
        };
        let mut out = Vec3::ZERO;
        let mut total = 0.0;
        for (joint, weight) in joints.iter().zip(weights) {
            let matrix = to_skeleton
                .get(*joint as usize)
                .copied()
                .flatten()
                .and_then(|j| palette.get(j));
            if let Some(matrix) = matrix {
                out += matrix.transform_point3(p) * weight;
                total += weight;
            }
        }
        if total > 0.0 { out / total } else { p }
    };
    let mut bind_offset_max = 0.0f32;
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let (mut bind_lo, mut bind_hi) = (lo, hi);
    for (v, position) in mesh.positions.iter().enumerate() {
        let p = Vec3::from_array(*position);
        let bind = skin_vertex(&bind_palette, v, p);
        bind_offset_max = bind_offset_max.max(bind.distance(p));
        bind_lo = bind_lo.min(bind);
        bind_hi = bind_hi.max(bind);
        let posed = skin_vertex(&palette, v, p);
        lo = lo.min(posed);
        hi = hi.max(posed);
    }
    let bounds = |lo: Vec3, hi: Vec3| {
        if mesh.positions.is_empty() {
            Value::Null
        } else {
            json!({ "min": lo.to_array(), "max": hi.to_array() })
        }
    };
    Ok(json!({
        "mesh": mesh_path,
        "skeleton": skeleton_path,
        "clip": clip.as_ref().map(|(path, _)| path.as_str()),
        "time": time,
        "vertices": mesh.positions.len(),
        "joints": skin.joint_names.len(),
        "skeleton_joints": skeleton.joint_names.len(),
        "names_match": skin.joint_names == skeleton.joint_names,
        "missing_joints": missing,
        "bind_offset_max": bind_offset_max,
        "bind_bounds": bounds(bind_lo, bind_hi),
        "bounds": bounds(lo, hi),
        // Influences the cooked skin stores per vertex (4 or 8) and how many the renderer draws:
        // 8 only when the mesh opted in (cooked `skin_influences` 8, ADR-1622).
        "stored_influences": skin.influences_per_vertex(),
        "draw_influences": if skin.eight_influences { 8 } else { 4 },
    }))
}

/// `assets.reload`: re-read cooked data for an asset (the hot-reload path).
pub(crate) fn reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let (path, id) = resolve_asset(&server, params)?;
    server.reload_id(id).map_err(|e| {
        RpcError::new(
            crate::protocol::INTERNAL_ERROR,
            "F0016",
            format!("cannot reload {path}: {e}"),
            "the cooked file may be missing; check assets.status",
        )
    })?;
    Ok(json!({ "path": path, "id": id.to_string(), "state": state_name(server.state(id)) }))
}

/// `assets.watch_status`: the live-reload watcher and its recent results
/// ([`regia_asset::AssetWatchStatus`]). An app without the asset plugin reports `watching:
/// false` and no entries.
pub(crate) fn watch_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = crate::params::opt_u64(params, "limit")?.map_or(16, |n| {
        n.clamp(1, regia_asset::AssetWatchStatus::RECENT as u64) as usize
    });
    let status = world
        .get_resource::<regia_asset::AssetWatchStatus>()
        .cloned()
        .unwrap_or_default();
    let project_root = world
        .get_resource::<AssetServer>()
        .map(|server| server.project_root().to_path_buf());
    let skip = status.recent.len().saturating_sub(limit);
    let recent: Vec<Value> = status
        .recent
        .iter()
        .skip(skip)
        .map(|entry| {
            let mut out = json!({
                "path": entry.path,
                "kind": entry.kind.as_str(),
                "ms": entry.millis,
                "ok": matches!(entry.outcome, regia_asset::WatchOutcome::Reloaded),
            });
            if let regia_asset::WatchOutcome::Error(err) = &entry.outcome {
                out["error"] = json!(err);
            }
            out
        })
        .collect();
    let mut out = json!({
        "watching": status.watching,
        "roots": status.roots.iter().map(|r| shown_root(r, project_root.as_deref())).collect::<Vec<_>>(),
        "debounce_ms": status.debounce_ms,
        "reloads": status.reloads,
        "errors": status.errors,
        "recent": recent,
    });
    if let Some(line) = status.last_line() {
        out["last"] = json!(line);
    }
    Ok(out)
}

/// `assets.status`: state, size and last error of an asset, plus the mounted paks and how many
/// of them serve zero-copy views now (verified, unchanged on disk).
pub(crate) fn status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(server) = world.get_resource::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let (path, id) = resolve_asset(server, params)?;
    let state = server.state(id);
    let mut out = json!({
        "path": path,
        "id": id.to_string(),
        "state": state_name(state.clone()),
        "cooked_bytes": cooked_size(server, id),
        "paks": {
            "mounted": server.pak_count(),
            "zero_copy": server.zero_copy_pak_count(),
        },
    });
    if let LoadState::Failed(error) = state {
        out["error"] = json!(error);
    }
    Ok(out)
}

/// `assets.cooked_pack`: every mounted pak by load cost, the cooked-pack rules each breaks (ADR-1594),
/// and the forms the server loaded so far. `inspect: true` also reads each texture entry's KTX2
/// header (every texture entry of every pak: an occasional check, not a per-frame call).
pub(crate) fn cooked_pack(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_asset::cooked_pack::{Severity, check_pak_summary};
    let inspect = match params.get("inspect") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(on)) => *on,
        Some(_) => return Err(RpcError::invalid_params("`inspect` must be a boolean")),
    };
    let Some(server) = world.get_resource::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let platform = server.platform().to_owned();
    let (mut errors, mut warnings) = (0usize, 0usize);
    let mut paks = Vec::new();
    for summary in server.mounted_pak_summaries(inspect) {
        let findings = check_pak_summary(&summary, &platform);
        for finding in &findings {
            match finding.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
            }
        }
        let mut row = serde_json::to_value(&summary).map_err(|e| RpcError::internal(e.to_string()))?;
        if let Value::Object(map) = &mut row {
            map.insert(
                "findings".into(),
                serde_json::to_value(&findings).map_err(|e| RpcError::internal(e.to_string()))?,
            );
        }
        paks.push(row);
    }
    let loads = server.load_forms();
    Ok(json!({
        "platform": platform,
        "paks": paks,
        "loads": serde_json::to_value(loads).map_err(|e| RpcError::internal(e.to_string()))?,
        "errors": errors,
        "warnings": warnings,
        "cooked_only": !paks.is_empty() && errors == 0 && loads.cooked_only(),
    }))
}

/// The shared "no AssetServer" error.
/// The file of the last cook's font report (`regia_asset_pipeline::font_subset::FONT_SUBSETS_FILE`; the runtime does not
/// link the pipeline, a test ties the two).
const FONT_SUBSETS_FILE: &str = "font_subsets.json";
/// The newest report this build reads.
const FONT_SUBSETS_VERSION: u64 = 1;

/// `assets.font_subset { font? }` (ADR-2697): what the last cook did to the project's fonts, from the cooked cache.
pub(crate) fn font_subset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted = match params.get("font").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            RpcError::new(
                crate::protocol::INVALID_PARAMS,
                "font_subset_bad_param",
                "`font` must be a non-empty string",
                "pass a project-relative path such as assets/fonts/ui.ttf",
            )
        })?),
    };
    let Some(server) = world.get_resource::<AssetServer>() else {
        return Err(no_asset_server());
    };
    let path = server.cache_dir().join(FONT_SUBSETS_FILE);
    let bytes = std::fs::read(&path).map_err(|_| {
        RpcError::new(
            crate::protocol::NOT_FOUND,
            "font_subset_no_report",
            format!("no {FONT_SUBSETS_FILE} in the cooked cache"),
            "cook the project first (regia cook); a package does not carry this report",
        )
    })?;
    let bad = |why: String| {
        RpcError::new(
            crate::protocol::INTERNAL_ERROR,
            "font_subset_bad_report",
            format!("{FONT_SUBSETS_FILE} does not read: {why}"),
            "cook the project again with this build",
        )
    };
    let report: Value = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    let version = report.get("format_version").and_then(Value::as_u64);
    if version.is_none_or(|v| v > FONT_SUBSETS_VERSION) {
        return Err(bad(format!("format_version {version:?}")));
    }
    let fonts = report
        .get("fonts")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("no `fonts` list".to_owned()))?;
    let rows: Vec<&Value> = fonts
        .iter()
        .filter(|f| wanted.is_none_or(|w| f.get("path").and_then(Value::as_str) == Some(w)))
        .collect();
    if let Some(font) = wanted
        && rows.is_empty()
    {
        return Err(RpcError::new(
            crate::protocol::NOT_FOUND,
            "font_subset_unknown_font",
            format!("the last cook has no font `{font}`"),
            "call assets.font_subset without `font` to see the cooked fonts",
        ));
    }
    let sum = |key: &str| {
        rows.iter()
            .filter_map(|f| f.get(key).and_then(Value::as_u64))
            .sum::<u64>()
    };
    let subset = rows
        .iter()
        .filter(|f| f.get("subset").and_then(Value::as_bool) == Some(true))
        .count();
    Ok(json!({
        "format_version": version,
        "totals": {
            "fonts": rows.len(),
            "subset": subset,
            "bytes_before": sum("bytes_before"),
            "bytes_after": sum("bytes_after"),
        },
        "fonts": rows,
    }))
}

pub(crate) fn no_asset_server() -> RpcError {
    RpcError::new(
        crate::protocol::NOT_FOUND,
        "F0016",
        "no AssetServer in this app",
        "add regia_asset::AssetPlugin with the project's settings",
    )
}

/// `assets.access_trace`: bounded runtime diagnostics; never opens a caller-supplied path.
pub(crate) fn access_trace(params: &Value) -> Result<Value, RpcError> {
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(256).min(256) as usize;
    serde_json::to_value(regia_asset::access_trace::snapshot(limit))
        .map_err(|e| RpcError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn folder_filter_ignores_case() {
        assert!(super::folder_matches("Characters/hero.glb", "characters"));
        assert!(super::folder_matches("a/b.glb", "a"));
        assert!(!super::folder_matches("ab/b.glb", "a"));
        assert!(!super::folder_matches("é", "a"));
    }

    use super::*;
    use regia_asset::{AssetManifest, AssetTypeTag, CookedHeader, ManifestEntry};
    use regia_asset::{AssetPlugin, AssetServerSettings};

    /// Builds a tiny project with one cooked mesh and an AssetServer over it.
    fn cooked_project(tag: &str) -> (std::path::PathBuf, AssetId) {
        let root = std::env::temp_dir().join(format!("regia-agent-assets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let platform = "linux";
        let cache = regia_asset::cooked_cache_dir(&root, platform);
        std::fs::create_dir_all(&cache).expect("cache dir");

        let id = AssetId::new_random();
        let manifest = AssetManifest {
            entries: vec![ManifestEntry {
                path: "assets/khronos/helmet.glb".into(),
                id,
                type_path: "regia_asset::CpuMesh".into(),
            }],
        };
        manifest.write_to_cache(&cache).expect("manifest");

        // A minimal valid cooked mesh file (triangle) so `load` can decode it.
        let mesh = regia_asset::CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0]; 3],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let payload = mesh.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: regia_asset::pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, &payload).expect("encode");
        std::fs::write(cache.join(cooked_file_name(id)), file).expect("cooked file");
        (root, id)
    }

    fn app_with_assets(tag: &str) -> (App, std::path::PathBuf, AssetId) {
        let (root, id) = cooked_project(tag);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        (app, root, id)
    }

    fn cleanup(root: &std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }

    /// The report the pipeline writes is the one the method reads (`FONT_SUBSETS_FILE` is written down twice).
    #[test]
    fn font_subset_reads_the_cooks_report() {
        use regia_asset_pipeline::font_subset as pipeline;
        let (mut app, root, _) = app_with_assets("font-subset");
        let world = app.world_mut();
        let code = |e: &RpcError| {
            serde_json::to_value(e)
                .ok()
                .and_then(|v| v.pointer("/data/code").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default()
        };
        let err = font_subset(world, &json!({})).expect_err("no cook yet");
        assert_eq!(code(&err), "font_subset_no_report", "{err:?}");
        let err = font_subset(world, &json!({ "font": 3 })).expect_err("bad param");
        assert_eq!(code(&err), "font_subset_bad_param", "{err:?}");

        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        assert_eq!(pipeline::FONT_SUBSETS_FILE, FONT_SUBSETS_FILE);
        assert_eq!(u64::from(pipeline::FONT_SUBSETS_VERSION), FONT_SUBSETS_VERSION);
        let row = |path: &str, before: u64, after: u64| pipeline::FontSubsetSummary {
            path: path.to_owned(),
            mode: "auto".to_owned(),
            subset: after < before,
            code: None,
            glyphs_total: 100,
            glyphs_kept: 10,
            bytes_before: before,
            bytes_after: after,
            locales: vec!["fr".to_owned()],
            missing: std::collections::BTreeMap::from([("fr".to_owned(), "\u{3042}".to_owned())]),
        };
        pipeline::write_report(
            &cache,
            &[
                row("assets/fonts/a.ttf", 1000, 400),
                row("assets/fonts/b.ttf", 500, 500),
            ],
        )
        .expect("report");

        let all = font_subset(world, &json!({})).expect("reads");
        assert_eq!(all["totals"]["fonts"], 2);
        assert_eq!(all["totals"]["subset"], 1);
        assert_eq!(all["totals"]["bytes_before"], 1500);
        assert_eq!(all["totals"]["bytes_after"], 900);
        let one = font_subset(world, &json!({ "font": "assets/fonts/a.ttf" })).expect("one");
        assert_eq!(one["fonts"].as_array().map(Vec::len), Some(1));
        assert_eq!(one["fonts"][0]["glyphs_kept"], 10);
        assert_eq!(one["fonts"][0]["bytes_after"], 400);
        assert_eq!(one["fonts"][0]["missing"]["fr"], "\u{3042}");
        let err = font_subset(world, &json!({ "font": "assets/fonts/c.ttf" })).expect_err("unknown");
        assert_eq!(code(&err), "font_subset_unknown_font", "{err:?}");

        std::fs::write(cache.join(FONT_SUBSETS_FILE), "{ nope").expect("damage");
        let err = font_subset(world, &json!({})).expect_err("damaged");
        assert_eq!(code(&err), "font_subset_bad_report", "{err:?}");
        std::fs::write(
            cache.join(FONT_SUBSETS_FILE),
            r#"{ "format_version": 99, "fonts": [] }"#,
        )
        .expect("newer");
        let err = font_subset(world, &json!({})).expect_err("newer");
        assert_eq!(code(&err), "font_subset_bad_report", "{err:?}");
        cleanup(&root);
    }

    #[test]
    fn list_filters_by_query_and_folder() {
        let (mut app, root, id) = app_with_assets("list");
        let listed = list(app.world_mut(), &json!({})).expect("lists");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["assets"][0]["id"], id.to_string());
        assert_eq!(listed["assets"][0]["path"], "assets/khronos/helmet.glb");

        let none = list(app.world_mut(), &json!({ "query": "notthere" })).expect("lists");
        assert_eq!(none["count"], 0);

        let folder = list(app.world_mut(), &json!({ "folder": "khronos" })).expect("lists");
        assert_eq!(folder["count"], 1);
        let other = list(app.world_mut(), &json!({ "folder": "other" })).expect("lists");
        assert_eq!(other["count"], 0);
        cleanup(&root);
    }

    /// A project with a cooked two-joint skinned triangle (`root` → `arm`), its skeleton, a
    /// clip that slides `root` 2 m along +X over one second, and a skeleton without `arm`.
    fn skinned_project(tag: &str) -> (App, std::path::PathBuf) {
        use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
        use regia_animation::skeleton::Skeleton;

        let root = std::env::temp_dir().join(format!("regia-agent-skin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let write = |id: AssetId, tag: regia_core::id::TypePathHash, payload: &[u8]| {
            let header = CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: tag,
                asset_id: id,
                importer_version: regia_asset::pack_importer_version(1, 8, 0),
                dependencies: vec![],
                payload_len: payload.len() as u64,
            };
            let file = regia_asset::encode_cooked_file(&header, payload).expect("encode");
            std::fs::write(cache.join(cooked_file_name(id)), file).expect("cooked file");
        };
        let ids = [
            AssetId::new_random(),
            AssetId::new_random(),
            AssetId::new_random(),
            AssetId::new_random(),
        ];
        let paths = [
            "assets/pilot.glb#mesh",
            "assets/pilot.glb#skeleton",
            "assets/pilot.glb#clip",
            "assets/other.glb#skeleton",
        ];
        let types = [
            "regia_asset::CookedMesh",
            "regia_animation::Skeleton",
            "regia_animation::AnimationClip",
            "regia_animation::Skeleton",
        ];
        AssetManifest {
            entries: (0..4)
                .map(|i| ManifestEntry {
                    path: paths[i].into(),
                    id: ids[i],
                    type_path: types[i].into(),
                })
                .collect(),
        }
        .write_to_cache(&cache)
        .expect("manifest");

        let positions: Vec<u8> = [0.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let skin = regia_asset::CpuSkin {
            joint_names: vec!["root".into(), "arm".into()],
            joints: vec![[0, 0, 0, 0], [1, 0, 0, 0], [1, 0, 0, 0]],
            weights: vec![[65535, 0, 0, 0]; 3],
            ..Default::default()
        };
        let mesh = regia_asset::CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.0],
            bounding_sphere: [0.5, 0.5, 0.0, 1.0],
            stream_positions: positions,
            stream_normals_tangents: vec![0u8; 3 * 8],
            stream_uv0: vec![0u8; 3 * 4],
            indices: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            index_width: regia_asset::CookedIndexWidth::U16,
            vertex_count: 3,
            lods: vec![],
            submeshes: vec![],
            lod_metric: regia_asset::LOD_METRIC_ERROR,
            format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            skin: Some(skin.pack(3).expect("pack skin")),
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
        };
        write(ids[0], AssetTypeTag::mesh().0, &mesh.to_postcard().expect("mesh"));

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "arm".into()],
            vec![None, Some(0)],
            vec![
                regia_animation::pose::translated(Vec3::ZERO),
                regia_animation::pose::translated(Vec3::Y),
            ],
        )
        .expect("skeleton");
        let skeleton_tag = regia_animation::cooked::skeleton_type_tag();
        write(
            ids[1],
            skeleton_tag,
            &postcard::to_allocvec(&skeleton).expect("skeleton"),
        );
        let mut clip = AnimationClip::empty("slide", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(regia_core::math::Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let clip_tag = regia_animation::cooked::clip_type_tag();
        write(ids[2], clip_tag, &postcard::to_allocvec(&clip).expect("clip"));
        let lone = Skeleton::from_local_bind(
            vec!["root".into()],
            vec![None],
            vec![regia_animation::pose::translated(Vec3::ZERO)],
        )
        .expect("skeleton");
        write(
            ids[3],
            skeleton_tag,
            &postcard::to_allocvec(&lone).expect("skeleton"),
        );

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        (app, root)
    }

    #[test]
    fn skin_check_skins_the_bind_pose_and_a_clip_pose() {
        let (mut app, root) = skinned_project("pose");
        let bind = skin_check(
            app.world_mut(),
            &json!({ "mesh": "pilot.glb#mesh", "skeleton": "pilot.glb#skeleton" }),
        )
        .expect("bind check");
        assert_eq!(bind["names_match"], true);
        assert_eq!(bind["joints"], 2);
        assert_eq!(bind["vertices"], 3);
        assert!(bind["bind_offset_max"].as_f64().expect("offset") < 1e-6, "{bind}");
        assert_eq!(bind["bounds"], bind["bind_bounds"]);
        assert_eq!(bind["clip"], Value::Null);
        assert_eq!(bind["stored_influences"], 4);
        assert_eq!(bind["draw_influences"], 4, "the default draws four");

        let posed = skin_check(
            app.world_mut(),
            &json!({ "mesh": "pilot.glb#mesh", "skeleton": "pilot.glb#skeleton",
                     "clip": "pilot.glb#clip", "time": 1.0 }),
        )
        .expect("pose check");
        let min = posed["bounds"]["min"][0].as_f64().expect("min");
        let max = posed["bounds"]["max"][0].as_f64().expect("max");
        assert!((min - 2.0).abs() < 1e-5 && (max - 3.0).abs() < 1e-5, "{posed}");
        assert_eq!(posed["clip"], "assets/pilot.glb#clip");
        cleanup(&root);
    }

    #[test]
    fn skin_check_reports_missing_joints_and_rejects_bad_input() {
        let (mut app, root) = skinned_project("bad");
        let other = skin_check(
            app.world_mut(),
            &json!({ "mesh": "pilot.glb#mesh", "skeleton": "other.glb#skeleton" }),
        )
        .expect("check");
        assert_eq!(other["names_match"], false);
        assert_eq!(other["missing_joints"], json!(["arm"]));

        let missing =
            skin_check(app.world_mut(), &json!({ "mesh": "pilot.glb#mesh" })).expect_err("skeleton required");
        assert!(missing.message.contains("`skeleton`"), "{}", missing.message);
        let swapped = skin_check(
            app.world_mut(),
            &json!({ "mesh": "pilot.glb#skeleton", "skeleton": "pilot.glb#skeleton" }),
        )
        .expect_err("a skeleton is not a mesh");
        assert!(
            swapped.message.contains("not a cooked mesh"),
            "{}",
            swapped.message
        );
        let bad_time = skin_check(
            app.world_mut(),
            &json!({ "mesh": "pilot.glb#mesh", "skeleton": "pilot.glb#skeleton", "time": "soon" }),
        )
        .expect_err("time must be a number");
        assert!(bad_time.message.contains("`time`"), "{}", bad_time.message);
        cleanup(&root);
    }

    #[test]
    fn describe_status_and_load_by_path_and_id() {
        let (mut app, root, id) = app_with_assets("load");
        let by_path =
            describe(app.world_mut(), &json!({ "asset": "khronos/helmet.glb" })).expect("describes");
        assert_eq!(by_path["id"], id.to_string());
        assert_eq!(by_path["type"], "regia_asset::CpuMesh");

        let by_id = describe(app.world_mut(), &json!({ "asset": id.to_string() })).expect("describes");
        assert_eq!(by_id["path"], "assets/khronos/helmet.glb");

        let loaded = load(app.world_mut(), &json!({ "asset": "assets/khronos/helmet.glb" })).expect("loads");
        assert_eq!(loaded["kind"], "mesh");
        assert_eq!(loaded["state"], "loaded");
        assert_eq!(loaded["mesh"]["vertices"], 3);
        assert_eq!(loaded["mesh"]["skin"], Value::Null);

        let status = status(app.world_mut(), &json!({ "asset": "khronos/helmet.glb" })).expect("status");
        assert_eq!(status["state"], "loaded");
        assert!(status["cooked_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(status.get("error").is_none());
        // A cooked-cache project mounts no pak.
        assert_eq!(status["paks"], json!({ "mounted": 0, "zero_copy": 0 }));

        let missing = load(app.world_mut(), &json!({ "asset": "khronos/nope.glb" }));
        let err = missing.expect_err("404s");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0016"));
        cleanup(&root);
    }

    /// A failed load records the reason on `assets.status` only.
    #[test]
    fn describe_omits_the_error_status_reports() {
        use std::collections::BTreeMap;

        let (mut app, root, id) = app_with_assets("describe-failed");
        {
            let mut server = app.world_mut().resource_mut::<AssetServer>();
            let mut chunk_of = BTreeMap::new();
            chunk_of.insert(id, "dlc".to_string());
            server.set_chunk_map(Some(regia_asset::content::ChunkMap::from_assignment(&chunk_of)));
        }
        let err =
            load(app.world_mut(), &json!({ "asset": "khronos/helmet.glb" })).expect_err("unmounted chunk");
        assert!(err.message.contains("unavailable:dlc"), "{}", err.message);

        let described = describe(app.world_mut(), &json!({ "asset": id.to_string() })).expect("describes");
        assert_eq!(described["state"], "failed");
        assert!(described.get("error").is_none(), "{described}");

        let status = status(app.world_mut(), &json!({ "asset": id.to_string() })).expect("status");
        assert_eq!(status["state"], "failed");
        assert_eq!(status["error"], "unavailable:dlc");
        cleanup(&root);
    }

    /// `roots` are project-relative. A root under the project has no absolute prefix.
    #[test]
    fn watch_status_root_is_relative_to_the_project() {
        let (mut app, root, _) = app_with_assets("watch-root");
        app.insert_resource(regia_asset::AssetWatchStatus {
            watching: true,
            roots: vec![root.join("assets")],
            debounce_ms: 120,
            ..Default::default()
        });
        let out = watch_status(app.world_mut(), &json!({})).expect("status");
        let shown = out["roots"][0].as_str().expect("root");
        assert_eq!(shown, "assets");
        assert!(!shown.starts_with('/'), "{shown}");
        cleanup(&root);
    }

    #[test]
    fn mesh_summary_reports_skin_colours_and_morph_targets() {
        let mesh = regia_asset::CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0; 2]; 3],
            indices: vec![0, 1, 2],
            skin: Some(regia_asset::CpuSkin {
                joint_names: (0..82).map(|i| format!("j{i}")).collect(),
                joints: vec![[0; 4]; 3],
                weights: vec![[65535, 0, 0, 0]; 3],
                joints1: vec![[0; 4]; 3],
                weights1: vec![[0; 4]; 3],
                eight_influences: false,
            }),
            colors: vec![[1.0; 4]; 3],
            morph_targets: vec![regia_asset::CpuMorphTarget {
                name: "smile".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let summary = mesh_summary(&mesh);
        assert_eq!(summary["triangles"], 1);
        assert_eq!(summary["skin"]["joints"], 82);
        assert_eq!(summary["skin"]["influences"], 8);
        assert_eq!(summary["colors"], true);
        assert_eq!(summary["morph_targets"], json!(["smile"]));
    }

    #[test]
    fn reload_returns_the_new_state() {
        let (mut app, root, id) = app_with_assets("reload");
        load(app.world_mut(), &json!({ "asset": id.to_string() })).expect("loads");
        let reloaded = reload(app.world_mut(), &json!({ "asset": "khronos/helmet.glb" })).expect("reloads");
        assert_eq!(reloaded["state"], "loaded");
        cleanup(&root);
    }

    #[test]
    fn watch_status_reports_the_watcher_and_recent_results() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let idle = watch_status(app.world_mut(), &json!({})).expect("no status resource is fine");
        assert_eq!(idle["watching"], false);
        assert_eq!(idle["recent"].as_array().map(Vec::len), Some(0));

        let mut status = regia_asset::AssetWatchStatus {
            watching: true,
            roots: vec!["/p/assets".into()],
            debounce_ms: 120,
            ..Default::default()
        };
        status.record(regia_asset::WatchEntry {
            path: "assets/albedo.png".into(),
            kind: regia_asset::SourceKind::Texture,
            millis: 95,
            outcome: regia_asset::WatchOutcome::Reloaded,
        });
        status.record(regia_asset::WatchEntry {
            path: "assets/m.material.ron".into(),
            kind: regia_asset::SourceKind::Material,
            millis: 4,
            outcome: regia_asset::WatchOutcome::Error("1:3: expected `)`".into()),
        });
        app.insert_resource(status);
        let out = watch_status(app.world_mut(), &json!({ "limit": 1 })).expect("status");
        assert_eq!(out["watching"], true);
        assert_eq!(out["roots"][0], "assets", "roots never show the absolute path");
        assert_eq!(out["debounce_ms"], 120);
        assert_eq!(out["reloads"], 1);
        assert_eq!(out["errors"], 1);
        assert_eq!(out["recent"].as_array().map(Vec::len), Some(1));
        assert_eq!(out["recent"][0]["kind"], "material");
        assert_eq!(out["recent"][0]["ok"], false);
        assert_eq!(
            out["last"],
            "cook error in assets/m.material.ron: 1:3: expected `)`"
        );
    }

    #[test]
    fn cooked_pack_reports_mounted_paks_their_findings_and_the_loads() {
        let (root, id) = cooked_project("cooked_pack");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        let content = root.join("content");
        regia_asset_pipeline::pack_cooked_directory_with(
            &cache,
            &content,
            &std::collections::BTreeMap::new(),
            false,
            false,
        )
        .expect("pak");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: Some(content),
            },
        });

        let before = cooked_pack(app.world_mut(), &json!({})).expect("report");
        assert_eq!(before["platform"], "linux");
        assert_eq!(before["paks"].as_array().map(Vec::len), Some(1), "{before:#}");
        let pak = &before["paks"][0];
        assert_eq!(pak["name"], "base");
        assert_eq!(pak["meshes"]["legacy"], 1, "{pak:#}");
        assert_eq!(pak["textures_inspected"], false);
        assert_eq!(pak["findings"][0]["code"], "legacy_meshes");
        assert_eq!(pak["findings"][0]["severity"], "error");
        assert_eq!(pak["findings"][0]["examples"][0], "assets/khronos/helmet.glb");
        assert_eq!(before["errors"], 1);
        assert_eq!(before["cooked_only"], false);
        assert_eq!(before["loads"]["pak_loads"], 0);

        load(app.world_mut(), &json!({ "asset": id.to_string() })).expect("loads");
        let after = cooked_pack(app.world_mut(), &json!({ "inspect": true })).expect("report");
        assert_eq!(after["paks"][0]["textures_inspected"], true);
        assert_eq!(after["loads"]["legacy_meshes"], 1, "{after:#}");
        assert_eq!(after["loads"]["pak_loads"], 1, "served by the pak, not the cache");
        assert_eq!(after["loads"]["cache_loads"], 0);

        let err = cooked_pack(app.world_mut(), &json!({ "inspect": "yes" })).expect_err("not a boolean");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        cleanup(&root);
    }

    #[test]
    fn cooked_pack_without_paks_is_not_cooked_only() {
        let (mut app, root, _) = app_with_assets("cooked_pack_dev");
        let out = cooked_pack(app.world_mut(), &json!({})).expect("report");
        assert_eq!(out["paks"].as_array().map(Vec::len), Some(0));
        assert_eq!(out["cooked_only"], false, "a dev run reads the cooked cache");
        cleanup(&root);
    }

    #[test]
    fn without_an_asset_server_calls_fail_cleanly() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        for (name, result) in [
            ("list", list(app.world_mut(), &json!({}))),
            ("describe", describe(app.world_mut(), &json!({ "asset": "x" }))),
            ("load", load(app.world_mut(), &json!({ "asset": "x" }))),
            ("reload", reload(app.world_mut(), &json!({ "asset": "x" }))),
            ("status", status(app.world_mut(), &json!({ "asset": "x" }))),
            ("cooked_pack", cooked_pack(app.world_mut(), &json!({}))),
        ] {
            let err = result.expect_err(name);
            assert_eq!(
                err.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("F0016"),
                "{name}"
            );
        }
    }
}

#[cfg(test)]
mod cooked_model_lods_protocol_tests {
    use super::*;

    #[test]
    fn cooked_model_all_lods_status_reports_every_authored_mesh() {
        use regia_asset::{AssetServerSettings, AssetTypeTag, CookedHeader, CookedModel, CookedModelPart};
        use regia_render::cooked_model::{load_cooked_models_all_lods, model_id};

        let root = regia_test_utils::TempDir::new("agent-all-lods");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let publish = |id, file: &[u8]| {
            let path = cache.join(regia_asset::cooked_file_name(id));
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, file).expect("write");
            std::fs::rename(tmp, path).expect("publish");
        };
        let gltf = AssetId::new_random();
        let meshes = [AssetId::new_random(), AssetId::new_random()];
        let info = regia_asset::GpuMeshInfo {
            aabb_min: [0.0; 3],
            aabb_max: [1.0; 3],
            bounding_sphere: [0.0, 0.0, 0.0, 1.0],
            lods: vec![],
        };
        for mesh in meshes {
            let file = regia_asset::encode_gpu_mesh_file(
                mesh,
                1,
                vec![],
                &info,
                &[0; regia_asset::GPU_VERTEX_STRIDE as usize * 3],
                &[0, 1, 2],
            )
            .expect("GPU mesh");
            publish(mesh, &file);
        }
        let model = CookedModel {
            version: regia_asset::COOKED_MODEL_VERSION,
            parts: meshes
                .into_iter()
                .enumerate()
                .map(|(lod, mesh)| CookedModelPart {
                    mesh,
                    material: None,
                    node: format!("Hull_LOD{lod}"),
                })
                .collect(),
        };
        let payload = model.to_postcard().expect("model");
        let header = CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::model().0,
            asset_id: model_id(gltf),
            importer_version: 1,
            dependencies: meshes.to_vec(),
            payload_len: payload.len() as u64,
        };
        publish(
            model_id(gltf),
            &regia_asset::encode_cooked_file(&header, &payload).expect("model file"),
        );
        AssetManifest {
            entries: meshes
                .iter()
                .enumerate()
                .map(|(lod, &id)| regia_asset::ManifestEntry {
                    path: format!("assets/hull.gltf#mesh/Hull_LOD{lod}/primitive_0"),
                    id,
                    type_path: "regia_asset::CookedMesh".into(),
                })
                .collect(),
        }
        .write_to_cache(&cache)
        .expect("manifest");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        let models = load_cooked_models_all_lods(&mut server, &[gltf]).expect("all LODs");
        assert_eq!(models[0].parts.len(), 2);
        let mut world = World::new();
        world.insert_resource(server);
        for mesh in meshes {
            let response = crate::dispatch::call_in_process(
                &mut world,
                "assets.status",
                json!({ "asset": mesh.to_string() }),
            )
            .expect("immediate status")
            .expect("status succeeds");
            assert_eq!(response["state"], "loaded");
            assert_eq!(response["id"], mesh.to_string());
        }
        let guide = crate::dispatch::call_in_process(
            &mut world,
            "agent.guide",
            json!({ "topic": "cooked-model-all-lods" }),
        )
        .expect("immediate guide")
        .expect("guide succeeds");
        assert!(
            guide["text"]
                .as_str()
                .expect("text")
                .contains("REGIA_PAK_GPU_MESHES")
        );
    }
}
