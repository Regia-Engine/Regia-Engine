// SPDX-License-Identifier: MPL-2.0
//! `render.get_visbuffer`: the visibility-buffer path as the game runs it
//! (`book/agents/visbuffer.md`, ADR-353, ADR-354): whether the profile turns it on, which meshes
//! the framework bridge (`regia_framework::meshlet_bridge`) put on it and why the others draw
//! forward, the cluster arena's occupancy and buffer creates, every GPU object the path created
//! in total and in the last frame (ADR-355), and the meshlet payloads that did not load.
//! `render.set_visbuffer` switches the path and its knobs, and both report how many mesh LOD
//! chains handed their meshes to the cluster LOD (ADR-1148). Which frames take the path and how
//! the resolve shades are the profile's `visibility_path` (ADR-2071).

use std::collections::HashSet;

use regia_app::prelude::*;
use regia_framework::meshlet_bridge::{MeshletAssets, MeshletBridgeState};
use regia_render::visbuffer::VisBufferSettings;
use regia_render::{
    ClusterLodHandoff, DenseSceneSelect, MeshRenderer, RenderSettings, RuntimeRenderer, VisibilityDefault,
    VisibilityPath, VisibilitySelect,
};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

/// `render.get_visbuffer`: read-only, answers in any world (null sections for what is absent).
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let enabled = world
        .get_resource::<RenderSettings>()
        .is_some_and(|s| s.profile.visibility_buffer);
    let (entities, distinct_meshes) = {
        let mut query = world.query::<&MeshRenderer>();
        let mut meshes = HashSet::new();
        let mut entities = 0usize;
        for renderer in query.iter(world) {
            entities += 1;
            meshes.insert(std::sync::Arc::as_ptr(&renderer.mesh) as usize);
        }
        (entities, meshes.len())
    };
    let state = world.get_resource::<MeshletBridgeState>();
    let on_entities = state.map_or(0, MeshletBridgeState::entities_on_visbuffer);
    let on_meshes = state.map_or(0, MeshletBridgeState::meshes_on_visbuffer);
    let mut reasons = Map::new();
    if enabled {
        let named = state.map_or(0, MeshletBridgeState::entities_with_meshlets);
        reasons.insert("no_meshlets".into(), json!(entities.saturating_sub(named)));
        for (reason, count) in state.map(MeshletBridgeState::forward_counts).unwrap_or_default() {
            reasons.insert(reason.code().into(), json!(count));
        }
    } else {
        reasons.insert("path_off".into(), json!(entities));
    }
    let assets = world.get_resource::<MeshletAssets>();
    let mut errors: Vec<Value> = assets
        .map(|a| {
            a.errors()
                .map(|e| json!({ "asset": e.id.to_string(), "code": e.code, "message": e.message }))
                .collect()
        })
        .unwrap_or_default();
    if let Some(state) = state {
        errors.extend(
            state
                .mismatches()
                .iter()
                .map(|e| json!({ "asset": e.id.to_string(), "code": e.code, "message": e.message })),
        );
    }
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| &r.0);
    let arena = renderer.map(|r| {
        let a = r.visbuffer_arena_stats();
        json!({
            "meshes": a.meshes,
            "pending": a.pending,
            "clusters": a.clusters,
            "cluster_capacity": a.cluster_capacity,
            "geometry_words": a.geometry_words,
            "geometry_capacity_words": a.geometry_capacity_words,
            "used_bytes": a.used_bytes,
            "free_bytes": a.free_bytes,
            "capacity_bytes": a.capacity_bytes,
            "free_ranges": a.free_ranges,
            "growths": a.growths,
            "buffer_creates": a.buffer_creates,
            "capped": a.capped,
        })
    });
    let last_frame = renderer.map(|r| {
        let s = r.visbuffer_stats();
        json!({
            "frames": s.frames,
            "instances": s.instances,
            "candidate_clusters": s.candidate_clusters,
            "materials": s.materials,
            "resident_meshes": s.resident_meshes,
            "geometry_bytes": s.geometry_bytes,
            "declined_meshes": s.declined_meshes,
            "receiver_instances": s.receiver_instances,
            "receiver_materials": s.receiver_materials,
            "masked_instances": s.masked_instances,
            "masked_materials": s.masked_materials,
            "masked_tested": s.masked_tested,
            "sway": {
                "instances": s.sway_instances,
                "materials": s.sway_materials,
                "moving": s.sway_moving,
                "forward": sway_forward_json(&s.sway_forward),
            },
            "lod_switches": s.lod_switches,
            // Per-view LOD history and its retirement (ADR-8620).
            "lod_history_discards": s.lod_history_discards,
            "lod_history_copies": s.lod_history_copies,
            "view_histories": s.view_histories,
            "view_history_bytes": s.view_history_bytes,
        })
    });
    let creates = renderer.map(|r| {
        let c = r.visbuffer_create_stats();
        json!({
            "total": creates_json(&c.total),
            "last_frame": creates_json(&c.last_frame),
            "frames": c.frames,
            "quiet_frames": c.quiet_frames,
        })
    });
    let stats = renderer.map(|r| r.visbuffer_arena_stats());
    let handoff = world.get_resource::<ClusterLodHandoff>().map(|h| {
        json!({
            "active": h.active,
            "chained": h.chained,
            "cluster_lod": h.cluster_lod,
        })
    });
    let selection = world
        .get_resource::<RenderSettings>()
        .map(|s| s.profile.visibility_path);
    let select = selection.map(|path| select_json(path, renderer));
    let resolve = renderer.map(|r| {
        let report = r.visbuffer_resolve_report();
        json!({
            "requested": selection.is_some_and(|p| p.single_pass_resolve),
            "single_pass": report.single_pass,
            "fallback": report.fallback.map(regia_render::SinglePassFallback::as_str),
            "draws": report.draws,
            "materials": report.materials,
            "lightmap_materials": report.lightmap_materials,
        })
    });
    let default = world.get_resource::<RenderSettings>().map(default_json);
    Ok(json!({
        "enabled": enabled,
        "active": renderer.is_some_and(|r| r.visbuffer_active()),
        "settings": renderer.map(|r| settings_json(&r.visbuffer_settings())),
        "default": default,
        "select": select,
        "resolve": resolve,
        "lod_handoff": handoff,
        "bridge": state.is_some(),
        "meshes": {
            "visbuffer": on_meshes,
            "forward": distinct_meshes.saturating_sub(on_meshes),
        },
        "entities": {
            "total": entities,
            "visbuffer": on_entities,
            "forward": entities.saturating_sub(on_entities),
            "forward_reasons": reasons,
        },
        "clusters_resident": stats.map_or(0, |a| a.clusters),
        "buffer_creates": stats.map_or(0, |a| a.buffer_creates),
        "arena": arena,
        "load_errors": assets.map_or(0, MeshletAssets::failures),
        "errors": errors,
        "last_frame": last_frame,
        "creates": creates,
    }))
}

/// The `render.set_visbuffer` parameters besides `enabled`.
const KNOBS: [&str; 7] = [
    "lod_threshold_px",
    "shadow_lod_threshold_px",
    "max_clusters",
    "occlusion",
    "cluster_shadows",
    "software_raster",
    "software_raster_max_px",
];

/// Swaying meshes that stayed forward last frame, by reason (ADR-3916): only the reasons that happened.
fn sway_forward_json(forward: &[u32]) -> Value {
    let reasons: serde_json::Map<String, Value> = regia_render::visbuffer::sway::SwaySkip::ALL
        .iter()
        .zip(forward)
        .filter(|&(_, &count)| count > 0)
        .map(|(skip, &count)| (skip.code().to_string(), json!(count)))
        .collect();
    Value::Object(reasons)
}

fn settings_json(s: &VisBufferSettings) -> Value {
    json!({
        "lod_threshold_px": s.lod_threshold_px,
        "lod_hysteresis_px": s.resolved_lod_hysteresis_px(),
        "lod_hysteresis": if s.lod_hysteresis_px.is_some() { "pixels" } else { "fraction" },
        "shadow_lod_threshold_px": s.shadow_lod_threshold_px,
        "max_clusters": s.max_clusters,
        "occlusion": s.occlusion,
        "cluster_shadows": s.cluster_shadows,
        "software_raster": s.software_raster,
        "software_raster_max_px": s.software_raster_max_px,
    })
}

/// The profile's selection, and what the renderer's gate did with it (`null` counters without a
/// renderer).
fn select_json(path: VisibilityPath, renderer: Option<&regia_render::FrameRenderer>) -> Value {
    let dense = match path.select {
        VisibilitySelect::Always => Value::Null,
        VisibilitySelect::Dense(select) => json!({
            "enter_triangles": select.enter_triangles,
            "enter_instances": select.enter_instances,
            "enter_frames": select.enter_frames,
            "leave_frames": select.leave_frames,
        }),
    };
    let gate = renderer.map(|r| {
        let report = r.visbuffer_dense_report();
        json!({
            "dense": report.dense,
            "triangles": report.density.triangles,
            "instances": report.density.instances,
            "entered": report.entered,
            "left": report.left,
        })
    });
    json!({ "mode": path.select.as_str(), "dense": dense, "gate": gate })
}

/// The dense-scene default (ADR-3149): the switch and whether it holds the profile's path.
fn default_json(settings: &RenderSettings) -> Value {
    json!({
        "setting": settings.visibility_default.as_str(),
        "state": settings.visibility_default_state().as_str(),
    })
}

/// What `render.set_visbuffer` reports before and after a change.
fn switch_state(world: &World) -> Value {
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| &r.0);
    let settings = world.get_resource::<RenderSettings>();
    json!({
        "enabled": settings.is_some_and(|s| s.profile.visibility_buffer),
        "active": renderer.is_some_and(|r| r.visbuffer_active()),
        "settings": renderer.map(|r| settings_json(&r.visbuffer_settings())),
        "select": settings.map(|s| s.profile.visibility_path.select.as_str()),
        "single_pass_resolve": settings.map(|s| s.profile.visibility_path.single_pass_resolve),
        "default": settings.map(default_json),
        // Masked (alpha-tested) instances of the last visibility frame (ADR-3915).
        "masked": renderer.map(|r| {
            let s = r.visbuffer_stats();
            json!({
                "instances": s.masked_instances,
                "materials": s.masked_materials,
                "tested": s.masked_tested,
            })
        }),
    })
}

/// The `render.set_visbuffer` parameters that edit the profile's `visibility_path` (ADR-2071).
const PATH_PARAMS: [&str; 4] = [
    "select",
    "dense_enter_triangles",
    "dense_enter_instances",
    "single_pass_resolve",
];

/// The profile's `visibility_path` after `params`, or an error naming the bad parameter. A density
/// threshold with `select` left out (or `always`) is refused: it would do nothing.
fn path_after(current: VisibilityPath, params: &Value) -> Result<VisibilityPath, RpcError> {
    let single_pass_resolve =
        bool_param(params, "single_pass_resolve")?.unwrap_or(current.single_pass_resolve);
    let triangles = match params.get("dense_enter_triangles") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().filter(|n| *n >= 1).ok_or_else(|| {
            RpcError::invalid_params("`dense_enter_triangles` must be an integer of at least 1")
        })?),
    };
    let instances = match params.get("dense_enter_instances") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| *n >= 1)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    RpcError::invalid_params(
                        "`dense_enter_instances` must be an integer from 1 to 4294967295",
                    )
                })?,
        ),
    };
    let dense_now = match current.select {
        VisibilitySelect::Dense(select) => Some(select),
        VisibilitySelect::Always => None,
    };
    let dense_with_thresholds = |base: DenseSceneSelect| {
        VisibilitySelect::Dense(DenseSceneSelect {
            enter_triangles: triangles.unwrap_or(base.enter_triangles),
            enter_instances: instances.unwrap_or(base.enter_instances),
            ..base
        })
    };
    let thresholds = triangles.is_some() || instances.is_some();
    let select = match params.get("select") {
        None | Some(Value::Null) => match dense_now {
            Some(base) => dense_with_thresholds(base),
            None if thresholds => {
                return Err(RpcError::invalid_params(
                    "`dense_enter_triangles` and `dense_enter_instances` need `select`: \"dense\"",
                ));
            }
            None => VisibilitySelect::Always,
        },
        Some(Value::String(name)) if name == "always" => {
            if thresholds {
                return Err(RpcError::invalid_params(
                    "`dense_enter_triangles` and `dense_enter_instances` need `select`: \"dense\"",
                ));
            }
            VisibilitySelect::Always
        }
        Some(Value::String(name)) if name == "dense" => {
            dense_with_thresholds(dense_now.unwrap_or(DenseSceneSelect::DEFAULT))
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`select` must be \"always\" or \"dense\"",
            ));
        }
    };
    Ok(VisibilityPath {
        select,
        single_pass_resolve,
    })
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean"))),
    }
}

/// A pixel or texel threshold: finite, above 0, at most 1024.
fn threshold_param(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x > 0.0 && *x <= 1024.0)
            .map(|x| Some(x as f32))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{name}` must be a number above 0 and at most 1024"))
            }),
    }
}

/// `render.set_visbuffer`: turns the active profile's `visibility_buffer` on or off (`enabled`)
/// and sets the runtime renderer's visibility-buffer knobs (the other parameters). Every
/// parameter is optional; nothing is applied unless all of them are valid. Returns the state
/// before and after (`active` turns true at the first frame drawn through the path).
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(object) = params.as_object()
        && let Some(unknown) = object.keys().find(|k| {
            !matches!(k.as_str(), "enabled" | "default")
                && !KNOBS.contains(&k.as_str())
                && !PATH_PARAMS.contains(&k.as_str())
        })
    {
        return Err(RpcError::invalid_params(format!(
            "unknown parameter `{unknown}`: pass enabled, default, {}, {}",
            KNOBS.join(", "),
            PATH_PARAMS.join(", ")
        )));
    }
    let enabled = bool_param(params, "enabled")?;
    let default = match params.get("default") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(VisibilityDefault::from_name)
                .ok_or_else(|| RpcError::invalid_params("`default` must be \"preset\" or \"dense\""))?,
        ),
    };
    let lod = threshold_param(params, "lod_threshold_px")?;
    let shadow_lod = threshold_param(params, "shadow_lod_threshold_px")?;
    let max_clusters = match params.get("max_clusters") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| *n >= 1)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    RpcError::invalid_params("`max_clusters` must be an integer from 1 to 4294967295")
                })?,
        ),
    };
    let occlusion = bool_param(params, "occlusion")?;
    let cluster_shadows = bool_param(params, "cluster_shadows")?;
    let software_raster = bool_param(params, "software_raster")?;
    let software_max_px = match params.get("software_raster_max_px") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=64).contains(n))
                .map(|n| n as u32)
                .ok_or_else(|| {
                    RpcError::invalid_params("`software_raster_max_px` must be an integer from 1 to 64")
                })?,
        ),
    };
    let knobs = lod.is_some()
        || shadow_lod.is_some()
        || max_clusters.is_some()
        || occlusion.is_some()
        || cluster_shadows.is_some()
        || software_raster.is_some()
        || software_max_px.is_some();
    let path_asked = PATH_PARAMS
        .iter()
        .any(|name| params.get(*name).is_some_and(|v| !v.is_null()));
    if (enabled.is_some() || path_asked || default.is_some()) && !world.contains_resource::<RenderSettings>()
    {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             FrameQuality::visibility_buffer)",
        ));
    }
    if knobs && !world.contains_resource::<RuntimeRenderer>() {
        return Err(RpcError::not_found(
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
             FrameRenderer::set_visbuffer_settings)",
        ));
    }
    // The path parameters apply over what the default writes, so the default is validated first
    // and the path is computed from the profile as the default leaves it.
    let path = world
        .get_resource::<RenderSettings>()
        .map(|settings| {
            let mut preview = settings.clone();
            if let Some(default) = default {
                preview.set_visibility_default(default);
            }
            path_after(preview.profile.visibility_path, params)
        })
        .transpose()?;
    let before = switch_state(world);
    if let Some(default) = default
        && let Some(mut settings) = world.get_resource_mut::<RenderSettings>()
    {
        settings.set_visibility_default(default);
    }
    if path_asked
        && let (Some(path), Some(mut settings)) = (path, world.get_resource_mut::<RenderSettings>())
        && settings.profile.visibility_path != path
    {
        settings.profile.visibility_path = path;
    }
    if let Some(enabled) = enabled
        && let Some(mut settings) = world.get_resource_mut::<RenderSettings>()
        && settings.profile.visibility_buffer != enabled
    {
        settings.profile.visibility_buffer = enabled;
    }
    if knobs && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
        let current = renderer.0.visbuffer_settings();
        let next = VisBufferSettings {
            lod_threshold_px: lod.unwrap_or(current.lod_threshold_px),
            lod_hysteresis_px: current.lod_hysteresis_px,
            shadow_lod_threshold_px: shadow_lod.unwrap_or(current.shadow_lod_threshold_px),
            max_clusters: max_clusters.unwrap_or(current.max_clusters),
            occlusion: occlusion.unwrap_or(current.occlusion),
            cluster_shadows: cluster_shadows.unwrap_or(current.cluster_shadows),
            software_raster: software_raster.unwrap_or(current.software_raster),
            software_raster_max_px: software_max_px.unwrap_or(current.software_raster_max_px),
            residency: current.residency,
        };
        renderer.0.set_visbuffer_settings(next);
    }
    let after = switch_state(world);
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next frame. The path draws a frame only when the profile also has a \
                 depth prepass, a post stack (linear HDR) and no MSAA, the device supports it, and some \
                 mesh has meshlets; read render.get_visbuffer `active` after a few frames. Under select \
                 dense it also waits for a dense scene (select.gate). single_pass_resolve needs a device \
                 on the texture table, chosen at creation (resolve.fallback says when it is not). A \
                 preset change (render.set_profile) replaces the profile and its visibility_buffer; \
                 `default` (\"dense\" or \"preset\") is kept across preset changes and holds the path \
                 only on Desktop High and Ultra (default.state says why not)",
    }))
}

/// `render.vis_lod_hysteresis`: read or set the cluster LOD band, and read last frame's switch count.
///
/// `band_px` absent reads. `null` restores the fraction of the threshold. `0` is the hard cut. A
/// positive number is the band in pixels. The call does not clear stored levels (`cleared` is false).
pub(crate) fn lod_hysteresis(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(object) = params.as_object() else {
        return Err(lod_err(
            "vis_lod_hysteresis_bad_params",
            "parameters must be an object",
            "pass { } to read, or { \"band_px\": number | null }",
        ));
    };
    enum BandOp {
        Get,
        Reset,
        Set(f32),
    }
    let op = match object.get("band_px") {
        None => BandOp::Get,
        Some(Value::Null) => BandOp::Reset,
        Some(Value::Number(number)) => {
            let Some(px) = number.as_f64() else {
                return Err(lod_err(
                    "vis_lod_hysteresis_bad_band",
                    "`band_px` is not a finite number",
                    "use a number from 0 to 3.4e38, null for the fraction, or omit it to read",
                ));
            };
            if !px.is_finite() || px < 0.0 || px > f64::from(f32::MAX) {
                return Err(lod_err(
                    "vis_lod_hysteresis_bad_band",
                    "`band_px` must be a finite number of 0 or more",
                    "0 is the hard cut; null restores the fraction of the threshold",
                ));
            }
            BandOp::Set(px as f32)
        }
        Some(_) => {
            return Err(lod_err(
                "vis_lod_hysteresis_bad_params",
                "`band_px` must be a number or null",
                "omit band_px to read the band",
            ));
        }
    };
    let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
        return Err(lod_err(
            "vis_lod_hysteresis_no_renderer",
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
             VisBufferSettings::lod_hysteresis_px)",
        ));
    };
    match op {
        BandOp::Get => {}
        BandOp::Reset => {
            let mut settings = renderer.0.visbuffer_settings();
            settings.lod_hysteresis_px = None;
            renderer.0.set_visbuffer_settings(settings);
        }
        BandOp::Set(px) => {
            let mut settings = renderer.0.visbuffer_settings();
            settings.lod_hysteresis_px = Some(px);
            renderer.0.set_visbuffer_settings(settings);
        }
    }
    let settings = renderer.0.visbuffer_settings();
    let switches = renderer.0.visbuffer_stats().lod_switches;
    Ok(json!({
        "band_px": settings.resolved_lod_hysteresis_px(),
        "band_source": if settings.lod_hysteresis_px.is_some() { "pixels" } else { "fraction" },
        "fraction": regia_render::visbuffer::LOD_HYSTERESIS_FRACTION,
        "threshold_px": settings.lod_threshold_px,
        "switches": switches,
        "cleared": false,
    }))
}

fn lod_err(code: &'static str, message: &str, hint: &str) -> RpcError {
    RpcError::new(crate::protocol::INVALID_PARAMS, code, message, hint)
}

/// One `VisBufferCreates` as JSON; `objects` is pipelines + bind groups + buffers + textures.
fn creates_json(c: &regia_render::visbuffer::VisBufferCreates) -> Value {
    json!({
        "objects": c.objects(),
        "pipelines": c.pipelines,
        "bind_groups": c.bind_groups,
        "buffers": c.buffers,
        "buffer_bytes": c.buffer_bytes,
        "textures": c.textures,
        "texture_bytes": c.texture_bytes,
        "upload_bytes": c.upload_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn answers_in_a_bare_world() {
        let mut world = World::new();
        let got = get(&mut world).expect("answers");
        assert_eq!(got["enabled"], false);
        assert_eq!(got["bridge"], false);
        assert!(got["arena"].is_null());
        assert!(got["creates"].is_null());
        assert_eq!(got["entities"]["total"], 0);
        assert_eq!(got["load_errors"], 0);
    }

    #[test]
    fn counts_forward_meshes_when_the_path_is_off_and_on() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.insert_resource(MeshletBridgeState::default());
        world.insert_resource(MeshletAssets::default());
        let mesh = Arc::new(regia_render::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        for _ in 0..3 {
            world.spawn(MeshRenderer::new(
                Arc::clone(&mesh),
                Arc::new(regia_render::CpuMaterial::default()),
            ));
        }
        world.insert_resource(RenderSettings::default());
        let off = get(&mut world).expect("answers");
        assert_eq!(off["entities"]["forward_reasons"]["path_off"], 3);
        assert_eq!(off["meshes"]["forward"], 1, "three entities share one mesh");

        let mut settings = RenderSettings::default();
        settings.profile.visibility_buffer = true;
        world.insert_resource(settings);
        let on = get(&mut world).expect("answers");
        assert_eq!(on["enabled"], true);
        assert_eq!(on["entities"]["forward_reasons"]["no_meshlets"], 3);
        assert_eq!(on["entities"]["forward_reasons"]["skinned"], 0);
        assert_eq!(on["arena"]["buffer_creates"], 0);
        assert_eq!(on["arena"]["capacity_bytes"], 0);
        // A renderer that has drawn nothing: every create counter is 0, no frame counted.
        assert_eq!(on["creates"]["total"]["objects"], 0);
        assert_eq!(on["creates"]["total"]["buffers"], 0);
        assert_eq!(on["creates"]["last_frame"]["upload_bytes"], 0);
        assert_eq!(on["creates"]["frames"], 0);
        assert_eq!(on["creates"]["quiet_frames"], 0);
        // The lightmap receiver counts (ADR-2689): none before a frame.
        assert_eq!(on["last_frame"]["receiver_instances"], 0);
        assert_eq!(on["last_frame"]["receiver_materials"], 0);
        assert_eq!(on["last_frame"]["masked_instances"], 0);
        assert_eq!(on["last_frame"]["masked_materials"], 0);
        assert_eq!(on["last_frame"]["masked_tested"], false);
        // The sway counts (ADR-3916): none before a frame, and no reason listed.
        assert_eq!(on["last_frame"]["sway"]["instances"], 0);
        assert_eq!(on["last_frame"]["sway"]["materials"], 0);
        assert_eq!(on["last_frame"]["sway"]["moving"], false);
        assert_eq!(on["last_frame"]["sway"]["forward"], json!({}));
        // No frame drew, so the path is not active; the knobs are the defaults.
        assert_eq!(on["active"], false);
        assert_eq!(on["settings"]["cluster_shadows"], true);
        assert_eq!(on["settings"]["max_clusters"], 1 << 20);
        assert!(
            on["lod_handoff"].is_null(),
            "no selector counters without the render plugin"
        );
        world.insert_resource(ClusterLodHandoff {
            active: true,
            chained: 5,
            cluster_lod: 3,
        });
        let counted = get(&mut world).expect("answers");
        assert_eq!(counted["lod_handoff"]["active"], true);
        assert_eq!(counted["lod_handoff"]["chained"], 5);
        assert_eq!(counted["lod_handoff"]["cluster_lod"], 3);
    }

    /// The agent reads the renderer's live sway counts through the protocol's request path.
    #[test]
    fn a_rendered_swaying_set_is_reported_through_the_protocol() {
        let device = match crate::seeing::headless_render_device() {
            Ok(device) => device,
            Err(error) => {
                eprintln!("skip rendered sway protocol check: {error}");
                return;
            }
        };
        let vertices: Vec<_> = [[-0.5, 0.0, 0.0], [0.5, 0.0, 0.0], [0.0, 2.0, 0.0]]
            .map(|position| regia_render::LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], [0.0; 2]))
            .into();
        let indices = [0, 1, 2];
        let mut material = regia_render::FoliageSway::default()
            .adopt(&regia_render::LabMaterial {
                double_sided: true,
                ..regia_render::LabMaterial::default()
            })
            .expect("adopt");
        Arc::make_mut(material.surface_graph.as_mut().expect("sway graph")).params[0] = [0.7, 0.6, 8.0, 2.0];
        let models = [regia_core::glam::Mat4::IDENTITY.to_cols_array_2d()];
        let mut renderer = regia_render::FrameRenderer::new();
        renderer.set_meshlet_builder(Some(Arc::new(
            |vertices: &[regia_render::LabVertex], indices: &[u32]| {
                let positions: Vec<_> = vertices.iter().map(|vertex| vertex.position).collect();
                Some(regia_render::visbuffer::MeshletMesh::single_level(
                    &positions, indices,
                ))
            },
        )));
        assert!(renderer.set_instance_set(
            1,
            &regia_render::VisInstanceSet {
                mesh_key: 1,
                vertices: &vertices,
                indices: &indices,
                material_key: 2,
                material: &material,
                models: &models,
                shadow: regia_render::ShadowCast::Off,
                instance_shadows: &[],
            },
        ));
        let mut settings = RenderSettings::default();
        settings.profile.visibility_buffer = true;
        let quality = regia_render::FrameQuality::from_profile(&settings.profile);
        let mut scene = regia_test_utils::scenes::primitives_row_scene();
        scene.width = 64;
        scene.height = 64;
        scene.eye = [0.0, 1.0, 5.0];
        scene.target = [0.0, 1.0, 0.0];
        scene.directional_shadows = false;
        scene.materials.clear();
        scene.meshes.clear();
        renderer
            .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
            .expect("render swaying set");
        let counters = renderer
            .visbuffer_counters(&device)
            .expect("read counters")
            .expect("frame");
        assert!(counters.phase1_clusters + counters.phase2_clusters > 0);
        let mut world = World::new();
        world.insert_resource(settings);
        world.insert_resource(RuntimeRenderer(renderer));
        let got = crate::dispatch::call_in_process(&mut world, "render.get_visbuffer", json!({}))
            .expect("immediate protocol response")
            .expect("successful protocol response");
        assert_eq!(got["active"], true);
        assert_eq!(got["last_frame"]["sway"]["instances"], 1);
        assert_eq!(got["last_frame"]["sway"]["materials"], 1);
        assert_eq!(got["last_frame"]["sway"]["moving"], true);
        assert_eq!(got["last_frame"]["sway"]["forward"], json!({}));
    }

    #[test]
    fn set_switches_the_profile_and_the_knobs() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.insert_resource(RenderSettings::default());
        let got = set(
            &mut world,
            &json!({ "enabled": true, "lod_threshold_px": 2.5, "cluster_shadows": false }),
        )
        .expect("set");
        assert_eq!(got["before"]["enabled"], false);
        assert_eq!(got["after"]["enabled"], true);
        assert_eq!(got["after"]["active"], false, "no frame drew yet");
        assert_eq!(got["after"]["masked"]["instances"], 0, "masked counts (ADR-3915)");
        assert_eq!(got["after"]["masked"]["tested"], false);
        assert_eq!(got["after"]["settings"]["lod_threshold_px"], 2.5);
        assert_eq!(got["after"]["settings"]["cluster_shadows"], false);
        assert_eq!(
            got["after"]["settings"]["occlusion"], true,
            "untouched knobs keep their value"
        );
        assert!(world.resource::<RenderSettings>().profile.visibility_buffer);
        let settings = world.resource::<RuntimeRenderer>().0.visbuffer_settings();
        assert_eq!(settings.lod_threshold_px, 2.5);
        assert!(!settings.cluster_shadows);
        let got = set(
            &mut world,
            &json!({ "software_raster": true, "software_raster_max_px": 24 }),
        )
        .expect("set");
        assert_eq!(got["after"]["settings"]["software_raster"], true);
        assert_eq!(got["after"]["settings"]["software_raster_max_px"], 24);
        // specs/98 § 8a: at most 64 pixels; out of range is F0002 and leaves the knob alone.
        for bad in [json!(65), json!(0), json!(-1), json!(16.5), json!("16")] {
            let err = set(&mut world, &json!({ "software_raster_max_px": bad }))
                .expect_err("out-of-range software_raster_max_px");
            assert_eq!(
                err.data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str),
                Some("F0002"),
                "software_raster_max_px {bad}"
            );
        }
        let at_max = set(&mut world, &json!({ "software_raster_max_px": 64 })).expect("64");
        assert_eq!(at_max["before"]["settings"]["software_raster_max_px"], 24);
        assert_eq!(at_max["after"]["settings"]["software_raster_max_px"], 64);
        assert!(set(&mut world, &json!({ "software_raster": 1 })).is_err());
        assert_eq!(settings.max_clusters, VisBufferSettings::default().max_clusters);

        // No parameters: a read of the current state.
        let same = set(&mut world, &json!({})).expect("empty");
        assert_eq!(same["before"], same["after"]);
        let off = set(&mut world, &json!({ "enabled": false, "max_clusters": 4096 })).expect("off");
        assert_eq!(off["after"]["enabled"], false);
        assert_eq!(off["after"]["settings"]["max_clusters"], 4096);
    }

    #[test]
    fn sway_forward_lists_the_reasons_that_happened_by_their_codes() {
        use regia_render::visbuffer::sway::SwaySkip;
        let mut forward = [0u32; SwaySkip::ALL.len()];
        forward[SwaySkip::GraphSurface.index()] = 3;
        forward[SwaySkip::HashedAlpha.index()] = 2;
        assert_eq!(
            sway_forward_json(&forward),
            json!({ "graph_surface": 3, "hashed_alpha": 2 })
        );
    }

    #[test]
    fn set_refuses_bad_values_without_applying_any() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.insert_resource(RenderSettings::default());
        for bad in [
            json!({ "enabled": true, "lod_threshold_px": 0.0 }),
            json!({ "enabled": true, "lod_threshold_px": -1.0 }),
            json!({ "enabled": true, "shadow_lod_threshold_px": 5000.0 }),
            json!({ "enabled": true, "max_clusters": 0 }),
            json!({ "enabled": true, "max_clusters": 1.5 }),
            json!({ "enabled": true, "max_clusters": 5_000_000_000u64 }),
            json!({ "enabled": "yes" }),
            json!({ "enabled": true, "occlusion": 1 }),
            json!({ "enabled": true, "lod_threshold": 2.0 }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad} must be refused");
        }
        assert!(
            !world.resource::<RenderSettings>().profile.visibility_buffer,
            "a refused call applies nothing"
        );
        assert_eq!(
            world.resource::<RuntimeRenderer>().0.visbuffer_settings(),
            VisBufferSettings::default()
        );
    }

    #[test]
    fn set_without_the_render_plugin_says_what_to_add() {
        let mut world = World::new();
        let err = set(&mut world, &json!({ "enabled": true })).expect_err("no settings");
        assert!(format!("{err:?}").contains("RenderPlugin"));
        world.insert_resource(RenderSettings::default());
        let err = set(&mut world, &json!({ "occlusion": false })).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
        // The profile switch alone needs no renderer.
        let got = set(&mut world, &json!({ "enabled": true })).expect("profile only");
        assert_eq!(got["after"]["enabled"], true);
        assert!(got["after"]["settings"].is_null());
    }

    fn world_with_settings() -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        world
    }

    #[test]
    fn the_default_profile_selects_nothing_new() {
        let mut world = world_with_settings();
        world.insert_resource(RuntimeRenderer::default());
        let got = get(&mut world).expect("answers");
        assert_eq!(got["select"]["mode"], "always");
        assert!(got["select"]["dense"].is_null());
        assert_eq!(got["select"]["gate"]["entered"], 0);
        assert_eq!(got["resolve"]["requested"], false);
        assert_eq!(got["resolve"]["single_pass"], false);
        assert!(got["resolve"]["fallback"].is_null());
    }

    #[test]
    fn select_and_single_pass_edit_the_profile_and_answer_both_ways() {
        let mut world = world_with_settings();
        let done = set(
            &mut world,
            &json!({ "select": "dense", "dense_enter_instances": 12, "single_pass_resolve": true }),
        )
        .expect("set");
        assert_eq!(done["before"]["select"], "always");
        assert_eq!(done["after"]["select"], "dense");
        assert_eq!(done["after"]["single_pass_resolve"], true);
        let path = world.resource::<RenderSettings>().profile.visibility_path;
        let VisibilitySelect::Dense(select) = path.select else {
            panic!("{path:?}");
        };
        assert_eq!(select.enter_instances, 12);
        assert_eq!(select.enter_triangles, DenseSceneSelect::DEFAULT.enter_triangles);
        // A threshold alone edits the selection that is on, and keeps the other choices.
        set(&mut world, &json!({ "dense_enter_triangles": 7 })).expect("threshold");
        let path = world.resource::<RenderSettings>().profile.visibility_path;
        let VisibilitySelect::Dense(select) = path.select else {
            panic!("{path:?}");
        };
        assert_eq!((select.enter_triangles, select.enter_instances), (7, 12));
        assert!(path.single_pass_resolve);
        let got = get(&mut world).expect("answers");
        assert_eq!(got["select"]["dense"]["enter_triangles"], 7);
        // And back.
        set(
            &mut world,
            &json!({ "select": "always", "single_pass_resolve": false }),
        )
        .expect("back");
        assert_eq!(
            world.resource::<RenderSettings>().profile.visibility_path,
            VisibilityPath::DEFAULT
        );
    }

    #[test]
    fn bad_path_parameters_change_nothing() {
        let mut world = world_with_settings();
        for bad in [
            json!({ "select": "sometimes" }),
            json!({ "select": 3 }),
            json!({ "dense_enter_triangles": 5 }),
            json!({ "select": "always", "dense_enter_instances": 5 }),
            json!({ "select": "dense", "dense_enter_triangles": 0 }),
            json!({ "select": "dense", "dense_enter_instances": -1 }),
            json!({ "enabled": true, "select": "dense", "single_pass_resolve": "yes" }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
            let profile = &world.resource::<RenderSettings>().profile;
            assert_eq!(profile.visibility_path, VisibilityPath::DEFAULT, "{bad}");
            assert!(!profile.visibility_buffer, "{bad}");
        }
    }

    #[test]
    fn without_settings_the_path_parameters_are_refused() {
        let mut world = World::new();
        assert!(set(&mut world, &json!({ "select": "dense" })).is_err());
        assert!(get(&mut world).expect("answers")["select"].is_null());
        assert!(set(&mut world, &json!({ "default": "dense" })).is_err());
        assert!(get(&mut world).expect("answers")["default"].is_null());
    }

    /// ADR-3149: `default` turns the dense-scene default on and off, reports why it holds the path
    /// or not, and other path parameters in the same call apply over it.
    #[test]
    fn the_dense_default_switches_and_reports_its_state() {
        use regia_render::RenderProfilePreset;
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let got = get(&mut world).expect("answers");
        assert_eq!(got["default"]["setting"], "preset");
        assert_eq!(got["default"]["state"], "switch_off");

        let done = set(&mut world, &json!({ "default": "dense" })).expect("on");
        assert_eq!(done["before"]["default"]["state"], "switch_off");
        assert_eq!(done["after"]["default"]["state"], "applied");
        assert_eq!(done["after"]["enabled"], true);
        assert_eq!(done["after"]["select"], "dense");
        assert_eq!(done["after"]["single_pass_resolve"], false);
        let settings = world.resource::<RenderSettings>();
        assert_eq!(settings.profile.visibility_path, VisibilityPath::DENSE_DEFAULT);
        assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopHigh));
        let got = get(&mut world).expect("answers");
        assert_eq!(
            got["select"]["dense"]["enter_triangles"],
            DenseSceneSelect::MEASURED.enter_triangles
        );

        // Off restores the preset's row.
        set(&mut world, &json!({ "default": "preset" })).expect("off");
        let settings = world.resource::<RenderSettings>();
        assert!(!settings.profile.visibility_buffer);
        assert_eq!(settings.profile.visibility_path, VisibilityPath::DEFAULT);

        // On together with the single pass: the parameter applies over the default, and the
        // profile is then the game's choice, which the default no longer claims.
        let done = set(
            &mut world,
            &json!({ "default": "dense", "single_pass_resolve": true }),
        )
        .expect("both");
        assert_eq!(done["after"]["single_pass_resolve"], true);
        assert_eq!(done["after"]["default"]["state"], "profile_choice");

        // An unmeasured preset: the switch stays on, the profile is untouched.
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileHigh));
        let done = set(&mut world, &json!({ "default": "dense" })).expect("mobile");
        assert_eq!(done["after"]["default"]["state"], "preset_unmeasured");
        assert_eq!(done["after"]["enabled"], false);
    }

    #[test]
    fn a_bad_default_changes_nothing() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(
            regia_render::RenderProfilePreset::DesktopHigh,
        ));
        for bad in [
            json!({ "default": "always" }),
            json!({ "default": true }),
            json!({ "default": "dense", "select": "sometimes" }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
            let settings = world.resource::<RenderSettings>();
            assert_eq!(settings.visibility_default, VisibilityDefault::Preset, "{bad}");
            assert!(!settings.profile.visibility_buffer, "{bad}");
        }
    }

    fn error_code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn vis_lod_hysteresis_is_available_through_the_agent_dispatch() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.insert_resource(regia_app::Time::default());
        for params in [json!({ "band_px": 0.25 }), json!({}), json!({ "band_px": null })] {
            let result = crate::dispatch::call_in_process(&mut world, "render.vis_lod_hysteresis", params)
                .expect("synchronous agent method")
                .expect("registered agent method");
            assert_eq!(result["switches"], 0);
            assert_eq!(result["cleared"], false);
        }
        let result = crate::dispatch::call_in_process(&mut world, "render.get_visbuffer", json!({}))
            .expect("synchronous visibility report")
            .expect("visibility report");
        assert_eq!(result["last_frame"]["lod_switches"], 0);
    }

    /// ADR-8620: `render.get_visbuffer` reports the per-view LOD history counters through the
    /// dispatcher (a renderer that drew nothing: no discards, no copies, no views kept aside).
    #[test]
    fn per_view_lod_history_counters_are_reported_through_the_agent_dispatch() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.insert_resource(regia_app::Time::default());
        let result = crate::dispatch::call_in_process(&mut world, "render.get_visbuffer", json!({}))
            .expect("synchronous visibility report")
            .expect("visibility report");
        let last = &result["last_frame"];
        for key in [
            "lod_history_discards",
            "lod_history_copies",
            "view_histories",
            "view_history_bytes",
        ] {
            assert_eq!(last[key], 0, "{key}: {last}");
        }
    }

    #[test]
    fn vis_lod_hysteresis_reads_and_sets_the_band() {
        let _method = "render.vis_lod_hysteresis";
        let mut bare = World::new();
        let err = lod_hysteresis(&mut bare, &json!({})).expect_err("no renderer");
        assert_eq!(error_code(&err), "vis_lod_hysteresis_no_renderer");
        for bad in [json!(null), json!([]), json!("x"), json!(5), json!(true)] {
            let err = lod_hysteresis(&mut bare, &bad).expect_err("not an object");
            assert_eq!(error_code(&err), "vis_lod_hysteresis_bad_params", "{bad}");
        }

        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        let err = lod_hysteresis(&mut world, &json!({ "band_px": "wide" })).expect_err("type");
        assert_eq!(error_code(&err), "vis_lod_hysteresis_bad_params");
        let err = lod_hysteresis(&mut world, &json!({ "band_px": -0.1 })).expect_err("negative");
        assert_eq!(error_code(&err), "vis_lod_hysteresis_bad_band");

        let configured = lod_hysteresis(&mut world, &json!({ "band_px": 0.25 })).expect("set");
        assert_eq!(configured["band_source"], "pixels");
        assert!((configured["band_px"].as_f64().unwrap_or(0.0) - 0.25).abs() < 1e-6);
        assert_eq!(configured["switches"], 0);
        assert_eq!(configured["cleared"], false);
        let got = lod_hysteresis(&mut world, &json!({})).expect("get");
        assert_eq!(got["band_source"], "pixels");
        assert!((got["band_px"].as_f64().unwrap_or(0.0) - 0.25).abs() < 1e-6);

        let reset = lod_hysteresis(&mut world, &json!({ "band_px": null })).expect("reset");
        assert_eq!(reset["band_source"], "fraction");
        let fraction = f64::from(regia_render::visbuffer::LOD_HYSTERESIS_FRACTION);
        let threshold = reset["threshold_px"].as_f64().unwrap_or(0.0);
        assert!((reset["band_px"].as_f64().unwrap_or(0.0) - fraction * threshold).abs() < 1e-5);

        let hard = lod_hysteresis(&mut world, &json!({ "band_px": 0 })).expect("hard cut");
        assert_eq!(hard["band_source"], "pixels");
        assert_eq!(hard["band_px"], 0.0);

        lod_hysteresis(&mut world, &json!({ "band_px": 0.25 })).expect("set again");
        set(&mut world, &json!({ "lod_threshold_px": 2.0 })).expect("knobs keep the band");
        world.insert_resource(RenderSettings::default());
        let kept = lod_hysteresis(&mut world, &json!({})).expect("kept");
        assert_eq!(kept["band_source"], "pixels");
        assert!((kept["band_px"].as_f64().unwrap_or(0.0) - 0.25).abs() < 1e-6);
        assert!((kept["threshold_px"].as_f64().unwrap_or(0.0) - 2.0).abs() < 1e-6);
    }
}
