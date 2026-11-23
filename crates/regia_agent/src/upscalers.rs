// SPDX-License-Identifier: MPL-2.0
//! `render.get_upscalers` / `render.set_upscaler`: the temporal upscaler and the upscale quality
//! mode on the agent protocol (`book/agents/upscalers.md`, ADR-324).
//!
//! `get` lists every backend with the availability probe's verdict
//! ([`regia_render::upscaler::available`]), the backend and quality mode in effect and the
//! quality table. `set` writes [`RenderProfile::temporal_upscaler`] and
//! [`RenderProfile::upscale_quality`] for the next extracted frame. A backend that cannot run
//! is refused with `data.error = "upscaler_unavailable"` and its `reason`, and nothing changes;
//! an available vendor (DLSS in a `dlss` build on an NVIDIA device with NGX ready, ADR-2002) is
//! selected on the runtime renderer. `render.set_vendor_upscaler` selects a vendor even where
//! it cannot run, and `get`'s `vendor_frame` shows what the frame did (active or the fallback).

use regia_ecs::prelude::*;
use regia_render::reactive_mask::{self, ReactiveSources};
use regia_render::upscaler::plugin::{
    self, InputContract, JitterSign, JitterUnits, MotionDirection, MotionUnits,
};
use regia_render::upscaler::plugin::{VendorRuntime, VendorRuntimes};
use regia_render::upscaler::reference::{self, MotionVectorMode, ReferenceCheck, SpatialFilter};
use regia_render::upscaler::vendor::dlss::{VendorFrameReport, dlss_status};
use regia_render::upscaler::vendor::{JitterSequence, LibraryName, RuntimeLibrary};
use regia_render::upscaler::{self, UpscalerFeatures};
use regia_render::{
    AntiAliasing, RenderDevice, RenderProfile, RenderSettings, RuntimeRenderer, UpscaleQuality, UpscalerKind,
    UpscalerStatus, VendorUpscaler,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `data.error` of a refused backend.
pub(crate) const UPSCALER_UNAVAILABLE: &str = "upscaler_unavailable";
/// `data.error` of a `backend` name that is not an upscaler.
pub(crate) const UNKNOWN_UPSCALER: &str = "unknown_upscaler";
/// `data.error` of a `quality` name that is not a mode.
pub(crate) const UNKNOWN_QUALITY: &str = "unknown_quality";
/// `data.error` of a `filter` name that is not a reference filter.
pub(crate) const UNKNOWN_FILTER: &str = "unknown_filter";
/// `data.error` of a `motion_vectors` name that is not a mode.
pub(crate) const UNKNOWN_MOTION_VECTORS: &str = "unknown_motion_vectors";
/// `data.error` of a number outside its documented range.
pub(crate) const OUT_OF_RANGE: &str = "out_of_range";

/// Largest display axis `render.reference_upscale` accepts (it runs on the main thread).
const REFERENCE_MAX_DISPLAY: u64 = 128;
/// Most frames `render.reference_upscale` runs.
const REFERENCE_MAX_FRAMES: u64 = 64;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource in this app",
        "the app runs without regia_render's RenderPlugin",
    )
}

fn no_runtime_renderer() -> RpcError {
    RpcError::not_found(
        "no runtime renderer: this app has no RuntimeRenderer",
        "add regia_render::RenderPlugin",
    )
}

/// Adds the snake_case `data.error` code (and more fields) to a Regia Engine error.
fn coded(mut error: RpcError, code: &str, extra: Value) -> RpcError {
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("error".into(), json!(code));
        if let Value::Object(fields) = extra {
            data.extend(fields);
        }
    }
    error
}

/// The probe's verdict for the running device; without one, the built-ins only.
fn statuses(world: &World) -> (Vec<UpscalerStatus>, Value) {
    match world.get_resource::<RenderDevice>() {
        Some(device) => {
            let info = device.adapter.get_info();
            // DLSS asks NGX about this device (an empty requirement list is enabled). Without
            // the binding, the flag is whether the last hook recorded any device extension
            // (ADR-2002). XeSS validates preparation for this exact device (ADR-4656).
            let recorded_nonempty = !dlss_status().device_extensions.is_empty();
            let per_device = regia_render::upscaler::vendor::dlss::ngx_extensions_on_device(&device.device);
            let features = UpscalerFeatures {
                dlss_vulkan_extensions: regia_render::upscaler::vendor::dlss::dlss_vulkan_extensions_flag(
                    per_device,
                    recorded_nonempty,
                ),
                xess_vulkan_extensions: regia_render::upscaler::vendor::xess::device_prepared(&device.device),
            };
            let mut runtimes = VendorRuntimes::current();
            // XeSS SR is Vulkan-only in this adapter. A different device in this process may
            // be ready; that does not make an unprepared Vulkan or DX12 device available.
            runtimes.xess = xess_runtime_for_device(runtimes.xess, features.xess_vulkan_extensions);
            let list = plugin::available_with(
                Some(&plugin::ProbeDevice {
                    adapter: &info,
                    backend: info.backend,
                    features,
                }),
                &runtimes,
            );
            let adapter = json!({
                "name": info.name,
                "vendor": info.vendor,
                "device_type": format!("{:?}", info.device_type),
                "backend": format!("{:?}", info.backend),
            });
            (list, adapter)
        }
        None => (upscaler::available_without_device(), Value::Null),
    }
}

fn status_json(status: &UpscalerStatus) -> Value {
    json!({
        "backend": status.kind.name(),
        "vendor": matches!(status.kind, UpscalerKind::Vendor(_)),
        "available": status.available,
        "reason": status.reason,
        "blockers": status.blockers,
        "unavailable_code": status.unavailable_code(),
        "inputs": plugin::probe_for(status.kind).map(|probe| contract_json(&probe.contract())),
        "runtime": runtime_json(status.kind),
        "resolve": resolve_json(status.kind),
    })
}

/// Where a built-in resolve differs from the reference algorithm it ports (ADR-7875): `fsr3` names
/// the SDK and lists each difference; the others port no reference (`null`).
fn resolve_json(kind: UpscalerKind) -> Value {
    if kind != UpscalerKind::Builtin(regia_render::TemporalUpscaler::Fsr3) {
        return Value::Null;
    }
    let differences: Vec<Value> = regia_render::FSR3_REFERENCE_DIFFERENCES
        .iter()
        .map(|d| json!({ "id": d.id, "adr": d.adr, "summary": d.summary }))
        .collect();
    json!({ "reference": regia_render::FSR3_REFERENCE, "differences": differences })
}

/// Where the backend's runtime comes from (ADR-1150): built-ins are compiled in; a vendor row
/// names this build's binding state, the library it looks for, where it looked and what it found.
/// Reads directory entries only; loads nothing.
fn runtime_json(kind: UpscalerKind) -> Value {
    let paths = |paths: &[std::path::PathBuf]| -> Vec<String> {
        paths.iter().map(|path| path.display().to_string()).collect()
    };
    match kind {
        UpscalerKind::Builtin(_) => json!({ "state": VendorRuntime::CompiledIn.name() }),
        UpscalerKind::Vendor(vendor) => {
            let library = RuntimeLibrary::current(vendor);
            let search = library.locate_for_this_process();
            let names: Vec<String> = library
                .names
                .iter()
                .map(|name| match name {
                    LibraryName::Exact(file) => (*file).to_string(),
                    LibraryName::Versioned(stem) => format!("{stem}.<version>"),
                })
                .collect();
            let mut row = json!({
                "state": VendorRuntimes::current().get(vendor).name(),
                "env_var": library.env_var,
                "library_names": names,
                "found": search.found.as_ref().map(|path| path.display().to_string()),
                "searched": paths(&search.searched),
                "refused_relative": search.refused_relative.as_ref().map(|path| path.display().to_string()),
            });
            if vendor == VendorUpscaler::Dlss
                && let Value::Object(fields) = &mut row
            {
                let status = dlss_status();
                fields.insert("binding_built".into(), json!(status.binding_built));
                fields.insert("sdk_version".into(), json!(status.sdk_version));
                fields.insert("code".into(), json!(status.code));
                fields.insert("instance_extensions".into(), json!(status.instance_extensions));
                fields.insert("device_extensions".into(), json!(status.device_extensions));
            }
            if vendor == VendorUpscaler::Xess
                && let Value::Object(fields) = &mut row
            {
                let status = regia_render::upscaler::vendor::xess::xess_status();
                fields.insert("binding_built".into(), json!(status.binding_built));
                fields.insert("sdk_version".into(), json!(status.sdk_version));
                fields.insert("runtime_version".into(), json!(status.runtime_version));
                fields.insert("temp_heaps".into(), json!(status.temp_heaps));
                fields.insert("code".into(), json!(status.code));
                fields.insert("instance_extensions".into(), json!(status.instance_extensions));
                fields.insert("device_extensions".into(), json!(status.device_extensions));
            }
            row
        }
        _ => Value::Null,
    }
}

/// The backend's input contract (ADR-510): what it reads and in which units.
fn contract_json(contract: &InputContract) -> Value {
    let c = contract.conventions;
    json!({
        "depth": contract.depth.name(),
        "motion": contract.motion.name(),
        "reactive": contract.reactive.name(),
        "transparency": contract.transparency.name(),
        "motion_units": match c.motion_units {
            MotionUnits::Uv => "uv",
            MotionUnits::RenderPixels => "render_pixels",
        },
        "motion_direction": match c.motion_direction {
            MotionDirection::ToPrevious => "to_previous",
            MotionDirection::ToCurrent => "to_current",
        },
        "jitter_units": match c.jitter_units {
            JitterUnits::Ndc => "ndc",
            JitterUnits::RenderPixels => "render_pixels",
        },
        "jitter_sign": match c.jitter_sign {
            JitterSign::Projection => "projection",
            JitterSign::Sample => "sample",
        },
        "vulkan_device_hook": contract.vulkan_device_hook,
        "max_ratio": f64::from(contract.max_ratio_permille) / 1000.0,
    })
}

fn current_json(profile: &RenderProfile) -> Value {
    json!({
        "backend": UpscalerKind::Builtin(profile.temporal_upscaler).name(),
        "quality": profile.upscale_quality.map(UpscaleQuality::name),
        "render_scale": profile.render_scale,
        "temporal": profile.anti_aliasing == AntiAliasing::Taa,
    })
}

fn qualities_json() -> Vec<Value> {
    UpscaleQuality::ALL
        .iter()
        .map(|mode| {
            let (min, max) = mode.permille_range();
            json!({
                "quality": mode.name(),
                "ratio": mode.ratio(),
                "permille": mode.permille(),
                "dynamic_range_permille": [min, max],
                "render_1440p": mode.render_size((2560, 1440)),
                "render_2160p": mode.render_size((3840, 2160)),
            })
        })
        .collect()
}

fn state(
    settings: &RenderSettings,
    list: &[UpscalerStatus],
    adapter: Value,
    vendor: Value,
    reactive: Value,
) -> Value {
    json!({
        "current": current_json(&settings.profile),
        "upscalers": list.iter().map(status_json).collect::<Vec<_>>(),
        "qualities": qualities_json(),
        "adapter": adapter,
        "vendor_frame": vendor,
        "reactive_mask": reactive,
    })
}

/// What fed the reactive mask on the last frame (ADR-3101); `null` without a runtime renderer.
/// `sources` lists the passes that wrote it (`oit`); `layers` says where the translucent layers
/// (particles, trails, decals) draw: after the resolve, so they never write it.
fn reactive_json(report: Option<ReactiveSources>) -> Value {
    let Some(report) = report else {
        return Value::Null;
    };
    json!({
        "sources": report.names(),
        "present": report.has_mask(),
        "readers": reactive_mask::READERS,
        "layers": {
            "installed": report.layers_installed,
            "placement": report.layer_placement.name(),
            "writes_mask": false,
        },
    })
}

fn reactive_sources(world: &World) -> Value {
    reactive_json(
        world
            .get_resource::<RuntimeRenderer>()
            .map(|renderer| ReactiveSources::of(&renderer.0)),
    )
}

/// What the frame's vendor slot did (ADR-2002); `null` without a runtime renderer. `backend` is
/// the built-in upscaler that runs when the vendor does not: while a selected vendor is not
/// running, `fallback_backend` names it and `reason` says why when the runtime is the cause.
fn vendor_frame_json(report: Option<VendorFrameReport>, backend: Option<&'static str>) -> Value {
    let Some(report) = report else {
        return Value::Null;
    };
    let waiting = report.selected.is_some() && !report.active;
    json!({
        "selected": report.selected.map(VendorUpscaler::name),
        "active": report.active,
        "fallback": report.fallback,
        "fallback_backend": if waiting { backend } else { None },
        "reason": report.runtime.and_then(VendorRuntime::blocker).filter(|_| waiting),
        "frames": report.frames,
        "fallbacks": report.fallbacks,
        "runtime": report.runtime.map(VendorRuntime::name),
        "render_range": report.render_range.map(|(min, max)| json!({ "min": [min.0, min.1], "max": [max.0, max.1] })),
        "render_preferred": report.render_preferred.map(|size| json!([size.0, size.1])),
    })
}

fn vendor_frame(world: &World) -> Value {
    let backend = world
        .get_resource::<RenderSettings>()
        .map(|settings| UpscalerKind::Builtin(settings.profile.temporal_upscaler).name());
    vendor_frame_json(
        world
            .get_resource::<RuntimeRenderer>()
            .map(|renderer| renderer.0.vendor_upscaler_report()),
        backend,
    )
}

/// `render.get_upscalers`: every backend with `available` and `reason`, the backend and quality
/// mode in effect, the quality table and the adapter the probe looked at.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let (list, adapter) = statuses(world);
    let vendor = vendor_frame(world);
    let reactive = reactive_sources(world);
    let settings = world.get_resource::<RenderSettings>().ok_or_else(no_renderer)?;
    Ok(state(settings, &list, adapter, vendor, reactive))
}

/// `render.set_vendor_upscaler { vendor }`: selects a vendor upscaler (or `none`) on the
/// runtime renderer even where it cannot run; the frame then falls back to its built-in
/// upscaler and `vendor_frame.fallback` says why (ADR-2002). The result says up front whether
/// the vendor can run here and, if not, its stable code and the built-in that runs (ADR-2679).
pub(crate) fn set_vendor(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (list, _) = statuses(world);
    set_vendor_with(world, params, &list)
}

/// `set_vendor` against an explicit probe result (the tests pass synthetic adapters).
fn set_vendor_with(world: &mut World, params: &Value, list: &[UpscalerStatus]) -> Result<Value, RpcError> {
    let name = params
        .get("vendor")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`vendor` is required: \"dlss\", \"xess\" or \"none\""))?;
    let vendor = match name {
        "none" => None,
        other => match UpscalerKind::from_name(other) {
            Some(UpscalerKind::Vendor(vendor)) => Some(vendor),
            _ => {
                return Err(coded(
                    RpcError::invalid_params(format!(
                        "unknown vendor upscaler {other:?}: dlss, xess or none"
                    )),
                    UNKNOWN_UPSCALER,
                    json!({ "vendor": other }),
                ));
            }
        },
    };
    let before = vendor_frame(world);
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_runtime_renderer)?;
    renderer.0.set_vendor_upscaler(vendor);
    let after = vendor_frame(world);
    let status = vendor.and_then(|v| list.iter().find(|s| s.kind == UpscalerKind::Vendor(v)));
    let cannot_run = status.is_some_and(|s| !s.available);
    let backend = world
        .get_resource::<RenderSettings>()
        .map(|settings| UpscalerKind::Builtin(settings.profile.temporal_upscaler).name());
    Ok(json!({
        "before": before,
        "after": after,
        "runtime": vendor.map(|v| VendorRuntimes::current().get(v).name()),
        "available": status.map(|s| s.available),
        "reason": status.and_then(|s| s.reason),
        "blockers": status.map(|s| s.blockers.clone()),
        "unavailable_code": status.and_then(UpscalerStatus::unavailable_code),
        "fallback_backend": if cannot_run { backend } else { None },
    }))
}

/// `render.set_upscaler { backend?, quality? }`: switches the temporal upscaler and/or the
/// quality mode (`quality: null` or `"custom"` returns to the profile's explicit render scale).
/// Every parameter is checked before anything changes. Returns `{ before, after, note }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (list, adapter) = statuses(world);
    set_with(world, params, &list, adapter)
}

/// `set` against an explicit probe result (the tests pass synthetic adapters).
fn set_with(
    world: &mut World,
    params: &Value,
    list: &[UpscalerStatus],
    adapter: Value,
) -> Result<Value, RpcError> {
    let backend = match params.get("backend") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(UpscalerKind::from_name(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!(
                    "unknown upscaler {name:?}: one of taau, tsr, fsr3, dlss, xess"
                )),
                UNKNOWN_UPSCALER,
                json!({ "backend": name }),
            )
        })?),
        Some(_) => return Err(RpcError::invalid_params("`backend` must be a string")),
    };
    // `None` leaves the mode alone; `Some(None)` clears it.
    let quality = match params.get("quality") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(name)) if name == "custom" => Some(None),
        Some(Value::String(name)) => Some(Some(UpscaleQuality::from_name(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!(
                    "unknown quality {name:?}: one of native, quality, balanced, performance, \
                     ultra_performance, custom"
                )),
                UNKNOWN_QUALITY,
                json!({ "quality": name }),
            )
        })?)),
        Some(_) => {
            return Err(RpcError::invalid_params("`quality` must be a string or null"));
        }
    };
    if backend.is_none() && quality.is_none() {
        return Err(RpcError::invalid_params("give `backend`, `quality` or both"));
    }
    let temporal = match backend {
        None => None,
        Some(kind) => {
            let status = list.iter().find(|s| s.kind == kind);
            match (kind, status) {
                (UpscalerKind::Builtin(builtin), Some(s)) if s.available => Some(Ok(builtin)),
                (UpscalerKind::Vendor(vendor), Some(s)) if s.available => Some(Err(vendor)),
                _ => {
                    let reason = status
                        .and_then(|s| s.reason)
                        .unwrap_or(upscaler::reason::SDK_NOT_BUNDLED);
                    return Err(coded(
                        RpcError::new(
                            protocol::UNSUPPORTED,
                            "F0013",
                            format!("upscaler {} is unavailable here: {reason}", kind.name()),
                            "render.get_upscalers lists every backend's reason; taau, tsr and fsr3 run everywhere",
                        ),
                        UPSCALER_UNAVAILABLE,
                        json!({
                            "backend": kind.name(),
                            "reason": reason,
                            "blockers": status.map(|s| s.blockers.clone()).unwrap_or_default(),
                        }),
                    ));
                }
            }
        }
    };
    let reactive = reactive_sources(world);
    if matches!(temporal, Some(Err(_))) && !world.contains_resource::<RuntimeRenderer>() {
        return Err(no_runtime_renderer());
    }
    // Snapshot the vendor slot before the change. `before` and `after` are the same state
    // `render.get_upscalers` returns, so a null vendor frame means there is no runtime renderer.
    let before_vendor = vendor_frame(world);
    let (before, vendor_choice) = {
        let mut settings = world
            .get_resource_mut::<RenderSettings>()
            .ok_or_else(no_renderer)?;
        let before = state(&settings, list, adapter.clone(), before_vendor, reactive.clone());
        // A built-in goes into the profile and clears a vendor selection; an available vendor is
        // selected on the runtime renderer, ahead of the profile's built-in (ADR-2002).
        let vendor_choice = match temporal {
            Some(Ok(builtin)) => {
                settings.profile.temporal_upscaler = builtin;
                Some(None)
            }
            Some(Err(vendor)) => Some(Some(vendor)),
            None => None,
        };
        if let Some(mode) = quality {
            settings.profile.upscale_quality = mode;
        }
        (before, vendor_choice)
    };
    if let Some(choice) = vendor_choice
        && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
    {
        renderer.0.set_vendor_upscaler(choice);
    }
    let after_vendor = vendor_frame(world);
    let settings = world.get_resource::<RenderSettings>().ok_or_else(no_renderer)?;
    let after = state(settings, list, adapter, after_vendor, reactive);
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next extracted frame; the backend runs on TAA frames only \
                 (current.temporal), and switching drops its history",
    }))
}

fn out_of_range(name: &str, range: &str) -> RpcError {
    coded(
        RpcError::invalid_params(format!("`{name}` must be {range}")),
        OUT_OF_RANGE,
        json!({ "param": name, "range": range }),
    )
}

/// Reads an optional `[x, y]` pair of numbers.
fn pair(params: &Value, name: &str) -> Result<Option<[f64; 2]>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() == 2 => match (items[0].as_f64(), items[1].as_f64()) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok(Some([x, y])),
            _ => Err(RpcError::invalid_params(format!("`{name}` must be two numbers"))),
        },
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be [x, y]"))),
    }
}

fn name_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be a string"))),
    }
}

/// Parses the `render.reference_upscale` parameters; every one is checked before anything runs.
fn reference_check(params: &Value) -> Result<ReferenceCheck, RpcError> {
    let mut check = ReferenceCheck::default();
    if let Some(name) = name_param(params, "filter")? {
        check.filter = SpatialFilter::from_name(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!("unknown filter {name:?}: bilinear or edge_adaptive")),
                UNKNOWN_FILTER,
                json!({ "filter": name }),
            )
        })?;
    }
    if let Some(name) = name_param(params, "quality")? {
        check.quality = UpscaleQuality::from_name(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!(
                    "unknown quality {name:?}: one of native, quality, balanced, performance, ultra_performance"
                )),
                UNKNOWN_QUALITY,
                json!({ "quality": name }),
            )
        })?;
    }
    if let Some(name) = name_param(params, "motion_vectors")? {
        check.motion_vectors = MotionVectorMode::from_name(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!(
                    "unknown motion_vectors {name:?}: correct, zero or negated"
                )),
                UNKNOWN_MOTION_VECTORS,
                json!({ "motion_vectors": name }),
            )
        })?;
    }
    if let Some([w, h]) = pair(params, "display")? {
        let max = REFERENCE_MAX_DISPLAY as f64;
        let ok = |v: f64| v.fract() == 0.0 && (8.0..=max).contains(&v);
        if !ok(w) || !ok(h) {
            return Err(out_of_range("display", "two integers from 8 to 128"));
        }
        check.display_size = (w as u32, h as u32);
    }
    match params.get("frames") {
        None | Some(Value::Null) => {}
        Some(value) => match value.as_u64() {
            Some(frames) if (1..=REFERENCE_MAX_FRAMES).contains(&frames) => check.frames = frames as u32,
            _ => return Err(out_of_range("frames", "an integer from 1 to 64")),
        },
    }
    if let Some([x, y]) = pair(params, "motion_px")? {
        if x.abs() > 8.0 || y.abs() > 8.0 {
            return Err(out_of_range(
                "motion_px",
                "two numbers from -8 to 8 (display pixels per frame)",
            ));
        }
        check.motion_px = [x as f32, y as f32];
    }
    match params.get("reactive") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(on)) => check.reactive = *on,
        Some(_) => return Err(RpcError::invalid_params("`reactive` must be a boolean")),
    }
    match params.get("sharpness") {
        None | Some(Value::Null) => {}
        Some(value) => match value.as_f64() {
            Some(s) if (0.0..=1.0).contains(&s) => check.sharpness = s as f32,
            _ => return Err(out_of_range("sharpness", "a number from 0 to 1")),
        },
    }
    Ok(check)
}

/// `render.reference_upscale { filter?, quality?, display?, frames?, motion_px?, motion_vectors?,
/// reactive?, sharpness? }`: runs the software reference upscaler (ADR-510) on its synthetic scene
/// and returns the error against the supersampled truth. Needs no renderer; changes nothing.
pub(crate) fn reference_upscale(params: &Value) -> Result<Value, RpcError> {
    let check = reference_check(params)?;
    let report = reference::run_check(&check).map_err(|error| {
        coded(
            RpcError::invalid_params(error.to_string()),
            error.code(),
            json!({}),
        )
    })?;
    let gain = if report.final_rmse > 0.0 {
        f64::from(report.spatial_only_rmse) / f64::from(report.final_rmse)
    } else {
        0.0
    };
    Ok(json!({
        "check": {
            "filter": check.filter.name(),
            "quality": check.quality.name(),
            "display": check.display_size,
            "frames": report.frames,
            "motion_px": check.motion_px,
            "motion_vectors": check.motion_vectors.name(),
            "reactive": check.reactive,
            "sharpness": check.sharpness,
        },
        "render": report.render_size,
        "first_frame_rmse": report.first_frame_rmse,
        "final_rmse": report.final_rmse,
        "spatial_only_rmse": report.spatial_only_rmse,
        "accumulation_gain": gain,
        "note": "software reference on a synthetic scene (CPU); accumulation_gain above 1 means jitter and \
                 history beat the spatial filter alone",
    }))
}

/// Largest display or render axis `render.upscaler_jitter` accepts.
const JITTER_MAX_AXIS: f64 = 16384.0;
/// Most frames `render.upscaler_jitter` lists.
const JITTER_MAX_FRAMES: u64 = 256;

/// Reads an optional `[w, h]` pair of integers from 1 to `JITTER_MAX_AXIS`.
fn size_param(params: &Value, name: &str) -> Result<Option<(u32, u32)>, RpcError> {
    let Some([w, h]) = pair(params, name)? else {
        return Ok(None);
    };
    let ok = |v: f64| v.fract() == 0.0 && (1.0..=JITTER_MAX_AXIS).contains(&v);
    if !ok(w) || !ok(h) {
        return Err(out_of_range(name, "two integers from 1 to 16384"));
    }
    Ok(Some((w as u32, h as u32)))
}

/// `render.upscaler_jitter { quality?, display?, render?, frames? }`: the projection jitter a
/// vendor-ready frame uses at a render and display size (ADR-1150): the Halton(2, 3) phase
/// count for the ratio and each frame's offset in render pixels (what a vendor runtime is given)
/// and in NDC (what the projection is moved by). Pure; needs no renderer; changes nothing.
pub(crate) fn upscaler_jitter(params: &Value) -> Result<Value, RpcError> {
    let quality = match name_param(params, "quality")? {
        None => None,
        Some(name) => Some(UpscaleQuality::from_name(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!(
                    "unknown quality {name:?}: one of native, quality, balanced, performance, ultra_performance"
                )),
                UNKNOWN_QUALITY,
                json!({ "quality": name }),
            )
        })?),
    };
    let display = size_param(params, "display")?.unwrap_or((2560, 1440));
    let explicit_render = size_param(params, "render")?;
    if quality.is_some() && explicit_render.is_some() {
        return Err(RpcError::invalid_params("give `quality` or `render`, not both"));
    }
    let quality = match explicit_render {
        Some(_) => None,
        None => Some(quality.unwrap_or(UpscaleQuality::Quality)),
    };
    let render = match (explicit_render, quality) {
        (Some(render), _) => render,
        (None, Some(mode)) => mode.render_size(display),
        (None, None) => display,
    };
    if render.0 > display.0 || render.1 > display.1 {
        return Err(out_of_range("render", "no larger than display on either axis"));
    }
    let frames = match params.get("frames") {
        None | Some(Value::Null) => 8,
        Some(value) => match value.as_u64() {
            Some(frames) if (1..=JITTER_MAX_FRAMES).contains(&frames) => frames,
            _ => return Err(out_of_range("frames", "an integer from 1 to 256")),
        },
    };
    let seq = JitterSequence::for_sizes(render, display);
    let offsets: Vec<Value> = (1..=frames)
        .map(|frame| {
            json!({
                "frame": frame,
                "halton_index": seq.halton_index(frame),
                "render_pixels": seq.render_pixels(frame),
                "ndc": seq.ndc(frame, render),
            })
        })
        .collect();
    Ok(json!({
        "quality": quality.map(UpscaleQuality::name),
        "display": display,
        "render": render,
        "ratio": [
            f64::from(display.0) / f64::from(render.0),
            f64::from(display.1) / f64::from(render.1),
        ],
        "sequence": "halton_2_3",
        "phase_count": seq.phase_count(),
        "offsets": offsets,
        "note": "render_pixels: y down, the offset the projection was moved by (what a vendor runtime is \
                 given); ndc: y up, added to the projection. This list repeats after phase_count frames. \
                 The frame's projection keeps the unbounded Halton index, so the two match for frames 1 \
                 through phase_count only",
    }))
}

#[cfg(test)]
mod tests {
    use regia_render::{RenderProfilePreset, TemporalUpscaler};

    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        world
    }

    /// The probe's list with both vendor kinds blocked by `blockers` (what `available` returns
    /// for that adapter; the adapter cases themselves are tested in `regia_render::upscaler`).
    fn vendor_blocked(blockers: &[&'static str]) -> Vec<UpscalerStatus> {
        let mut list = upscaler::available_without_device();
        for status in &mut list {
            if matches!(status.kind, UpscalerKind::Vendor(_)) {
                status.blockers = blockers.to_vec();
                status.reason = blockers.first().copied();
            }
        }
        list
    }

    fn data_str<'a>(error: &'a RpcError, key: &str) -> Option<&'a str> {
        error
            .data
            .as_ref()
            .and_then(|d| d.get(key))
            .and_then(Value::as_str)
    }

    #[test]
    fn get_reports_the_reactive_mask_sources() -> Result<(), RpcError> {
        // ADR-3101: no runtime renderer, no report.
        let mut world = world();
        assert_eq!(get(&mut world)?["reactive_mask"], Value::Null);
        world.insert_resource(RuntimeRenderer::default());
        let got = get(&mut world)?;
        let mask = &got["reactive_mask"];
        assert_eq!(mask["sources"], json!([]), "no frame drew, no source");
        assert_eq!(mask["present"], false);
        assert_eq!(
            mask["readers"],
            json!(["taau", "tsr", "fsr3", "vendor", "frame_gen"])
        );
        assert_eq!(mask["layers"]["installed"], 0);
        assert_eq!(mask["layers"]["placement"], "after_resolve");
        assert_eq!(mask["layers"]["writes_mask"], false);
        // The OIT composite is the one source, and the field's shape is the schema's.
        let oit = reactive_json(Some(ReactiveSources::from_parts(true, 1)));
        assert_eq!(oit["sources"], json!(["oit"]));
        assert_eq!(oit["present"], true);
        assert_eq!(oit["layers"]["installed"], 1);
        Ok(())
    }

    #[test]
    fn get_lists_every_backend_and_the_current_choice() -> Result<(), RpcError> {
        let mut empty = World::new();
        assert!(get(&mut empty).is_err(), "no RenderSettings");
        let mut world = world();
        let got = get(&mut world)?;
        assert_eq!(got["current"]["backend"], "taau");
        assert_eq!(got["current"]["quality"], Value::Null);
        assert_eq!(got["current"]["temporal"], true);
        assert_eq!(got["adapter"], Value::Null, "no device in a bare world");
        let names: Vec<&str> = got["upscalers"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r["backend"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(names, vec!["taau", "tsr", "fsr3", "dlss", "xess"]);
        assert_eq!(got["upscalers"][2]["available"], true);
        assert_eq!(got["upscalers"][3]["available"], false);
        assert_eq!(got["upscalers"][3]["reason"], "no_render_device");
        assert_eq!(got["qualities"][1]["quality"], "quality");
        assert_eq!(got["qualities"][1]["render_1440p"], json!([1707, 960]));
        assert_eq!(got["qualities"][2]["render_2160p"], json!([2259, 1271]));
        Ok(())
    }

    #[test]
    fn an_unavailable_vendor_backend_is_refused_and_changes_nothing() {
        for (statuses, reason) in [
            // NVIDIA on Vulkan without NGX's extensions, NVIDIA with only the SDK missing, AMD on Vulkan.
            (
                vendor_blocked(&["needs_dx12_or_vulkan_extension", "sdk_not_bundled"]),
                "needs_dx12_or_vulkan_extension",
            ),
            (vendor_blocked(&["sdk_not_bundled"]), "sdk_not_bundled"),
            (
                vendor_blocked(&[
                    "needs_nvidia_adapter",
                    "needs_dx12_or_vulkan_extension",
                    "sdk_not_bundled",
                ]),
                "needs_nvidia_adapter",
            ),
            (upscaler::available_without_device(), "no_render_device"),
        ] {
            let mut world = world();
            let before = world.resource::<RenderSettings>().profile.clone();
            let result = set_with(
                &mut world,
                &json!({ "backend": "dlss", "quality": "performance" }),
                &statuses,
                Value::Null,
            );
            let Err(error) = result else {
                panic!("dlss was accepted"); // invariant: test-only; the vendor kind is unavailable
            };
            assert_eq!(error.code, protocol::UNSUPPORTED);
            assert_eq!(data_str(&error, "code"), Some("F0013"));
            assert_eq!(data_str(&error, "error"), Some(UPSCALER_UNAVAILABLE));
            assert_eq!(data_str(&error, "reason"), Some(reason));
            assert_eq!(data_str(&error, "backend"), Some("dlss"));
            assert_eq!(
                world.resource::<RenderSettings>().profile,
                before,
                "the quality in the same call is not applied either"
            );
        }
        // XeSS on an Android GPU (Mali on Vulkan).
        let mut world = world();
        let before = world.resource::<RenderSettings>().profile.clone();
        let mali = vendor_blocked(&[
            "needs_desktop_gpu",
            "needs_dx12_or_vulkan_extension",
            "sdk_not_bundled",
        ]);
        let result = set_with(&mut world, &json!({ "backend": "XeSS" }), &mali, Value::Null);
        let Err(error) = result else {
            panic!("xess was accepted"); // invariant: test-only; the vendor kind is unavailable
        };
        assert_eq!(data_str(&error, "reason"), Some("needs_desktop_gpu"));
        assert_eq!(world.resource::<RenderSettings>().profile, before);
    }

    #[test]
    fn a_built_in_the_probe_refuses_is_refused_too() {
        let mut world = world();
        let mut list = upscaler::available_without_device();
        list[1].available = false;
        list[1].reason = Some("needs_hardware_adapter");
        list[1].blockers = vec!["needs_hardware_adapter"];
        let Err(error) = set_with(&mut world, &json!({ "backend": "tsr" }), &list, Value::Null) else {
            panic!("tsr was accepted"); // invariant: test-only; the list says tsr cannot run
        };
        assert_eq!(data_str(&error, "error"), Some(UPSCALER_UNAVAILABLE));
        assert_eq!(data_str(&error, "reason"), Some("needs_hardware_adapter"));
        assert_eq!(
            world.resource::<RenderSettings>().profile.temporal_upscaler,
            TemporalUpscaler::Taau
        );
    }

    #[test]
    fn built_in_backends_and_quality_modes_switch() -> Result<(), RpcError> {
        let mut world = world();
        let list = upscaler::available_without_device();
        let out = set_with(
            &mut world,
            &json!({ "backend": "tsr", "quality": "balanced" }),
            &list,
            Value::Null,
        )?;
        assert_eq!(out["before"]["current"]["backend"], "taau");
        assert_eq!(out["after"]["current"]["backend"], "tsr");
        assert_eq!(out["after"]["current"]["quality"], "balanced");
        let profile = world.resource::<RenderSettings>().profile.clone();
        assert_eq!(profile.temporal_upscaler, TemporalUpscaler::Tsr);
        assert_eq!(profile.upscale_quality, Some(UpscaleQuality::Balanced));
        assert_eq!(
            world.resource::<RenderSettings>().quality().render_scale_permille,
            588
        );

        // Quality alone keeps the backend; `custom` (or null) clears the mode.
        set_with(
            &mut world,
            &json!({ "quality": "ultra-performance" }),
            &list,
            Value::Null,
        )?;
        assert_eq!(
            world.resource::<RenderSettings>().profile.upscale_quality,
            Some(UpscaleQuality::UltraPerformance)
        );
        assert_eq!(
            world.resource::<RenderSettings>().profile.temporal_upscaler,
            TemporalUpscaler::Tsr
        );
        set_with(
            &mut world,
            &json!({ "backend": "fsr3", "quality": "custom" }),
            &list,
            Value::Null,
        )?;
        let profile = world.resource::<RenderSettings>().profile.clone();
        assert_eq!(profile.temporal_upscaler, TemporalUpscaler::Fsr3);
        assert_eq!(profile.upscale_quality, None);
        set_with(&mut world, &json!({ "quality": "native" }), &list, Value::Null)?;
        set_with(&mut world, &json!({ "quality": null }), &list, Value::Null)?;
        assert_eq!(world.resource::<RenderSettings>().profile.upscale_quality, None);
        Ok(())
    }

    #[test]
    fn every_backend_lists_its_input_contract() -> Result<(), RpcError> {
        let mut world = world();
        let got = get(&mut world)?;
        let rows = got["upscalers"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 5);
        let inputs = |name: &str| {
            rows.iter()
                .find(|r| r["backend"] == name)
                .map(|r| r["inputs"].clone())
                .unwrap_or(Value::Null)
        };
        assert_eq!(inputs("taau")["depth"], "optional");
        assert_eq!(inputs("taau")["reactive"], "optional");
        assert_eq!(inputs("tsr")["depth"], "required");
        assert_eq!(inputs("fsr3")["motion_units"], "uv");
        assert_eq!(inputs("fsr3")["jitter_units"], "ndc");
        assert_eq!(inputs("fsr3")["vulkan_device_hook"], false);
        for vendor in ["dlss", "xess"] {
            let row = inputs(vendor);
            assert_eq!(row["motion"], "required", "{vendor}");
            assert_eq!(row["motion_units"], "render_pixels", "{vendor}");
            assert_eq!(row["motion_direction"], "to_previous", "{vendor}");
            assert_eq!(row["jitter_units"], "render_pixels", "{vendor}");
            assert_eq!(row["jitter_sign"], "projection", "{vendor}");
            assert_eq!(row["vulkan_device_hook"], true, "{vendor}");
            assert_eq!(row["max_ratio"], 3.0, "{vendor}");
        }
        Ok(())
    }

    #[test]
    fn reference_upscale_measures_the_reference_and_changes_nothing() -> Result<(), RpcError> {
        // The regia_render tests pin the numbers; here: the shape, the defaults and the
        // reactive identity (a fully reactive frame is the spatial filter exactly).
        let out = reference_upscale(&json!({}))?;
        assert_eq!(out["check"]["filter"], "edge_adaptive");
        assert_eq!(out["check"]["quality"], "performance");
        assert_eq!(out["check"]["display"], json!([64, 64]));
        assert_eq!(out["check"]["frames"], 32);
        assert_eq!(out["render"], json!([32, 32]));
        let gain = out["accumulation_gain"].as_f64().unwrap_or(0.0);
        assert!(gain > 1.3, "jittered accumulation beats one frame: {out}");
        let reactive = reference_upscale(&json!({
            "filter": "bilinear",
            "quality": "quality",
            "display": [48, 24],
            "frames": 4,
            "motion_px": [1, -0.5],
            "reactive": true,
            "sharpness": 0.5,
        }))?;
        assert_eq!(reactive["render"], json!([32, 16]));
        assert_eq!(reactive["check"]["motion_vectors"], "correct");
        assert_eq!(reactive["final_rmse"], reactive["spatial_only_rmse"]);
        Ok(())
    }

    #[test]
    fn reference_upscale_refuses_bad_parameters_with_codes() {
        for (params, code) in [
            (json!({ "filter": "lanczos" }), Some(UNKNOWN_FILTER)),
            (json!({ "quality": "max" }), Some(UNKNOWN_QUALITY)),
            (json!({ "motion_vectors": "up" }), Some(UNKNOWN_MOTION_VECTORS)),
            (json!({ "display": [4, 4] }), Some(OUT_OF_RANGE)),
            (json!({ "display": [8.5, 8] }), Some(OUT_OF_RANGE)),
            (json!({ "display": [256, 64] }), Some(OUT_OF_RANGE)),
            (json!({ "frames": 0 }), Some(OUT_OF_RANGE)),
            (json!({ "frames": 65 }), Some(OUT_OF_RANGE)),
            (json!({ "motion_px": [9, 0] }), Some(OUT_OF_RANGE)),
            (json!({ "sharpness": 2 }), Some(OUT_OF_RANGE)),
            (json!({ "display": "64" }), None),
            (json!({ "display": [64] }), None),
            (json!({ "reactive": "yes" }), None),
            (json!({ "filter": 1 }), None),
        ] {
            let Err(error) = reference_upscale(&params) else {
                panic!("{params} was accepted"); // invariant: test-only; every case is invalid
            };
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{params}");
            assert_eq!(data_str(&error, "error"), code, "{params}");
        }
        for code in [UNKNOWN_FILTER, UNKNOWN_MOTION_VECTORS, OUT_OF_RANGE] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn bad_parameters_are_refused_with_codes_and_change_nothing() {
        let mut world = world();
        let list = upscaler::available_without_device();
        let before = world.resource::<RenderSettings>().profile.clone();
        for (params, code) in [
            (json!({ "backend": "metalfx" }), Some(UNKNOWN_UPSCALER)),
            (
                json!({ "backend": "tsr", "quality": "max" }),
                Some(UNKNOWN_QUALITY),
            ),
            (json!({ "backend": 3 }), None),
            (json!({ "quality": 0.5 }), None),
            (json!({}), None),
        ] {
            let Err(error) = set_with(&mut world, &params, &list, Value::Null) else {
                panic!("{params} was accepted"); // invariant: test-only; every case is invalid
            };
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{params}");
            assert_eq!(data_str(&error, "error"), code, "{params}");
        }
        assert_eq!(world.resource::<RenderSettings>().profile, before);
        for code in [UPSCALER_UNAVAILABLE, UNKNOWN_UPSCALER, UNKNOWN_QUALITY] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn fsr3_row_lists_where_its_resolve_differs_from_the_sdk() -> Result<(), RpcError> {
        let mut world = world();
        let got = get(&mut world)?;
        let rows = got["upscalers"].as_array().cloned().unwrap_or_default();
        let resolve = |name: &str| {
            rows.iter()
                .find(|r| r["backend"] == name)
                .map(|r| r["resolve"].clone())
                .unwrap_or(Value::Bool(false))
        };
        let fsr3 = resolve("fsr3");
        assert_eq!(fsr3["reference"], regia_render::FSR3_REFERENCE);
        let ids: Vec<&str> = fsr3["differences"]
            .as_array()
            .map(|list| list.iter().filter_map(|d| d["id"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(
            ids.len(),
            regia_render::FSR3_REFERENCE_DIFFERENCES.len(),
            "{fsr3}"
        );
        assert!(ids.contains(&"nearest_sample_restart"), "{fsr3}");
        for difference in fsr3["differences"].as_array().into_iter().flatten() {
            assert!(
                difference["adr"]
                    .as_str()
                    .is_some_and(|adr| adr.starts_with("ADR-")),
                "{difference}"
            );
            assert!(
                difference["summary"].as_str().is_some_and(|s| !s.is_empty()),
                "{difference}"
            );
        }
        for other in ["taau", "tsr", "dlss", "xess"] {
            assert_eq!(resolve(other), Value::Null, "{other}");
        }
        Ok(())
    }

    #[test]
    fn every_row_says_where_its_runtime_comes_from() -> Result<(), RpcError> {
        let mut world = world();
        let got = get(&mut world)?;
        let rows = got["upscalers"].as_array().cloned().unwrap_or_default();
        let runtime = |name: &str| {
            rows.iter()
                .find(|r| r["backend"] == name)
                .map(|r| r["runtime"].clone())
                .unwrap_or(Value::Null)
        };
        for builtin in ["taau", "tsr", "fsr3"] {
            assert_eq!(runtime(builtin), json!({ "state": "compiled_in" }), "{builtin}");
        }
        // An SDK-free build says not_bundled; a linked build says its runtime's state (ADR-4907).
        let current = VendorRuntimes::current();
        for (vendor, env_var, state, linked) in [
            (
                "dlss",
                "REGIA_DLSS_PATH",
                current.dlss,
                dlss_status().binding_built,
            ),
            (
                "xess",
                "REGIA_XESS_PATH",
                current.xess,
                regia_render::upscaler::vendor::xess::BINDING_BUILT,
            ),
        ] {
            let row = runtime(vendor);
            assert_eq!(row["state"], state.name(), "{vendor}");
            assert_eq!(state == VendorRuntime::NotBundled, !linked, "{vendor}");
            assert_eq!(row["env_var"], env_var, "{vendor}");
            assert!(row["library_names"].is_array(), "{row}");
            assert!(row["searched"].is_array(), "{row}");
        }
        // The library names are this OS's (the probe never looks at another OS's files).
        let dlss_names = runtime("dlss")["library_names"].clone();
        if cfg!(target_os = "linux") {
            assert_eq!(dlss_names, json!(["libnvidia-ngx-dlss.so.<version>"]));
            assert_eq!(runtime("xess")["library_names"], json!([]));
        } else if cfg!(target_os = "windows") {
            assert_eq!(dlss_names, json!(["nvngx_dlss.dll"]));
            assert_eq!(runtime("xess")["library_names"], json!(["libxess.dll"]));
        } else {
            assert_eq!(dlss_names, json!([]));
        }
        Ok(())
    }

    #[test]
    fn upscaler_jitter_lists_the_ratio_scaled_sequence() -> Result<(), RpcError> {
        let out = upscaler_jitter(&json!({}))?;
        assert_eq!(out["quality"], "quality");
        assert_eq!(out["display"], json!([2560, 1440]));
        assert_eq!(out["render"], json!([1707, 960]));
        assert_eq!(out["sequence"], "halton_2_3");
        assert_eq!(out["phase_count"], 18);
        let offsets = out["offsets"].as_array().cloned().unwrap_or_default();
        assert_eq!(offsets.len(), 8);
        // Frame 1 is Halton index 1: (1/2, 1/3), so x 0 and y −(1/3 − 1/2) = 1/6 render pixel.
        assert_eq!(offsets[0]["frame"], 1);
        assert_eq!(offsets[0]["halton_index"], 1);
        let px = offsets[0]["render_pixels"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (x, y) = (
            px.first().and_then(Value::as_f64).unwrap_or(f64::NAN),
            px.get(1).and_then(Value::as_f64).unwrap_or(f64::NAN),
        );
        assert!(x.abs() < 1.0e-6 && (y - 1.0 / 6.0).abs() < 1.0e-6, "{px:?}");
        let ndc_y = offsets[0]["ndc"][1].as_f64().unwrap_or(f64::NAN);
        assert!((ndc_y - (-1.0 / 6.0) * 2.0 / 960.0).abs() < 1.0e-7, "{ndc_y}");

        // Performance wraps after 32 frames.
        let perf = upscaler_jitter(&json!({ "quality": "performance", "display": [64, 32], "frames": 33 }))?;
        assert_eq!(perf["render"], json!([32, 16]));
        assert_eq!(perf["phase_count"], 32);
        assert_eq!(perf["offsets"][32]["halton_index"], 1);
        assert_eq!(
            perf["offsets"][32]["render_pixels"],
            perf["offsets"][0]["render_pixels"]
        );
        // An explicit render size: no mode, Ultra Performance's 3× gives 72 phases.
        let custom = upscaler_jitter(&json!({ "render": [100, 50], "display": [300, 150], "frames": 1 }))?;
        assert_eq!(custom["quality"], Value::Null);
        assert_eq!(custom["phase_count"], 72);
        assert_eq!(custom["ratio"], json!([3.0, 3.0]));
        Ok(())
    }

    #[test]
    fn upscaler_jitter_refuses_bad_parameters_with_codes() {
        for (params, code) in [
            (json!({ "quality": "max" }), Some(UNKNOWN_QUALITY)),
            (json!({ "display": [0, 10] }), Some(OUT_OF_RANGE)),
            (json!({ "display": [20000, 10] }), Some(OUT_OF_RANGE)),
            (json!({ "display": [10.5, 10] }), Some(OUT_OF_RANGE)),
            (
                json!({ "render": [200, 10], "display": [100, 100] }),
                Some(OUT_OF_RANGE),
            ),
            (json!({ "frames": 0 }), Some(OUT_OF_RANGE)),
            (json!({ "frames": 257 }), Some(OUT_OF_RANGE)),
            (json!({ "quality": "quality", "render": [10, 10] }), None),
            (json!({ "display": "1440p" }), None),
            (json!({ "quality": 2 }), None),
        ] {
            let Err(error) = upscaler_jitter(&params) else {
                panic!("{params} was accepted"); // invariant: test-only; every case is invalid
            };
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{params}");
            assert_eq!(data_str(&error, "error"), code, "{params}");
        }
    }

    #[test]
    fn set_vendor_selects_on_the_renderer_and_reports_the_frame() -> Result<(), RpcError> {
        let mut world = world();
        assert!(
            set_vendor(&mut world, &json!({ "vendor": "dlss" })).is_err(),
            "no runtime renderer"
        );
        world.insert_resource(RuntimeRenderer::default());
        let got = get(&mut world)?;
        assert_eq!(got["vendor_frame"]["selected"], Value::Null);
        let set = set_vendor(&mut world, &json!({ "vendor": "dlss" }))?;
        assert_eq!(set["before"]["selected"], Value::Null);
        assert_eq!(set["after"]["selected"], "dlss");
        assert_eq!(set["after"]["active"], false);
        assert_eq!(set["after"]["frames"], 0);
        assert!(set["runtime"].is_string());
        let got = get(&mut world)?;
        assert_eq!(got["vendor_frame"]["selected"], "dlss");
        // The DLSS runtime row carries the binding's state (not built in a default build).
        let dlss = &got["upscalers"][3]["runtime"];
        assert_eq!(dlss["binding_built"], dlss_status().binding_built);
        assert!(dlss["device_extensions"].is_array());
        let off = set_vendor(&mut world, &json!({ "vendor": "none" }))?;
        assert_eq!(off["after"]["selected"], Value::Null);
        for (params, code) in [
            (json!({}), None),
            (json!({ "vendor": 3 }), None),
            (json!({ "vendor": "tsr" }), Some(UNKNOWN_UPSCALER)),
            (json!({ "vendor": "bogus" }), Some(UNKNOWN_UPSCALER)),
        ] {
            let Err(error) = set_vendor(&mut world, &params) else {
                panic!("{params} was accepted"); // invariant: test-only; the params are invalid
            };
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{params}");
            assert_eq!(data_str(&error, "error"), code, "{params}");
        }
        Ok(())
    }

    /// A probe of a mocked device with DLSS in `runtime`, the way the running renderer would list it.
    fn dlss_on(info: &wgpu::AdapterInfo, runtime: VendorRuntime) -> Vec<UpscalerStatus> {
        plugin::available_with(
            Some(&plugin::ProbeDevice {
                adapter: info,
                backend: info.backend,
                features: UpscalerFeatures {
                    dlss_vulkan_extensions: true,
                    xess_vulkan_extensions: false,
                },
            }),
            &VendorRuntimes {
                dlss: runtime,
                xess: VendorRuntime::NotBundled,
            },
        )
    }

    fn adapter(vendor: u32, name: &str) -> wgpu::AdapterInfo {
        let mut info = wgpu::AdapterInfo::new(wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        info.vendor = vendor;
        info.name = name.to_string();
        info
    }

    /// ADR-2679: each way DLSS cannot run answers one stable code on both methods, the selection
    /// still holds, and the built-in that runs instead is named. The GTX 1080 row mocks the card
    /// and NGX's answer (`SuperSampling_Available = 0` reaches here as `reports_unsupported`).
    #[test]
    fn every_reason_a_vendor_cannot_run_has_a_stable_code_on_both_methods() -> Result<(), RpcError> {
        let gtx1080 = adapter(0x10DE, "NVIDIA GeForce GTX 1080");
        let rtx = adapter(0x10DE, "NVIDIA GeForce RTX 5090");
        let radeon = adapter(0x1002, "AMD Radeon RX 7900 XTX");
        for (info, runtime, code) in [
            (&rtx, VendorRuntime::NotBundled, "sdk_not_bundled"),
            (&rtx, VendorRuntime::NotFound, "runtime_not_found"),
            (
                &gtx1080,
                VendorRuntime::ReportsUnsupported,
                "runtime_reports_unsupported",
            ),
            (&rtx, VendorRuntime::NeedsNewerDriver, "needs_newer_driver"),
            (&rtx, VendorRuntime::InitFailed, "runtime_init_failed"),
            (&radeon, VendorRuntime::NotBundled, "needs_nvidia_adapter"),
        ] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            let list = dlss_on(info, runtime);
            let mut world = world();
            world.insert_resource(RuntimeRenderer::default());
            // render.get_upscalers: the DLSS row.
            let got = state(
                world.resource::<RenderSettings>(),
                &list,
                Value::Null,
                Value::Null,
                Value::Null,
            );
            assert_eq!(got["upscalers"][3]["backend"], "dlss");
            assert_eq!(got["upscalers"][3]["available"], false, "{code}");
            assert_eq!(got["upscalers"][3]["unavailable_code"], code);
            // render.set_upscaler refuses with it in `blockers`.
            let Err(error) = set_with(&mut world, &json!({ "backend": "dlss" }), &list, Value::Null) else {
                panic!("{code}: dlss was accepted"); // invariant: test-only; the vendor cannot run
            };
            assert_eq!(data_str(&error, "error"), Some(UPSCALER_UNAVAILABLE));
            let blockers = error.data.as_ref().map(|d| d["blockers"].clone());
            assert!(
                blockers.is_some_and(|b| b.as_array().is_some_and(|b| b.iter().any(|v| v == code))),
                "{code}"
            );
            // render.set_vendor_upscaler selects it and says what will happen.
            let out = set_vendor_with(&mut world, &json!({ "vendor": "dlss" }), &list)?;
            assert_eq!(out["available"], false, "{code}");
            assert_eq!(out["unavailable_code"], code);
            assert_eq!(out["fallback_backend"], "taau", "{code}");
            assert_eq!(out["after"]["selected"], "dlss", "{code}");
            assert_eq!(out["after"]["fallback_backend"], "taau", "{code}");
        }
        // A vendor that can run has no code and no fallback; `none` has neither to report.
        let list = dlss_on(&rtx, VendorRuntime::Ready);
        let mut world = world();
        world.insert_resource(RuntimeRenderer::default());
        let out = set_vendor_with(&mut world, &json!({ "vendor": "dlss" }), &list)?;
        assert_eq!(out["available"], true);
        assert_eq!(out["unavailable_code"], Value::Null);
        assert_eq!(out["fallback_backend"], Value::Null);
        let out = set_vendor_with(&mut world, &json!({ "vendor": "none" }), &list)?;
        assert_eq!(out["available"], Value::Null);
        assert_eq!(out["after"]["fallback_backend"], Value::Null);
        Ok(())
    }

    /// What the frame did: the reason next to the fallback, and the sizes the runtime asked for.
    #[test]
    fn the_vendor_frame_shows_the_reason_and_the_preferred_size() {
        let fell_back = VendorFrameReport {
            selected: Some(VendorUpscaler::Dlss),
            fallback: Some("runtime_unavailable"),
            fallbacks: 2,
            runtime: Some(VendorRuntime::NeedsNewerDriver),
            ..VendorFrameReport::default()
        };
        let row = vendor_frame_json(Some(fell_back), Some("tsr"));
        assert_eq!(row["fallback"], "runtime_unavailable");
        assert_eq!(row["reason"], "needs_newer_driver");
        assert_eq!(row["runtime"], "needs_newer_driver");
        assert_eq!(row["fallback_backend"], "tsr");
        let running = VendorFrameReport {
            selected: Some(VendorUpscaler::Xess),
            active: true,
            frames: 4,
            runtime: Some(VendorRuntime::Ready),
            render_range: Some(((610, 305), (910, 455))),
            render_preferred: Some((770, 385)),
            ..VendorFrameReport::default()
        };
        let row = vendor_frame_json(Some(running), Some("tsr"));
        assert_eq!(row["reason"], Value::Null);
        assert_eq!(row["fallback_backend"], Value::Null);
        assert_eq!(row["render_preferred"], json!([770, 385]));
        assert_eq!(row["render_range"]["min"], json!([610, 305]));
        assert_eq!(vendor_frame_json(None, Some("tsr")), Value::Null);
    }

    #[test]
    fn an_available_vendor_requires_a_renderer_without_changing_the_profile() {
        let mut world = world();
        let before = world.resource::<RenderSettings>().profile.clone();
        let mut list = upscaler::available_without_device();
        let dlss = list
            .iter_mut()
            .find(|s| s.kind == UpscalerKind::Vendor(VendorUpscaler::Dlss))
            .expect("dlss");
        dlss.available = true;
        dlss.reason = None;
        dlss.blockers.clear();
        let err = set_with(
            &mut world,
            &json!({ "backend": "dlss", "quality": "performance" }),
            &list,
            Value::Null,
        )
        .expect_err("vendor selection needs RuntimeRenderer");
        let vendor_err = set_vendor(&mut world, &json!({ "vendor": "dlss" })).expect_err("no renderer");
        assert_eq!(err.code, protocol::NOT_FOUND);
        assert_eq!(err.code, vendor_err.code);
        assert_eq!(err.message, vendor_err.message);
        assert_eq!(err.data, vendor_err.data);
        assert_eq!(world.resource::<RenderSettings>().profile, before);
    }

    #[test]
    fn an_available_vendor_is_selected_on_the_renderer_and_a_built_in_clears_it() -> Result<(), RpcError> {
        let mut world = world();
        world.insert_resource(RuntimeRenderer::default());
        let mut list = upscaler::available_without_device();
        for status in &mut list {
            if status.kind == UpscalerKind::Vendor(VendorUpscaler::Dlss) {
                status.available = true;
                status.reason = None;
                status.blockers.clear();
            }
        }
        let selected = set_with(
            &mut world,
            &json!({ "backend": "dlss", "quality": "performance" }),
            &list,
            Value::Null,
        )?;
        assert_eq!(selected["before"]["vendor_frame"]["selected"], Value::Null);
        assert_eq!(selected["after"]["vendor_frame"]["selected"], "dlss");
        assert_eq!(selected["after"]["current"]["quality"], "performance");
        assert_eq!(
            selected["after"]["vendor_frame"],
            get(&mut world)?["vendor_frame"]
        );
        let quality_only = set_with(&mut world, &json!({ "quality": "balanced" }), &list, Value::Null)?;
        assert_eq!(quality_only["before"]["vendor_frame"]["selected"], "dlss");
        assert_eq!(quality_only["after"]["vendor_frame"]["selected"], "dlss");
        assert_eq!(quality_only["after"]["current"]["quality"], "balanced");
        assert_eq!(selected["after"]["current"]["backend"], "taau");
        let report = world.resource::<RuntimeRenderer>().0.vendor_upscaler_report();
        assert_eq!(report.selected, Some(VendorUpscaler::Dlss));
        assert_eq!(
            world.resource::<RenderSettings>().profile.temporal_upscaler,
            TemporalUpscaler::Taau
        );
        let kept = set_with(&mut world, &json!({ "quality": "quality" }), &list, Value::Null)?;
        assert_eq!(kept["after"]["vendor_frame"]["selected"], "dlss");
        let cleared = set_with(&mut world, &json!({ "backend": "tsr" }), &list, Value::Null)?;
        assert_eq!(cleared["before"]["vendor_frame"]["selected"], "dlss");
        assert_eq!(cleared["after"]["vendor_frame"]["selected"], Value::Null);
        assert_eq!(cleared["after"]["vendor_frame"], get(&mut world)?["vendor_frame"]);
        assert_eq!(cleared["after"]["current"]["backend"], "tsr");
        assert_eq!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .vendor_upscaler_report()
                .selected,
            None
        );
        assert_eq!(
            world.resource::<RenderSettings>().profile.temporal_upscaler,
            TemporalUpscaler::Tsr
        );
        Ok(())
    }

    #[test]
    fn the_published_set_upscaler_example_is_accepted() {
        let mut world = world();
        let example = crate::contract::contract("render.set_upscaler")
            .expect("contract")
            .example_json();
        let got = set(&mut world, &example).expect("example");
        assert_eq!(got["after"]["current"]["backend"], "tsr");
    }

    #[test]
    fn upscaler_reads_do_not_return_f0013() {
        let errors = |name: &str| crate::contract::contract(name).expect(name).errors;
        assert!(errors("render.get_upscalers").contains(&"F0003"));
        assert!(!errors("render.get_upscalers").contains(&"F0013"));
        assert!(errors("render.reference_upscale").is_empty());
        assert!(errors("render.upscaler_jitter").is_empty());
        assert!(errors("render.set_upscaler").contains(&"F0013"));
        let err = get(&mut World::new()).expect_err("no settings");
        assert_eq!(data_str(&err, "code"), Some("F0003"));
        assert!(
            get(&mut world()).is_ok(),
            "unavailable vendors are still a successful get"
        );
        let err = reference_upscale(&json!({ "filter": "lanczos" })).expect_err("filter");
        assert_eq!(data_str(&err, "code"), Some("F0002"));
        let err = upscaler_jitter(&json!({ "frames": 0 })).expect_err("frames");
        assert_eq!(data_str(&err, "code"), Some("F0002"));
    }
}

/// Shared capability rows for render.get_capabilities (ADR-4656).
pub(crate) fn capability_rows(world: &World) -> Value {
    let (list, _) = statuses(world);
    json!(list.iter().map(status_json).collect::<Vec<_>>())
}

fn xess_runtime_for_device(runtime: VendorRuntime, prepared: bool) -> VendorRuntime {
    if runtime == VendorRuntime::Ready && !prepared {
        VendorRuntime::ReportsUnsupported
    } else {
        runtime
    }
}

#[cfg(test)]
mod xess_tests {
    use super::*;
    #[test]
    fn another_devices_readiness_cannot_enable_xess_here() {
        assert_eq!(
            xess_runtime_for_device(VendorRuntime::Ready, false),
            VendorRuntime::ReportsUnsupported
        );
        assert_eq!(
            xess_runtime_for_device(VendorRuntime::Ready, true),
            VendorRuntime::Ready
        );
        for state in [
            VendorRuntime::NotBundled,
            VendorRuntime::NotFound,
            VendorRuntime::Found,
            VendorRuntime::ReportsUnsupported,
            VendorRuntime::NeedsNewerDriver,
            VendorRuntime::InitFailed,
        ] {
            assert_eq!(xess_runtime_for_device(state, false), state);
        }
    }
    #[test]
    fn no_renderer_still_exposes_xess_build_and_runtime_metadata() {
        let rows = capability_rows(&World::new());
        let xess = rows
            .as_array()
            .expect("rows")
            .iter()
            .find(|r| r["backend"] == "xess")
            .expect("XeSS row");
        assert_eq!(xess["available"], false);
        assert_eq!(xess["reason"], "no_render_device");
        assert!(xess["runtime"]["binding_built"].is_boolean());
        assert!(xess["runtime"]["sdk_version"].is_string());
    }
}
