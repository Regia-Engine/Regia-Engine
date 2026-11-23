// SPDX-License-Identifier: MPL-2.0
//! `level.get_look` / `level.set_look`: the map's exposure meter, colour grade and local exposure
//! (`LevelSettings.look`, `docs/specs/13-renderer.md` § 5 step 12; `docs/specs/23-agent-native.md` § 3).
//!
//! The look is plain serde data, so the agent reads it as JSON and writes a partial object
//! that is deep-merged into the current value: `{ "exposure_bias_ev": 0.5 }` changes one field,
//! `{ "grade": { "white_balance_kelvin": 6900 } }` one nested field (a `null` grade first takes
//! the neutral grade). The renderer applies the new look on the next extracted frame.
//!
//! `render.problems`: what broke the last frame the runtime renderer drew
//! (`FrameRenderer::frame_problems`), e.g. an MSAA quality with terrain, which skips the
//! terrain and the planet sky (`book/user-guide/host-look-checklist.md`).
//!
//! `render.mesh_occluders`: the large-mesh occlusion proxies (`OcclusionProxy`) the renderer
//! shades the ground and nearby meshes with.

use regia_ecs::prelude::*;
use regia_scene::level::{LevelLook, LevelSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `level.get_look`: the active level's look.
pub(crate) fn get_look(world: &mut World) -> Result<Value, RpcError> {
    let settings = level(world)?;
    let look = serde_json::to_value(&settings.look)
        .map_err(|e| RpcError::internal(format!("look serialization failed: {e}")))?;
    Ok(json!({ "look": look }))
}

/// `level.set_look`: deep-merges `params.look` into the level's look and returns before/after.
pub(crate) fn set_look(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let patch = params.get("look").filter(|v| v.is_object()).ok_or_else(|| {
        RpcError::invalid_params(
            "`look` (an object of LevelLook fields, e.g. {\"exposure_bias_ev\": 0.5}) is required",
        )
    })?;
    let before_look = level(world)?.look.clone();
    let before = serde_json::to_value(&before_look)
        .map_err(|e| RpcError::internal(format!("look serialization failed: {e}")))?;
    let mut merged = before.clone();
    merge(&mut merged, patch, &neutral_children()?);
    let after_look: LevelLook = serde_json::from_value(merged)
        .map_err(|e| RpcError::invalid_params(format!("the look does not fit LevelLook: {e}")))?;
    let after = serde_json::to_value(&after_look)
        .map_err(|e| RpcError::internal(format!("look serialization failed: {e}")))?;
    let mut settings = world.get_resource_mut::<LevelSettings>().ok_or_else(no_level)?;
    settings.look = after_look;
    Ok(json!({ "before": before, "after": after }))
}

fn level(world: &World) -> Result<&LevelSettings, RpcError> {
    world.get_resource::<LevelSettings>().ok_or_else(no_level)
}

fn no_level() -> RpcError {
    RpcError::not_found(
        "no LevelSettings resource: no level is open",
        "open a level (its settings carry the look) or insert LevelSettings",
    )
}

/// The neutral value of each optional nested block, used when a patch fills a `null` one.
fn neutral_children() -> Result<serde_json::Map<String, Value>, RpcError> {
    let neutral = json!({
        "meter": serde_json::to_value(regia_scene::level::LevelMeter::default())
            .map_err(|e| RpcError::internal(e.to_string()))?,
        "grade": serde_json::to_value(regia_scene::level::LevelGrade::default())
            .map_err(|e| RpcError::internal(e.to_string()))?,
        "local_exposure": serde_json::to_value(regia_scene::level::LevelLocalExposure::default())
            .map_err(|e| RpcError::internal(e.to_string()))?,
    });
    Ok(neutral.as_object().cloned().unwrap_or_default())
}

/// Recursive object merge; a `null` target object takes its neutral block before merging.
fn merge(target: &mut Value, patch: &Value, neutral: &serde_json::Map<String, Value>) {
    let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) else {
        return;
    };
    for (key, value) in patch {
        match (target.get_mut(key), value) {
            (Some(slot), Value::Object(_)) if slot.is_object() => merge(slot, value, neutral),
            (Some(slot), Value::Object(_)) if slot.is_null() => {
                let mut base = neutral.get(key).cloned().unwrap_or(json!({}));
                merge(&mut base, value, neutral);
                *slot = base;
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_set_look_merge_partial_objects() {
        let mut world = World::new();
        assert!(get_look(&mut world).is_err(), "no level, a clear error");
        world.insert_resource(LevelSettings::default());
        let got = get_look(&mut world).expect("get");
        assert_eq!(got["look"]["exposure_bias_ev"], json!(0.0));
        assert_eq!(got["look"]["grade"], Value::Null);

        let set = set_look(
            &mut world,
            &json!({ "look": { "exposure_bias_ev": 0.5, "grade": { "white_balance_kelvin": 6900.0 } } }),
        )
        .expect("set");
        assert_eq!(set["before"]["exposure_bias_ev"], json!(0.0));
        assert_eq!(set["after"]["exposure_bias_ev"], json!(0.5));
        let look = world.resource::<LevelSettings>().look.clone();
        assert_eq!(look.exposure_bias_ev, 0.5);
        let grade = look.grade.expect("grade filled from neutral");
        assert_eq!(grade.white_balance_kelvin, 6900.0);
        assert_eq!(grade.gamma, [1.0; 3], "untouched fields stay neutral");

        // A second patch keeps the first.
        set_look(
            &mut world,
            &json!({ "look": { "meter": { "center_weight": 0.4 } } }),
        )
        .expect("set meter");
        let look = world.resource::<LevelSettings>().look.clone();
        assert_eq!(look.exposure_bias_ev, 0.5);
        assert_eq!(look.meter.expect("meter").center_weight, 0.4);

        // Local exposure (DO27's `local_exposure_shadows 0.75`): off until set, the rest neutral.
        set_look(
            &mut world,
            &json!({ "look": { "local_exposure": { "shadow_contrast": 0.75 } } }),
        )
        .expect("set local exposure");
        let local = world
            .resource::<LevelSettings>()
            .look
            .local_exposure
            .expect("local exposure filled from neutral");
        assert_eq!(local.shadow_contrast, 0.75);
        assert_eq!(local.highlight_contrast, 1.0);
        assert_eq!(local.detail_strength, 1.0);

        let bad = set_look(&mut world, &json!({ "look": { "contrast": "high" } }));
        assert!(bad.is_err(), "a wrong type is rejected");
        assert!(set_look(&mut world, &json!({})).is_err(), "`look` is required");
    }
}

/// `render.problems`: the runtime renderer's last-frame problems, `{ problems: [{ code,
/// message }], ok }` (`ok` is true when there are none).
pub(crate) fn render_problems(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .ok_or_else(|| {
            RpcError::not_found(
                "no RuntimeRenderer resource in this app",
                "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
                 FrameRenderer::frame_problems)",
            )
        })?;
    let problems: Vec<Value> = renderer
        .0
        .frame_problems()
        .iter()
        .map(|p| json!({ "code": p.code, "message": p.message }))
        .collect();
    Ok(json!({ "ok": problems.is_empty(), "problems": problems }))
}

/// `render.memory_pressure`: the out-of-memory handling state (ADR-223, v0.1.16): pressure
/// signals, texture budget steps, caught and uncaught out-of-memory errors, the mip hold, the
/// streaming back-off, the integrated-GPU defaults and the last GPU memory buckets. Works
/// without a renderer (the process-wide parts).
pub(crate) fn memory_pressure(world: &mut World) -> Result<Value, RpcError> {
    use regia_render::gpu_health as health;
    let renderer = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| &r.0);
    let integrated = renderer.map(regia_render::FrameRenderer::integrated_gpu_limits);
    let warm = world.get_resource::<regia_render::GameFrameWarm>();
    let categories = regia_render::memory::last_summary_buckets().map(|buckets| {
        buckets
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), json!(bytes)))
            .collect::<serde_json::Map<String, Value>>()
    });
    let stream = renderer.map(|r| {
        let report = r.stream_report();
        let budget = r.texture_budget_bytes();
        json!({
            "resident_bytes": report.resident_bytes,
            "budget_bytes": (budget != u64::MAX).then_some(budget),
            "held": r.texture_streaming_held(),
            "backing_off": r.texture_streaming_backing_off(),
        })
    });
    Ok(json!({
        "signals": health::memory_pressure_count(),
        "texture_budget_steps": health::texture_budget_steps(),
        "oom_uncaught": health::gpu_oom_count(),
        "oom_caught": health::captured_oom_count(),
        "mips_held": health::texture_mips_held(),
        "mip_drops": health::texture_mip_drops(),
        "device_lost": health::device_lost(),
        "integrated_limits": integrated,
        "new_scene_warm": warm.map(|w| w.new_scene_warm_on(integrated.unwrap_or(false))),
        "stream": stream,
        "categories": categories,
        "pipeline_cache_bytes": regia_render::memory::pipeline_cache_bytes(),
    }))
}

/// `assets.texture_memory`: [`regia_render::FrameRenderer::texture_memory`] as JSON, the
/// `top` largest textures first.
pub(crate) fn texture_memory(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let top = params.get("top").and_then(Value::as_u64).unwrap_or(50).min(1000) as usize;
    let renderer = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .ok_or_else(|| {
            RpcError::not_found(
                "no RuntimeRenderer resource in this app",
                "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
                 FrameRenderer::texture_memory)",
            )
        })?;
    Ok(texture_memory_json(&renderer.0.texture_memory(), top))
}

/// The JSON shape of a [`regia_render::TextureMemoryReport`].
fn texture_memory_json(report: &regia_render::TextureMemoryReport, top: usize) -> Value {
    let textures: Vec<Value> = report
        .textures
        .iter()
        .take(top)
        .map(|t| {
            json!({
                "material_key": t.material_key,
                "name": t.name,
                "slot": t.slot,
                "width": t.width,
                "height": t.height,
                "resident_width": t.resident_width,
                "resident_height": t.resident_height,
                "format": t.format,
                "compressed": t.compressed,
                "streamed": t.streamed,
                "gpu_bytes": t.gpu_bytes,
                "cpu_bytes": t.cpu_bytes,
                "last_seen_frame": t.last_seen_frame,
            })
        })
        .collect();
    let problems: Vec<Value> = report
        .problems()
        .iter()
        .map(|p| json!({ "code": p.code, "message": p.message }))
        .collect();
    json!({
        "frame": report.frame,
        "streaming": report.streaming,
        // `null` = no budget (streaming off; the diagnostics then check Desktop High's).
        "budget_bytes": (report.budget_bytes != u64::MAX).then_some(report.budget_bytes),
        "resident_bytes": report.resident_bytes,
        "streamed_bytes": report.streamed_bytes,
        "unstreamed_bytes": report.unstreamed_bytes,
        "cpu_bytes": report.cpu_bytes,
        "uncompressed": report.uncompressed,
        "uncompressed_bytes": report.uncompressed_bytes,
        "tiny_uncompressed": report.tiny_uncompressed,
        "uncompressed_limit": report.uncompressed_limit(),
        "over_budget": report.over_budget(),
        "problems": problems,
        "wrong_platform": {
            "decoded": report.wrong_platform.decoded,
            "decoded_bytes": report.wrong_platform.decoded_bytes,
            "unbound": report.wrong_platform.unbound,
            "formats": report.wrong_platform.formats,
            "labels": report.wrong_platform.labels,
        },
        "textures": textures,
    })
}

/// `render.mesh_occluders`: the large-mesh occlusion proxies the runtime renderer shades with
/// (`OcclusionProxy` entities, `FrameRenderer::set_mesh_occluders`): `{ in_use, bands: { near,
/// far, dropped, near_max, far_max }, occluders: [{ center, axes, half_extents, albedo,
/// ground_y, far_band }] }`. `in_use` is how many the last frame shaded at full quality (0 on
/// frames not lit by the planet atmosphere's IBL); `bands` splits the last frame's proxies into
/// full quality, far-band contact blobs and dropped (ADR-7970).
pub(crate) fn render_mesh_occluders(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .ok_or_else(|| {
            RpcError::not_found(
                "no RuntimeRenderer resource in this app",
                "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
                 FrameRenderer::mesh_occluders)",
            )
        })?;
    let occluders: Vec<Value> = renderer
        .0
        .mesh_occluders()
        .iter()
        .map(|o| {
            json!({
                "center": o.center,
                "axes": o.axes,
                "half_extents": o.half_extents,
                "albedo": o.albedo,
                "ground_y": o.ground_y,
                "far_band": o.far_band,
            })
        })
        .collect();
    let bands = renderer.0.mesh_occluder_bands();
    Ok(json!({
        "in_use": renderer.0.mesh_occluders_in_use(),
        "bands": {
            "near": bands.near,
            "far": bands.far,
            "dropped": bands.dropped,
            "near_max": regia_render::MAX_MESH_OCCLUDERS,
            "far_max": regia_render::MAX_FAR_MESH_OCCLUDERS,
        },
        "occluders": occluders,
    }))
}

/// `render.emissive_lights`: what the last frame did with the area lights fitted to emissive
/// meshes ([`regia_render::FrameRenderer::emissive_lights`]).
pub(crate) fn render_emissive_lights(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .ok_or_else(|| {
            RpcError::not_found(
                "no RuntimeRenderer resource in this app",
                "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
                 FrameRenderer::emissive_lights)",
            )
        })?;
    let r = renderer.0.emissive_lights();
    let f = renderer.0.emissive_fit_stats();
    Ok(json!({
        "enabled": r.cap > 0,
        "budget": r.cap,
        "meshes": r.meshes,
        "fitted": r.fitted,
        "in_view": r.in_view,
        "uploaded": r.uploaded,
        "dropped": r.dropped,
        "refits": r.refits,
        "cpu_ms": r.cpu_ms,
        "probes_traced": r.probes_traced,
        "probes_deferred": r.probes_deferred,
        "buried": r.buried,
        // ADR-1386: uploaded lights of emitters the baked volume holds (left to it where it covers).
        "baked": r.baked,
        // v0.1.18 (ADR-225): the content-keyed fit cache and its background thread, since start.
        "fit_cache": {
            "fits_done": f.fits_done,
            "fits_deferred": f.fits_deferred,
            "cache_hits": f.cache_hits,
            "probe_hits": f.probe_hits,
            "probes_traced": f.probes_traced,
            "budget_holds": f.budget_holds,
            "stand_ins": f.stand_ins,
            "hashes": f.hashes,
            "frame_ms": f.frame_ms,
            "worker_ms": f.worker_ms,
            "pending": f.pending,
            "background": f.background,
            "frame_budget_ms": regia_render::emissive_lights::EMISSIVE_FRAME_BUDGET_MS,
        },
    }))
}

#[cfg(test)]
mod problem_tests {
    use super::*;

    #[test]
    fn emissive_lights_reports_the_last_frame_and_needs_the_renderer() {
        let mut world = World::new();
        assert!(render_emissive_lights(&mut world).is_err());
        world.insert_resource(regia_render::RuntimeRenderer::default());
        let v = render_emissive_lights(&mut world).expect("report");
        assert_eq!(v["enabled"], json!(false));
        assert_eq!(v["uploaded"], json!(0));
        // v0.1.18: the fit cache's counters, all zero before a frame.
        assert_eq!(v["fit_cache"]["fits_done"], json!(0));
        assert_eq!(v["fit_cache"]["background"], json!(false));
        assert_eq!(v["fit_cache"]["frame_budget_ms"], json!(1.0));
    }

    #[test]
    fn render_problems_reports_an_empty_list_and_needs_the_renderer() {
        let mut world = World::new();
        assert!(render_problems(&mut world).is_err());
        world.insert_resource(regia_render::RuntimeRenderer::default());
        let v = render_problems(&mut world).expect("problems");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["problems"], json!([]));
    }

    #[test]
    fn texture_memory_reports_the_budget_and_needs_the_renderer() {
        let mut world = World::new();
        assert!(texture_memory(&mut world, &json!({})).is_err());
        world.insert_resource(regia_render::RuntimeRenderer::default());
        let v = texture_memory(&mut world, &json!({ "top": 3 })).expect("texture memory");
        assert_eq!(v["resident_bytes"], json!(0));
        assert_eq!(v["uncompressed"], json!(0));
        assert_eq!(v["over_budget"], json!(false));
        assert_eq!(v["textures"], json!([]));
        assert_eq!(v["problems"], json!([]));
        // A report with entries lists the top ones, largest first, and its problems.
        let entry = |bytes: u64| regia_render::TextureMemoryEntry {
            material_key: bytes,
            name: Some("M_Hull".into()),
            slot: "base_color",
            width: 4096,
            height: 4096,
            resident_width: 2048,
            resident_height: 2048,
            format: "Rgba8UnormSrgb".into(),
            compressed: false,
            streamed: true,
            gpu_bytes: bytes,
            cpu_bytes: 0,
            last_seen_frame: 7,
        };
        let report = regia_render::TextureMemoryReport {
            streaming: true,
            budget_bytes: 1000,
            resident_bytes: 3000,
            textures: vec![entry(2000), entry(1000)],
            ..Default::default()
        };
        let v = texture_memory_json(&report, 1);
        assert_eq!(v["budget_bytes"], json!(1000));
        assert_eq!(v["over_budget"], json!(true));
        assert_eq!(v["textures"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["textures"][0]["gpu_bytes"], json!(2000));
        assert_eq!(v["textures"][0]["name"], json!("M_Hull"));
        assert_eq!(
            v["problems"][0]["code"],
            json!(regia_render::PROBLEM_TEXTURE_BUDGET)
        );
    }

    #[test]
    fn render_mesh_occluders_lists_the_proxies() {
        let mut world = World::new();
        assert!(render_mesh_occluders(&mut world).is_err());
        let mut renderer = regia_render::RuntimeRenderer::default();
        renderer
            .0
            .set_mesh_occluders(vec![regia_render::MeshOccluder::from_local_bounds(
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                [-2.0, 0.4, -1.0],
                [2.0, 1.8, 1.0],
                [0.3; 3],
            )]);
        world.insert_resource(renderer);
        let v = render_mesh_occluders(&mut world).expect("occluders");
        assert_eq!(v["in_use"], json!(0));
        assert_eq!(v["occluders"].as_array().map(Vec::len), Some(1));
        let ground_y = v["occluders"][0]["ground_y"].as_f64().unwrap_or(f64::NAN);
        assert!((ground_y - 0.4).abs() < 1e-6, "ground_y {ground_y}");
    }

    /// ADR-7970: the far band through the protocol's dispatch path: each proxy's opt-in and the
    /// band split (all 0 until a frame lit by the planet IBL packs them) with the caps.
    #[test]
    fn render_mesh_occluders_reports_the_far_band_through_dispatch() {
        let mut world = World::new();
        world.insert_resource(regia_app::Time::default());
        let hull = regia_render::MeshOccluder::from_local_bounds(
            regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            [-2.0, 0.4, -1.0],
            [2.0, 1.8, 1.0],
            [0.3; 3],
        );
        let mut renderer = regia_render::RuntimeRenderer::default();
        renderer
            .0
            .set_mesh_occluders(vec![hull, hull.with_far_band(true)]);
        world.insert_resource(renderer);
        let v = crate::dispatch::call_in_process(&mut world, "render.mesh_occluders", json!({}))
            .expect("synchronous method")
            .expect("protocol call");
        assert_eq!(v["occluders"][0]["far_band"], json!(false));
        assert_eq!(v["occluders"][1]["far_band"], json!(true));
        assert_eq!(
            v["bands"],
            json!({
                "near": 0,
                "far": 0,
                "dropped": 0,
                "near_max": regia_render::MAX_MESH_OCCLUDERS,
                "far_max": regia_render::MAX_FAR_MESH_OCCLUDERS,
            })
        );
    }
}
