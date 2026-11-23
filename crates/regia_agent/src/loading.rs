// SPDX-License-Identifier: MPL-2.0
//! Instant-load surfaces (`docs/specs/12-assets.md` § 5b, `docs/specs/13-rendering.md` § 2b–2c):
//! `render.boot_timeline`, `render.pipeline_warm`, `render.memory_budget`,
//! `render.set_host_budget` and `assets.load_progress`.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `render.boot_timeline`: milliseconds from process start to each boot mark.
pub(crate) fn boot_timeline() -> Result<Value, RpcError> {
    let marks: Vec<Value> = regia_render::boot::timeline()
        .into_iter()
        .map(|(mark, ms)| json!({ "mark": mark, "ms": ms }))
        .collect();
    Ok(json!({
        "now_ms": regia_render::boot::since_process_start().as_secs_f64() * 1000.0,
        "marks": marks,
    }))
}

/// A headless host's offscreen warm (ADR-1965), asked for with `render.pipeline_warm { offscreen }`.
///
/// A headless host draws only through offscreen captures, so nothing warms the pipelines its first
/// render-out frame needs: that frame compiled them on the app thread, 30 s without an answer on the
/// showcase scene (Red-Rouge v0.2.8). The warm moves the runtime renderer to a thread of its own
/// ([`regia_render::OffscreenWarm::start_in_place`]) and renders the extracted frame there: its
/// pipelines, the scene's uploads and the size's targets. `render.out_start` waits for it (`F0018`).
#[derive(Resource, Default)]
pub(crate) struct OffscreenWarmState {
    /// The warm thread, when it started and the size it warms.
    running: Option<(regia_render::OffscreenWarm, std::time::Instant, (u32, u32))>,
    /// A finished warm not put back yet (taken the next app frame).
    ready: Option<(regia_render::WarmedRenderer, (u32, u32))>,
    /// The last finished warm (`ms`, `pipelines`, `renderer_back`, `error`, `size`).
    report: Option<Value>,
}

impl OffscreenWarmState {
    /// `render.pipeline_warm`'s `offscreen_warm` block.
    fn to_json(&self) -> Value {
        let (state, size) = match (&self.running, &self.ready, &self.report) {
            (Some((_, _, size)), _, _) => ("running", Some(*size)),
            (None, Some((_, size)), _) => ("returning", Some(*size)),
            (None, None, Some(_)) => ("done", None),
            (None, None, None) => ("idle", None),
        };
        json!({
            "state": state,
            "size": size,
            "last": self.report,
        })
    }
}

/// The `offscreen` param: `{ width, height }`, each 1..=16384, or absent.
fn offscreen_size(params: &Value) -> Result<Option<(u32, u32)>, RpcError> {
    let Some(offscreen) = params.get("offscreen").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let side = |key: &str| {
        offscreen
            .get(key)
            .and_then(Value::as_u64)
            .filter(|v| (1..=16_384).contains(v))
            .map(|v| v as u32)
    };
    match (side("width"), side("height")) {
        (Some(width), Some(height)) => Ok(Some((width, height))),
        _ => Err(RpcError::invalid_params(
            "`offscreen` must be { width, height } with whole numbers from 1 to 16384 (the render-out's render size)",
        )),
    }
}

/// Starts the offscreen warm at `size` unless one is running. A host with a window answers
/// without one (its window warms its frames).
fn start_offscreen_warm(world: &mut World, size: (u32, u32)) -> Result<(), RpcError> {
    if world
        .get_resource::<regia_render::WindowSurfaces>()
        .is_some_and(|surfaces| surfaces.primary_size().is_some())
        || world
            .get_resource::<OffscreenWarmState>()
            .is_some_and(|state| state.running.is_some() || state.ready.is_some())
    {
        return Ok(());
    }
    let nothing = || {
        RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "nothing to warm offscreen",
            "spawn a Camera and a MeshRenderer (the warm renders the extracted frame), with RenderPlugin",
        )
    };
    let device = world
        .get_resource::<regia_render::RenderDevice>()
        .ok_or_else(nothing)?;
    let frame = world
        .get_resource::<regia_render::runtime::ExtractedFrame>()
        .filter(|frame| frame.is_renderable())
        .ok_or_else(nothing)?;
    if !world.contains_resource::<regia_render::runtime::RuntimeRenderer>() {
        return Err(nothing());
    }
    let (device, frame) = (device.clone(), frame.clone());
    let warm = world.resource_scope(|_, mut runtime: Mut<regia_render::runtime::RuntimeRenderer>| {
        regia_render::OffscreenWarm::start_in_place(&device, &mut runtime.0, &frame, size)
    });
    let warm =
        warm.map_err(|e| RpcError::internal(format!("cannot start the offscreen warm thread: {e}")))?;
    world
        .get_resource_or_insert_with(OffscreenWarmState::default)
        .running = Some((warm, std::time::Instant::now(), size));
    Ok(())
}

/// `Last`: puts a finished offscreen warm's renderer back in place.
pub(crate) fn advance_offscreen_warm(world: &mut World) {
    let ready = {
        let Some(mut state) = world.get_resource_mut::<OffscreenWarmState>() else {
            return;
        };
        if let Some((warm, _, size)) = state.running.as_ref()
            && let Some(warmed) = warm.take()
        {
            let size = *size;
            state.running = None;
            state.ready = Some((warmed, size));
        }
        state.ready.is_some()
    };
    if !ready {
        return;
    }
    let Some((warmed, size)) = world
        .get_resource_mut::<OffscreenWarmState>()
        .and_then(|mut state| state.ready.take())
    else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_agent", "offscreen_warm_adopt").entered();
    let adopted = warmed.renderer.map(|renderer| {
        world
            .get_resource_mut::<regia_render::runtime::RuntimeRenderer>()
            .map(|mut runtime| regia_render::OffscreenWarm::put_back(&mut runtime.0, renderer))
            .is_some()
    });
    let report = json!({
        "size": [size.0, size.1],
        "ms": warmed.ms,
        "pipelines": warmed.pipelines,
        "renderer_back": adopted.unwrap_or(false),
        "error": warmed.error,
    });
    tracing::info!(target: "regia_agent", "offscreen warm: {report}");
    if let Some(mut state) = world.get_resource_mut::<OffscreenWarmState>() {
        state.report = Some(report);
    }
}

/// Whether an offscreen warm has the runtime renderer (`render.out_start` waits, `F0018`).
pub(crate) fn offscreen_warm_busy(world: &World) -> bool {
    world
        .get_resource::<OffscreenWarmState>()
        .is_some_and(|state| state.running.is_some() || state.ready.is_some())
}

/// `render.pipeline_warm { offscreen? }`: the pipeline warm queue's progress per tier and the
/// persistent cache; with `offscreen: { width, height }` it also starts the offscreen warm of a
/// headless host (ADR-1965), reported in `offscreen_warm`.
pub(crate) fn pipeline_warm(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(size) = offscreen_size(params)? {
        start_offscreen_warm(world, size)?;
    }
    advance_offscreen_warm(world);
    let offscreen = world.get_resource::<OffscreenWarmState>().map_or_else(
        || OffscreenWarmState::default().to_json(),
        OffscreenWarmState::to_json,
    );
    let Some(queue) = world.get_resource::<regia_render::PipelineWarmQueue>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no pipeline warm queue (render plugin not added)",
            "add RenderPlugin",
        ));
    };
    let mut value = warm_json(&queue.progress());
    value["offscreen_warm"] = offscreen;
    // ADR-7885: the targets the host registered for the warm, and the newest pipeline creations
    // with their target, thread, site and new forward-set key axes.
    value["targets"] = Value::Array(
        regia_render::pipeline_warm::registered_warm_targets()
            .iter()
            .map(|t| json!({ "format": format!("{:?}", t.format), "samples": t.samples }))
            .collect(),
    );
    value["recent_pipelines"] = recent_pipelines_json();
    // ADR-8453: an install prepare this process ran (the host's "Preparing shaders" page).
    value["prepare"] = prepare_json();
    if let Some(game) = world.get_resource::<regia_render::GameFrameWarm>() {
        value["game_frame"] = json!({
            "pending": game.is_pending(),
            "warming": game.is_warming(),
            "on_splash_finish": game.on_splash_finish,
            "on_new_scene": game.on_new_scene,
            "scene_warms": game.scene_warms(),
            "last": game.last_report().map(|r| json!({ "ms": r.ms, "pipelines": r.pipelines, "error": r.error })),
            "first_frame_ms": game.first_frame_ms(),
        });
    }
    // Background boot work after the splash (ADR-173).
    if let Some(bg) = world.get_resource::<regia_render::BootBackground>() {
        value["boot_background"] = json!({
            "splash_done": bg.splash_done,
            "splash_capped": bg.splash_capped,
            "game_warm_pending": bg.game_warm_pending,
            "game_warm_running": bg.game_warm_running,
            "deferred_pending": bg.deferred_pending,
            "probe_active": bg.probe_active,
            "idle": bg.is_idle(),
        });
    }
    Ok(value)
}

/// `render.scene_ready`: the host's scene prepare (ADR-7883), as published into
/// `regia_render::SceneReadiness` by the render path.
pub(crate) fn scene_ready(world: &mut World) -> Result<Value, RpcError> {
    let Some(readiness) = world.get_resource::<regia_render::SceneReadiness>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no scene readiness (render plugin not added)",
            "add RenderPlugin",
        ));
    };
    let s = readiness.status();
    Ok(json!({
        "state": s.state.as_str(),
        "map_id": s.map_id,
        "prepare": s.prepare,
        "uploaded_mb": s.uploaded_mb,
        "meshes": s.meshes,
        "pipelines": s.pipelines,
        "warms": s.warms,
        "ms": s.ms,
        "timed_out": s.timed_out,
        "error": s.error,
        "complete": s.complete,
        "first_present_frame": s.first_present_frame,
        "first_present_ms": s.first_present_ms,
        "presented_to_window": s.presented_to_window,
    }))
}

/// The newest progress of `regia_render::prepare::run` / `run_on` in this process, or null.
fn prepare_json() -> Value {
    let Some((progress, elapsed)) = regia_render::prepare::last_progress() else {
        return Value::Null;
    };
    let eta = regia_render::prepare::eta(&progress, elapsed);
    json!({
        "state": progress.state.name(),
        "phase": progress.phase,
        "done": progress.done,
        "total": progress.total,
        "fraction": progress.fraction,
        "pipelines": progress.pipelines,
        "skipped": progress.skipped,
        "error": progress.error,
        "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "eta_ms": eta.map(|eta| u64::try_from(eta.as_millis()).unwrap_or(u64::MAX)),
    })
}

/// The newest pipeline creations of the process (at most `RECENT_CREATES`), oldest first, with
/// what each drew into (ADR-7885).
fn recent_pipelines_json() -> Value {
    let mut buffer = regia_render::create_buffer();
    let n = regia_render::recent_creates(0, &mut buffer);
    Value::Array(
        buffer[..n]
            .iter()
            .filter(|r| r.kind == regia_render::FramePathKind::Pipeline)
            .map(|r| {
                let d = &r.detail;
                json!({
                    "label": r.label(),
                    "format": d.format.map(|f| format!("{f:?}")),
                    "samples": d.samples,
                    "depth_format": d.depth_format.map(|f| format!("{f:?}")),
                    "thread": if d.warm_worker { "warm_worker" } else { "frame" },
                    "site": d.site.map(|site| format!("{}:{}", site.file(), site.line())),
                    "new_axes": d.new_axes.names().collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn warm_json(p: &regia_render::PipelineWarmProgress) -> Value {
    let tiers: Vec<Value> = p
        .tiers
        .iter()
        .map(|t| {
            json!({
                "tier": t.tier,
                "jobs_done": t.jobs_done,
                "jobs_total": t.jobs_total,
                "jobs_failed": t.jobs_failed,
                "pipelines": t.pipelines,
                "busy_ms": t.busy_ms,
                "ready_after_ms": t.ready_after_ms,
            })
        })
        .collect();
    let load = match &p.cache.load {
        None => json!(null),
        Some(regia_render::pipeline_cache::CacheLoad::Missing) => json!({ "state": "missing" }),
        Some(regia_render::pipeline_cache::CacheLoad::Loaded(bytes)) => {
            json!({ "state": "loaded", "bytes": bytes })
        }
        Some(regia_render::pipeline_cache::CacheLoad::Discarded(reason)) => {
            json!({ "state": "discarded", "reason": reason.to_string() })
        }
        Some(regia_render::pipeline_cache::CacheLoad::Seeded(bytes)) => {
            json!({ "state": "seeded", "bytes": bytes })
        }
    };
    let shipped = match &p.cache.shipped {
        None => json!(null),
        Some(regia_render::pipeline_cache::ShippedLoad::NoMatch) => json!({ "state": "no_match" }),
        Some(regia_render::pipeline_cache::ShippedLoad::Seeded(from)) => {
            json!({ "state": "seeded", "from": from })
        }
        Some(regia_render::pipeline_cache::ShippedLoad::Discarded(from, reason)) => {
            json!({ "state": "discarded", "from": from, "reason": reason.to_string() })
        }
    };
    json!({
        "done": p.done,
        "total": p.total,
        "fraction": p.fraction(),
        "started": p.started,
        "tiers": tiers,
        "pipelines_outside": p.pipelines_outside,
        "outside_sites": p.outside_sites.iter().map(|(site, n)| json!({ "site": site, "count": n })).collect::<Vec<_>>(),
        "pipelines_created": regia_render::pipeline_cache::pipelines_created(),
        "deferred": { "pending": p.deferred_pending, "built": p.deferred_built, "released": p.deferred_released },
        "last_error": p.last_error,
        "cache": {
            "enabled": p.cache.enabled,
            "path": p.cache.path,
            "load": load,
            "shipped": shipped,
            "load_ms": p.cache.load_ms,
            "saved_bytes": p.cache.saved_bytes,
            "saves": p.cache.saves,
            // A driver rule left the device without a cache (Intel's Windows Vulkan driver, ADR-3288).
            "skipped": (!p.cache.enabled && regia_render::pipeline_cache::driver_skipped())
                .then_some(regia_render::pipeline_cache::DRIVER_SKIP_REASON),
        },
    })
}

/// `assets.load_progress`: residency and progress of one load set (`set`) or all of them.
pub(crate) fn load_progress(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(sets) = world.get_resource::<regia_asset::LoadSets>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no load-set registry (asset plugin not added)",
            "add AssetPlugin",
        ));
    };
    let names: Vec<String> = match crate::params::opt_str(params, "set")? {
        Some(name) => {
            if sets.get(name).is_none() {
                return Err(RpcError::invalid_params(format!(
                    "no load set named {name:?}; defined: {:?}",
                    sets.names()
                )));
            }
            vec![name.to_string()]
        }
        None => sets.names(),
    };
    let out: Vec<Value> = names
        .iter()
        .filter_map(|n| sets.load_progress(n))
        .map(|p| progress_json(&p))
        .collect();
    Ok(json!({ "sets": out }))
}

fn progress_json(p: &regia_asset::LoadProgress) -> Value {
    let kinds: Vec<Value> = p
        .kinds
        .iter()
        .map(|k| {
            json!({
                "kind": k.kind,
                "resident": k.resident,
                "total": k.total,
                "resident_bytes": k.resident_bytes,
                "total_bytes": k.total_bytes,
            })
        })
        .collect();
    json!({
        "name": p.name,
        "resident": p.is_resident(),
        "items_resident": p.resident,
        "items_total": p.total,
        "fraction": p.fraction,
        "kinds": kinds,
        "failed": p.failed,
    })
}

/// `render.memory_budget`: current use by category and per load set against a tier's budget, and
/// the admission check for `set` when given.
pub(crate) fn memory_budget(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tier_name = crate::params::opt_str(params, "tier")?.unwrap_or("minimum");
    let Some(tier) = regia_render::BudgetTier::from_name(tier_name) else {
        return Err(RpcError::invalid_params(format!(
            "unknown tier {tier_name:?} ({})",
            regia_render::BudgetTier::names()
        )));
    };
    let Some(device) = world.get_resource::<regia_render::RenderDevice>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no render device yet",
            "open a window or take an offscreen screenshot first (seeing.screenshot target=offscreen)",
        ));
    };
    let report = regia_render::memory_report(device);
    let sets = world
        .get_resource::<regia_asset::LoadSets>()
        .cloned()
        .unwrap_or_default();
    let budget = regia_render::MemoryBudget::for_tier(tier);
    let current = regia_render::memory_use(&report, &sets);
    let admission = crate::params::opt_str(params, "set")?
        .map(|name| regia_render::admit(&budget, &current, &sets, name));
    let mut out = budget_json(tier, &budget, &current, admission.as_ref());
    out["host"] = host_memory_json();
    Ok(out)
}

/// The host memory budget block (ADR-7891): the enforced budget, the override, and the counters
/// of every `HostMeshStore` in the process. `null` budget sides are unlimited.
fn host_memory_json() -> Value {
    use regia_render::host_memory as hm;
    let side = |bytes: u64| {
        if bytes == u64::MAX {
            Value::Null
        } else {
            json!(bytes)
        }
    };
    let budget = |b: hm::HostMemoryBudget| json!({ "cpu_mesh_bytes": side(b.cpu_mesh_bytes), "cpu_texture_bytes": side(b.cpu_texture_bytes) });
    let c = hm::host_memory_counters();
    json!({
        "budget": budget(hm::last_enforced_host_budget()),
        "override": hm::host_budget_override().map_or(Value::Null, budget),
        "cpu_mesh_bytes": c.cpu_mesh_bytes,
        "cpu_texture_bytes": c.cpu_texture_bytes,
        "dropped_bytes": c.dropped_bytes,
        "drops": c.drops,
        "reread_bytes": c.reread_bytes,
        "rereads": c.rereads,
        "over_budget_ticks": c.over_budget_ticks,
        "pinned_bytes": c.pinned_bytes,
        "unsourced_bytes": c.unsourced_bytes,
        "mapped_touched_bytes": c.mapped_touched_bytes,
    })
}

fn host_budget_bad_params(detail: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "host_budget_bad_params",
        detail,
        "pass { \"cpu_mesh_bytes\": 67108864 } and/or { \"cpu_texture_bytes\": n } (whole bytes), or { \"clear\": true } \
         to go back to the profile's budget (book/agents/load-memory.md)",
    )
}

/// `render.set_host_budget { cpu_mesh_bytes?, cpu_texture_bytes?, clear? }`: sets (or clears) the
/// process-wide host memory budget override every `HostMeshStore` enforces from its next frame
/// (ADR-7891) and answers the `host` block of `render.memory_budget`. A side left out keeps the
/// override's value, else the budget last enforced.
pub(crate) fn set_host_budget(params: &Value) -> Result<Value, RpcError> {
    use regia_render::host_memory as hm;
    let bytes = |key: &str| {
        crate::params::opt_u64(params, key).map_err(|_| {
            host_budget_bad_params(&format!("`{key}` must be a whole number of bytes (0 or more)"))
        })
    };
    let mesh = bytes("cpu_mesh_bytes")?;
    let texture = bytes("cpu_texture_bytes")?;
    let clear = crate::params::opt_bool(params, "clear")
        .map_err(|_| host_budget_bad_params("`clear` must be true or false"))?
        .unwrap_or(false);
    if clear {
        if mesh.is_some() || texture.is_some() {
            return Err(host_budget_bad_params("`clear` takes no byte values"));
        }
        hm::set_host_budget_override(None);
        return Ok(host_memory_json());
    }
    if mesh.is_none() && texture.is_none() {
        return Err(host_budget_bad_params(
            "give cpu_mesh_bytes, cpu_texture_bytes or clear",
        ));
    }
    let base = hm::host_budget_override().unwrap_or_else(hm::last_enforced_host_budget);
    hm::set_host_budget_override(Some(hm::HostMemoryBudget {
        cpu_mesh_bytes: mesh.unwrap_or(base.cpu_mesh_bytes),
        cpu_texture_bytes: texture.unwrap_or(base.cpu_texture_bytes),
    }));
    Ok(host_memory_json())
}

fn budget_json(
    tier: regia_render::BudgetTier,
    budget: &regia_render::MemoryBudget,
    current: &regia_render::MemoryUse,
    admission: Option<&regia_render::Admission>,
) -> Value {
    let categories: serde_json::Map<String, Value> = current
        .vram_by_category
        .iter()
        .map(|(c, b)| (c.name().to_owned(), json!(b)))
        .collect();
    let sets: Vec<Value> = current
        .sets
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "ram_bytes": s.ram_bytes,
                "vram_bytes": s.vram_bytes,
                "ram_pending": s.ram_pending,
                "vram_pending": s.vram_pending,
                "fraction": s.fraction,
            })
        })
        .collect();
    json!({
        "tier": tier.name(),
        "budget": { "ram": budget.ram, "vram": budget.vram },
        "use": {
            "vram": current.vram,
            "vram_measured": current.vram_measured,
            "ram": current.ram,
            "vram_by_category": categories,
        },
        "sets": sets,
        "admission": admission.map(|a| json!({
            "set": a.name,
            "fits": a.fits,
            "vram_after": a.vram_after,
            "ram_after": a.ram_after,
            "reason": a.reason,
        })),
    })
}

/// `assets.handles { kind?, limit? }`: generic-asset reference counts, pending unloads, budgets and
/// load-group progress (`docs/specs/12-assets.md` §§ 2-3, ADR-7202), with the built-in kinds
/// (`mesh`, `texture`, `image`, `material`: counted and pinned) and reference cycles as warnings
/// (ADR-7256).
pub(crate) fn asset_handles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(server) = world.get_resource::<regia_asset::AssetServer>() else {
        return Err(crate::assets::no_asset_server());
    };
    let kind = crate::params::opt_str(params, "kind")?;
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(100);
    let report = server.handle_report(kind);
    if let Some(kind) = kind
        && report.kinds.is_empty()
    {
        let mut known: Vec<String> = server
            .handle_report(None)
            .kinds
            .into_iter()
            .map(|k| k.kind)
            .collect();
        for builtin in ["image", "material", "mesh", "texture"] {
            if !known.iter().any(|k| k == builtin) {
                known.push(builtin.to_owned());
            }
        }
        known.sort();
        return Err(RpcError::invalid_params(format!(
            "no asset loader of kind {kind:?}, or a built-in kind that holds nothing; kinds: {known:?}"
        )));
    }
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let kinds: Vec<Value> = report
        .kinds
        .iter()
        .map(|k| {
            json!({
                "kind": k.kind,
                "loaded": k.loaded,
                "loading": k.loading,
                "strong_handles": k.strong_handles,
                "pending_unloads": k.pending_unloads,
                "pinned": k.pinned,
                "cpu_bytes": k.cpu_bytes,
                "gpu_bytes": k.gpu_bytes,
                "budget": k.budget,
            })
        })
        .collect();
    let assets: Vec<Value> = report
        .assets
        .iter()
        .take(limit)
        .map(|a| {
            json!({
                "id": a.id.to_string(),
                "kind": a.kind,
                "strong": a.strong,
                "unload_in_frames": a.unload_in_frames,
                "cpu_bytes": a.cpu_bytes,
                "gpu_bytes": a.gpu_bytes,
                "cpu_data_kept": a.cpu_data_kept,
                "dependencies": a.dependencies,
                "pinned": a.pinned,
            })
        })
        .collect();
    let groups: Vec<Value> = report
        .groups
        .iter()
        .map(|g| {
            json!({
                "name": g.name,
                "loaded": g.loaded,
                "failed": g.failed,
                "total": g.total,
                "fraction": g.fraction,
                "complete": g.complete,
                "failures": g.failures,
            })
        })
        .collect();
    let cycles: Vec<Vec<String>> = report
        .cycles
        .iter()
        .map(|c| c.iter().map(ToString::to_string).collect())
        .collect();
    let warnings: Vec<String> = cycles
        .iter()
        .map(|c| {
            format!(
                "reference cycle: {} hold each other and never unload by count",
                c.join(", ")
            )
        })
        .collect();
    Ok(json!({
        "grace_frames": report.grace_frames,
        "kinds": kinds,
        "assets": assets,
        "truncated": report.assets.len() > limit,
        "groups": groups,
        "cycles": cycles,
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-byte-per-char text asset for the `assets.handles` round trip.
    struct Blob(String);

    struct BlobLoader;

    impl regia_asset::AssetLoader for BlobLoader {
        type Asset = Blob;

        fn type_tag(&self) -> regia_asset::AssetTypeTag {
            regia_asset::AssetTypeTag(regia_core::id::TypePathHash::of("agent_test::Blob"))
        }

        fn load(
            &self,
            payload: &[u8],
            _ctx: &mut regia_asset::LoadContext<'_>,
        ) -> Result<Blob, regia_asset::AssetLoadError> {
            Ok(Blob(String::from_utf8_lossy(payload).into_owned()))
        }

        fn cpu_bytes(&self, asset: &Blob) -> u64 {
            asset.0.len() as u64
        }
    }

    #[test]
    fn asset_handles_reports_counts_and_groups_through_the_protocol() {
        let root = regia_test_utils::unique_temp_dir("agent-asset-handles");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let ids = [
            regia_asset::AssetId::new_random(),
            regia_asset::AssetId::new_random(),
        ];
        for (id, text) in ids.iter().zip(["abc", "defgh"]) {
            let header = regia_asset::CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: regia_core::id::TypePathHash::of("agent_test::Blob"),
                asset_id: *id,
                importer_version: regia_asset::pack_importer_version(1, 0, 0),
                dependencies: Vec::new(),
                payload_len: text.len() as u64,
            };
            let bytes = regia_asset::encode_cooked_file(&header, text.as_bytes()).expect("encode");
            std::fs::write(cache.join(regia_asset::cooked_file_name(*id)), bytes).expect("write");
        }
        let mut world = World::new();
        let call = |world: &mut World, params: Value| {
            crate::dispatch::call_in_process(world, "assets.handles", params).expect("synchronous method")
        };
        assert!(call(&mut world, json!({})).is_err(), "no asset server");

        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("open");
        server.register_loader(BlobLoader);
        server.set_unload_grace_frames(7);
        let held = server.load::<Blob>(ids[0]).expect("load");
        let released = server.load::<Blob>(ids[1]).expect("load");
        let group = server.load_group::<Blob>(&ids[..1]).expect("group");
        server.wait_for_loads();
        drop(released);
        server.update();
        world.insert_resource(server);

        let v = call(&mut world, json!({ "kind": "Blob" })).expect("report");
        assert_eq!(v["grace_frames"], 7);
        assert_eq!(v["kinds"][0]["kind"], "Blob");
        assert_eq!(v["kinds"][0]["loaded"], 2);
        assert_eq!(v["kinds"][0]["pending_unloads"], 1);
        assert_eq!(v["kinds"][0]["cpu_bytes"], 8);
        let row = |id: regia_asset::AssetId| {
            v["assets"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["id"] == id.to_string()).cloned())
                .expect("row")
        };
        assert_eq!(row(ids[0])["strong"], 2, "the handle and the group");
        assert_eq!(row(ids[1])["unload_in_frames"], 6);
        assert_eq!(v["groups"][0]["name"], group.name());
        assert_eq!(v["groups"][0]["complete"], true);
        let limited = call(&mut world, json!({ "limit": 1 })).expect("limited");
        assert_eq!(limited["assets"].as_array().map(Vec::len), Some(1));
        assert_eq!(limited["truncated"], true);
        assert!(
            call(&mut world, json!({ "kind": "Mesh" })).is_err(),
            "unknown kind"
        );
        drop((held, group));
    }

    /// A blob that names its dependencies (one id per line), for the cycle report.
    struct Link {
        _deps: Vec<regia_asset::StrongHandle<Link>>,
    }

    struct LinkLoader;

    impl regia_asset::AssetLoader for LinkLoader {
        type Asset = Link;

        fn type_tag(&self) -> regia_asset::AssetTypeTag {
            regia_asset::AssetTypeTag(regia_core::id::TypePathHash::of("agent_test::Link"))
        }

        fn load(
            &self,
            payload: &[u8],
            ctx: &mut regia_asset::LoadContext<'_>,
        ) -> Result<Self::Asset, regia_asset::AssetLoadError> {
            let text = String::from_utf8_lossy(payload);
            let mut deps = Vec::new();
            for line in text.lines().filter(|l| !l.is_empty()) {
                let id: regia_asset::AssetId = line
                    .parse()
                    .map_err(|_| regia_asset::AssetLoadError::Invalid(line.to_owned()))?;
                deps.push(ctx.load::<Link>(id)?);
            }
            Ok(Link { _deps: deps })
        }
    }

    fn write_cooked(
        cache: &std::path::Path,
        id: regia_asset::AssetId,
        tag: regia_core::id::TypePathHash,
        payload: &[u8],
    ) {
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: tag,
            asset_id: id,
            importer_version: regia_asset::pack_importer_version(1, 0, 0),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let bytes = regia_asset::encode_cooked_file(&header, payload).expect("encode");
        std::fs::write(cache.join(regia_asset::cooked_file_name(id)), bytes).expect("write");
    }

    /// ADR-7256 through the protocol: a counted mesh and a pinned (typed-load) mesh are listed
    /// under the built-in kind `mesh`, and a two-asset reference cycle comes back as a warning.
    #[test]
    fn asset_handles_lists_built_in_meshes_and_warns_about_a_reference_cycle() {
        let root = regia_test_utils::unique_temp_dir("agent-asset-handles-builtin");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let new = regia_asset::AssetId::new_random;
        let (counted, pinned, a, b) = (new(), new(), new(), new());
        let mesh = regia_asset::CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0; 2]; 3],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let mesh_bytes = mesh.to_postcard().expect("postcard");
        for id in [counted, pinned] {
            write_cooked(
                &cache,
                id,
                regia_asset::AssetTypeTag::mesh_legacy_cpu().0,
                &mesh_bytes,
            );
        }
        let link = regia_core::id::TypePathHash::of("agent_test::Link");
        write_cooked(&cache, a, link, b.to_string().as_bytes());
        write_cooked(&cache, b, link, a.to_string().as_bytes());

        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("open");
        server.register_loader(LinkLoader);
        let held = server.load::<regia_asset::CpuMesh>(counted).expect("counted");
        let _legacy = server.load_mesh(pinned).expect("typed load");
        let link_handle = server.load::<Link>(a).expect("link");
        server.wait_for_loads();
        let mut world = World::new();
        world.insert_resource(server);

        let v = crate::dispatch::call_in_process(&mut world, "assets.handles", json!({ "kind": "mesh" }))
            .expect("synchronous method")
            .expect("report");
        assert_eq!(v["kinds"][0]["kind"], "mesh");
        assert_eq!(v["kinds"][0]["loaded"], 2);
        assert_eq!(v["kinds"][0]["pinned"], 1);
        assert_eq!(v["kinds"][0]["strong_handles"], 1);
        let rows = v["assets"].as_array().cloned().unwrap_or_default();
        let row = |id: regia_asset::AssetId| rows.iter().find(|r| r["id"] == id.to_string()).cloned();
        assert_eq!(row(pinned).map(|r| r["pinned"].clone()), Some(json!(true)));
        assert_eq!(row(counted).map(|r| r["strong"].clone()), Some(json!(1)));
        let mut cycle = vec![a.to_string(), b.to_string()];
        cycle.sort();
        assert_eq!(v["cycles"], json!([cycle]));
        assert_eq!(v["warnings"].as_array().map(Vec::len), Some(1));
        drop((held, link_handle));
    }

    /// ADR-7284 through the protocol: counted loads (no typed load anywhere) are counted in the
    /// load-forms block `assets.cooked_pack` reports, as typed loads are, and `assets.handles`
    /// lists them under their built-in kinds.
    #[test]
    fn counted_loads_show_in_the_cooked_pack_load_forms_and_the_handle_report() {
        let root = regia_test_utils::unique_temp_dir("agent-counted-load-forms");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let (mesh_id, image_id) = (
            regia_asset::AssetId::new_random(),
            regia_asset::AssetId::new_random(),
        );
        let mesh = regia_asset::CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0; 2]; 3],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        write_cooked(
            &cache,
            mesh_id,
            regia_asset::AssetTypeTag::mesh_legacy_cpu().0,
            &mesh.to_postcard().expect("postcard"),
        );
        let image = regia_asset::CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![1, 2, 3, 4],
        };
        write_cooked(
            &cache,
            image_id,
            regia_asset::AssetTypeTag::image().0,
            &image.to_postcard().expect("postcard"),
        );
        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("open");
        let held = (
            server
                .load::<regia_asset::CpuMesh>(mesh_id)
                .expect("counted mesh"),
            server
                .load::<regia_asset::CookedImage>(image_id)
                .expect("counted image"),
        );
        server.wait_for_loads();
        let mut world = World::new();
        world.insert_resource(server);

        let v = crate::dispatch::call_in_process(&mut world, "assets.cooked_pack", json!({}))
            .expect("synchronous method")
            .expect("report");
        assert_eq!(v["loads"]["legacy_meshes"], 1, "{v}");
        assert_eq!(v["loads"]["images_rgba8"], 1, "{v}");
        assert_eq!(v["loads"]["cache_loads"], 2, "{v}");
        let v = crate::dispatch::call_in_process(&mut world, "assets.handles", json!({}))
            .expect("synchronous method")
            .expect("report");
        let kinds: Vec<&str> = v["kinds"]
            .as_array()
            .map(|k| k.iter().filter_map(|k| k["kind"].as_str()).collect())
            .unwrap_or_default();
        assert!(kinds.contains(&"mesh") && kinds.contains(&"image"), "{kinds:?}");
        drop(held);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_progress_lists_sets_and_rejects_unknown_names() {
        let mut world = World::new();
        assert!(load_progress(&mut world, &json!({})).is_err());
        let sets = regia_asset::LoadSets::default();
        sets.define(
            regia_asset::LoadSet::new("menu")
                .with(regia_asset::LoadKind::Texture, "logo", 0, 10)
                .with(regia_asset::LoadKind::Pipeline, "menu ui", 0, 0),
        );
        sets.mark_resident(regia_asset::LoadKind::Texture, "logo");
        world.insert_resource(sets);
        let v = load_progress(&mut world, &json!({ "set": "menu" })).expect("progress");
        assert_eq!(v["sets"][0]["items_resident"], 1);
        assert_eq!(v["sets"][0]["resident"], false);
        assert_eq!(v["sets"][0]["kinds"][1]["kind"], "pipeline");
        assert!(load_progress(&mut world, &json!({ "set": "nope" })).is_err());
        let all = load_progress(&mut world, &json!({})).expect("all");
        assert_eq!(all["sets"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn warm_and_boot_json_shapes() {
        let mut world = World::new();
        assert!(pipeline_warm(&mut world, &json!({})).is_err());
        world.insert_resource(regia_render::PipelineWarmQueue::new(1));
        let v = pipeline_warm(&mut world, &json!({})).expect("warm");
        assert_eq!(v["tiers"].as_array().map(Vec::len), Some(5));
        assert_eq!(v["tiers"][0]["tier"], "boot");
        assert_eq!(v["started"], false);
        regia_render::boot::mark_named("agent_test_mark");
        let t = boot_timeline().expect("timeline");
        assert!(
            t["marks"]
                .as_array()
                .is_some_and(|m| m.iter().any(|x| x["mark"] == "agent_test_mark"))
        );
        assert!(memory_budget(&mut world, &json!({ "tier": "nope" })).is_err());
        assert!(memory_budget(&mut world, &json!({})).is_err(), "no device");
    }

    /// (d) of the DO27 first-frame-stall brief (ADR-7883): `render.scene_ready` walks idle ->
    /// preparing -> presented through the dispatcher, with the render path's systems (headless:
    /// the first renderable tick is presented) and one `SceneReady` message.
    #[test]
    fn scene_ready_walks_idle_preparing_presented_through_the_dispatcher() {
        use regia_ecs::prelude::{Messages, Schedule};
        use regia_render::scene_ready::{
            advance_headless_scene_ready, publish_scene_ready, take_scene_prepare,
        };
        let mut world = World::new();
        let state = |world: &mut World| {
            let v = crate::dispatch::call_in_process(world, "render.scene_ready", json!({}))
                .expect("answered now")
                .expect("render.scene_ready");
            v["state"].as_str().map(str::to_owned).unwrap_or_default()
        };
        let missing = crate::dispatch::call_in_process(&mut world, "render.scene_ready", json!({}))
            .expect("answered now");
        let code = missing.err().and_then(|e| e.data).map(|d| d["code"].clone());
        assert_eq!(code, Some(json!("F0011")));
        world.init_resource::<regia_render::SceneReadiness>();
        world.init_resource::<regia_render::GameFrameWarm>();
        world.init_resource::<Messages<regia_render::SceneReady>>();
        world.insert_resource(regia_render::ExtractedFrame::default());
        let mut render = Schedule::default();
        render.add_systems(
            (
                take_scene_prepare,
                advance_headless_scene_ready,
                publish_scene_ready,
            )
                .chain(),
        );
        render.run(&mut world);
        assert_eq!(state(&mut world), "idle");
        world
            .resource_mut::<regia_render::SceneReadiness>()
            .prepare("bore_twelve");
        assert_eq!(state(&mut world), "preparing");
        render.run(&mut world);
        assert_eq!(state(&mut world), "preparing", "no renderable frame yet");
        world.insert_resource(regia_render::pipeline_warm::canonical_frame());
        render.run(&mut world);
        let v = crate::dispatch::call_in_process(&mut world, "render.scene_ready", json!({}))
            .expect("answered now")
            .expect("render.scene_ready");
        assert_eq!(v["state"], json!("presented"));
        assert_eq!(v["map_id"], json!("bore_twelve"));
        assert_eq!(
            v["presented_to_window"],
            json!(false),
            "headless presents nothing"
        );
        render.run(&mut world);
        let sent = world.resource::<Messages<regia_render::SceneReady>>();
        assert_eq!(sent.len(), 1, "one SceneReady per prepare");
    }

    /// `render.memory_budget {"tier":"integrated_low"}` through the dispatcher on a headless device
    /// answers the integrated-GPU floor row, 1536 MiB GPU memory and 2560 MiB RAM (ADR-7977 build B,
    /// ADR-8878); an unknown tier's error lists every `BudgetTier::ALL` name, four of them.
    #[test]
    fn memory_budget_answers_the_integrated_low_row() {
        let mut world = World::new();
        let unknown =
            crate::dispatch::call_in_process(&mut world, "render.memory_budget", json!({ "tier": "tiny" }))
                .expect("answered now")
                .expect_err("an unknown tier is refused");
        assert_eq!(unknown.code, protocol::INVALID_PARAMS);
        for name in ["minimum", "deck", "desktop_high", "integrated_low"] {
            assert!(unknown.message.contains(name), "{name} in {:?}", unknown.message);
        }
        assert_eq!(regia_render::BudgetTier::ALL.len(), 4);

        world.insert_resource(regia_render::RenderDevice::headless().expect("headless device (lavapipe)"));
        let v = crate::dispatch::call_in_process(
            &mut world,
            "render.memory_budget",
            json!({ "tier": "integrated_low" }),
        )
        .expect("answered now")
        .expect("render.memory_budget integrated_low");
        assert_eq!(v["tier"], "integrated_low");
        assert_eq!(v["budget"]["vram"], 1536u64 << 20);
        assert_eq!(v["budget"]["ram"], 2560u64 << 20);
    }

    #[test]
    fn budget_json_reports_admission() {
        let budget = regia_render::MemoryBudget::for_tier(regia_render::BudgetTier::Minimum);
        let sets = regia_asset::LoadSets::default();
        sets.define(regia_asset::LoadSet::new("map").with(regia_asset::LoadKind::Mesh, "m", 1, 2));
        // A measured device (allocator report and RSS sample): the set fits.
        let mut report = regia_render::MemoryReport::from_allocations("Vulkan", [("frame hdr", 1)], 1);
        report.rss_bytes = Some(1);
        let current = regia_render::memory_use(&report, &sets);
        let a = regia_render::admit(&budget, &current, &sets, "map");
        let v = budget_json(regia_render::BudgetTier::Minimum, &budget, &current, Some(&a));
        assert_eq!(v["admission"]["fits"], true);
        // No allocator report (GL, Metal, WebGPU): not admitted, and the reason says why.
        let blind = regia_render::memory_use(&regia_render::MemoryReport::default(), &sets);
        let a = regia_render::admit(&budget, &blind, &sets, "map");
        let v = budget_json(regia_render::BudgetTier::Minimum, &budget, &blind, Some(&a));
        assert_eq!(v["admission"]["fits"], false);
        assert!(
            v["admission"]["reason"]
                .as_str()
                .is_some_and(|r| r.contains("not measured"))
        );
        assert_eq!(v["sets"][0]["vram_pending"], 2);
        assert_eq!(v["tier"], "minimum");
    }

    /// `render.pipeline_warm` through the dispatcher lists the registered warm targets and the
    /// newest pipeline creations with their target and thread (ADR-7885).
    #[test]
    fn pipeline_warm_reports_registered_targets_and_recent_pipelines() {
        let mut world = World::new();
        world.insert_resource(regia_render::PipelineWarmQueue::new(1));
        let queue = world.resource::<regia_render::PipelineWarmQueue>().clone();
        queue.warm_target(wgpu::TextureFormat::Rgba8UnormSrgb, 4);
        let v = crate::dispatch::call_in_process(&mut world, "render.pipeline_warm", json!({}))
            .expect("synchronous method")
            .expect("protocol call");
        regia_render::pipeline_warm::clear_warm_targets();
        let targets = v["targets"].as_array().cloned().unwrap_or_default();
        assert!(
            targets
                .iter()
                .any(|t| t["format"] == "Rgba8UnormSrgb" && t["samples"] == 4),
            "{v}"
        );
        assert!(v["recent_pipelines"].is_array(), "{v}");
        // No install prepare ran in this process (regia_render's prepare_maps test covers a run).
        assert!(v["prepare"].is_null(), "{v}");
    }

    /// ADR-7891: `render.set_host_budget` through the dispatcher sets the process override, keeps
    /// a side left out, refuses bad params with `host_budget_bad_params`, and `clear` restores the
    /// profile budget. The override is process-wide (the coverage sweep sets it too), so the test
    /// reads each answer, which is built right after the call sets it.
    #[test]
    fn set_host_budget_round_trips_through_the_dispatcher() {
        let mut world = World::new();
        world.init_resource::<regia_app::Time>();
        let call = |world: &mut World, params: Value| {
            crate::dispatch::call_in_process(world, "render.set_host_budget", params)
                .expect("synchronous method")
        };
        let v = call(&mut world, json!({ "cpu_mesh_bytes": 64u64 << 20 })).expect("set");
        assert_eq!(v["override"]["cpu_mesh_bytes"], 64u64 << 20, "{v}");
        let v = call(&mut world, json!({ "cpu_texture_bytes": 7 })).expect("set texture");
        assert_eq!(v["override"]["cpu_mesh_bytes"], 64u64 << 20, "kept: {v}");
        assert_eq!(v["override"]["cpu_texture_bytes"], 7, "{v}");
        for bad in [
            json!({}),
            json!({ "cpu_mesh_bytes": -1 }),
            json!({ "cpu_mesh_bytes": "64" }),
            json!({ "clear": "yes" }),
            json!({ "clear": true, "cpu_mesh_bytes": 1 }),
        ] {
            let err = call(&mut world, bad.clone()).expect_err("refused");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
            assert_eq!(
                err.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("host_budget_bad_params"),
                "{bad}"
            );
        }
        let v = call(&mut world, json!({ "clear": true })).expect("clear");
        assert_eq!(v["override"], Value::Null, "{v}");
        assert!(v["rereads"].is_u64() && v["over_budget_ticks"].is_u64(), "{v}");
    }

    /// `render.pipeline_warm`'s `offscreen` (ADR-1965): absent or null asks for nothing; a size
    /// needs both sides as whole numbers in 1..=16384; anything else is INVALID_PARAMS.
    #[test]
    fn the_offscreen_param_is_a_render_size() {
        assert_eq!(offscreen_size(&serde_json::json!({})).expect("absent"), None);
        assert_eq!(
            offscreen_size(&serde_json::json!({ "offscreen": null })).expect("null"),
            None
        );
        assert_eq!(
            offscreen_size(&serde_json::json!({ "offscreen": { "width": 320, "height": 180 } }))
                .expect("size"),
            Some((320, 180))
        );
        for bad in [
            serde_json::json!({ "offscreen": { "width": 320 } }),
            serde_json::json!({ "offscreen": { "width": 0, "height": 180 } }),
            serde_json::json!({ "offscreen": { "width": 16_385, "height": 180 } }),
            serde_json::json!({ "offscreen": { "width": 1.5, "height": 180 } }),
            serde_json::json!({ "offscreen": { "width": "320", "height": 180 } }),
            serde_json::json!({ "offscreen": [320, 180] }),
        ] {
            let err = offscreen_size(&bad).expect_err("refused");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }

    /// The `offscreen_warm` block before any warm, and a warm asked for in an app with nothing to
    /// render: `F0011`, and still `idle` (nothing started).
    #[test]
    fn an_offscreen_warm_needs_a_renderable_frame() {
        assert_eq!(
            OffscreenWarmState::default().to_json(),
            serde_json::json!({ "state": "idle", "size": null, "last": null })
        );
        let mut world = World::new();
        let err = start_offscreen_warm(&mut world, (64, 64)).expect_err("nothing to warm");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0011"));
        assert!(!offscreen_warm_busy(&world));
        advance_offscreen_warm(&mut world);
        assert!(world.get_resource::<OffscreenWarmState>().is_none());
    }
}
