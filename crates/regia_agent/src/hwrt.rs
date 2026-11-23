// SPDX-License-Identifier: MPL-2.0
//! `render.hwrt`: the hardware ray tracing state of the render device (`book/agents/hwrt.md`,
//! ADR-515, ADR-1149). What the adapter offers (ray queries, vertex return, acceleration-structure
//! limits), whether the device request (`hwrt::set_device_request`) asked for it, whether the
//! device got it, and which shadow-ray tracer that leaves: `ray_query` or the `compute_fallback`.
//! `sun_shadows` is the sun-shadow variant chosen from `RenderProfile::sun_shadow_trace` (the
//! environment variables were removed in v0.2.11) with the shader defs it compiles. With `adapters: true` it also probes every adapter in the machine. Read-only.

use regia_ecs::prelude::*;
use regia_render::hwrt::{
    AdapterRayTracing, HwrtReport, HwrtRequest, RtShadowRequest, ShadowRayPath, SunShadowSelection,
    device_request, last_device_report, probe_adapters, select_sun_shadow_variant,
};
use regia_render::{RenderSettings, RuntimeRenderer, SunTraceReport};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `render.hwrt`: the last created render device's ray-tracing report
/// (`regia_render::hwrt::last_device_report`), the sun-shadow variant for the active preset,
/// and with `adapters: true` every adapter's offer.
pub(crate) fn render_hwrt(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let with_adapters = match params.get("adapters").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`adapters` must be a boolean"))?,
    };
    let report = last_device_report();
    let settings = world.get_resource::<RenderSettings>();
    let preset = settings.and_then(RenderSettings::preset);
    let request = settings.map_or(RtShadowRequest::Off, |s| s.profile.sun_shadow_trace);
    let selection = select_sun_shadow_variant(request, report.as_ref(), preset);
    let frame = world
        .get_resource::<RuntimeRenderer>()
        .map(|renderer| (renderer.0.sun_trace_report(), renderer.0.sun_trace_applied()));
    let mut out = report_json(report, device_request());
    out["sun_shadows"] = sun_shadows_json(&selection, frame);
    if with_adapters {
        out["adapters"] = adapters_json(&probe_adapters());
    }
    Ok(out)
}

fn report_json(report: Option<HwrtReport>, device_request: HwrtRequest) -> Value {
    let Some(r) = report else {
        return json!({
            "probed": false,
            "requested": device_request.as_str(),
            "adapter": Value::Null,
            "device_ray_query": false,
            "shadow_path": ShadowRayPath::ComputeFallback.as_str(),
            "reason": Value::Null,
            "message": "no render device has been created in this process yet",
        });
    };
    let limits = r.probe.limits;
    json!({
        "probed": true,
        "requested": r.request.as_str(),
        "adapter": {
            "backend": r.probe.backend.to_str(),
            "ray_query": r.probe.ray_query,
            "vertex_return": r.probe.vertex_return,
            "limits": {
                "max_blas_primitive_count": limits.max_blas_primitive_count,
                "max_blas_geometry_count": limits.max_blas_geometry_count,
                "max_tlas_instance_count": limits.max_tlas_instance_count,
                "max_acceleration_structures_per_shader_stage":
                    limits.max_acceleration_structures_per_shader_stage,
            },
        },
        "device_ray_query": r.device_ray_query,
        "shadow_path": r.shadow_path().as_str(),
        "reason": r.reason().code(),
        "message": r.reason().message(),
    })
}

/// `selection` is what the profile and the device give; once a frame has drawn this request, what
/// that frame chose (it also knows the frame's depth, sun and view) replaces it.
fn sun_shadows_json(selection: &SunShadowSelection, frame: Option<(SunTraceReport, bool)>) -> Value {
    let drawn = frame.filter(|(report, _)| {
        report.selection.request == selection.request && selection.request != RtShadowRequest::Off
    });
    let shown = drawn.map_or(*selection, |(report, _)| SunShadowSelection {
        preset: selection.preset,
        ..report.selection
    });
    json!({
        "requested": shown.request.as_str(),
        "preset": shown.preset.map(|p| p.name()),
        "variant": shown.variant.as_str(),
        "reason": shown.reason.code(),
        "message": shown.reason.message(),
        "forward_defs": shown.variant.forward_def_names(),
        "trace_shader": shown.variant.trace_shader().map(|t| t.file),
        "pipeline_key_bits": shown.variant.key_bits(),
        "applied": drawn.is_some_and(|(_, applied)| applied),
        "frame": drawn.map(|(report, _)| json!({
            "traces": report.traces,
            "scene_builds": report.scene_builds,
            "casters": report.casters,
            "triangles": report.triangles,
            "skipped_deforming": report.skipped_deforming,
            "skipped_masked": report.skipped_masked,
            "shared_casters": report.shared_casters,
            "set_casters": report.set_casters,
            "masked_casters": report.masked_casters,
            "tlas_refits": report.tlas_refits,
            "instances": report.accel.instances,
            "refit": report.accel.refit,
            "instances_moved": report.accel.instances_moved,
            "sun_maps_skipped": report.sun_maps_skipped,
            "blas_built": report.accel.blas_built,
            "blas_resident": report.accel.blas_resident,
            "failure": report.failure,
        })),
    })
}

/// `render.set_sun_shadow_trace`: switches the active profile's `sun_shadow_trace`. Returns the
/// request before and after.
pub(crate) fn set_sun_shadow_trace(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(name) = crate::params::opt_str(params, "mode")? else {
        return Err(RpcError::invalid_params(
            "`mode` must be one of off, on, compute, screen",
        ));
    };
    let Some(request) = RtShadowRequest::from_name(name) else {
        return Err(RpcError::invalid_params(format!(
            "unknown mode `{name}`: pass off, on, compute or screen"
        )));
    };
    let Some(mut settings) = world.get_resource_mut::<RenderSettings>() else {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             FrameQuality::sun_shadow_trace)",
        ));
    };
    let before = settings.profile.sun_shadow_trace;
    if before != request {
        settings.profile.sun_shadow_trace = request;
    }
    Ok(json!({
        "before": before.as_str(),
        "after": request.as_str(),
        "note": "applies from the next frame; read render.hwrt sun_shadows. `on` needs a device created with \
                 the knob already on (the first window's); without one the frame takes the screen-space \
                 trace (rt_shadows_screen_fallback). A preset change (render.set_profile) replaces the \
                 profile and turns it off",
    }))
}

fn adapters_json(adapters: &[AdapterRayTracing]) -> Value {
    Value::Array(
        adapters
            .iter()
            .map(|a| {
                let limits = a.probe.limits;
                let reason = a.decide(HwrtRequest::On).reason;
                json!({
                    "index": a.index,
                    "name": a.name,
                    "device_type": a.device_type_str(),
                    "backend": a.probe.backend.to_str(),
                    "driver": a.driver,
                    "driver_info": a.driver_info,
                    "ray_query": a.probe.ray_query,
                    "vertex_return": a.probe.vertex_return,
                    "limits": {
                        "max_blas_primitive_count": limits.max_blas_primitive_count,
                        "max_blas_geometry_count": limits.max_blas_geometry_count,
                        "max_tlas_instance_count": limits.max_tlas_instance_count,
                        "max_acceleration_structures_per_shader_stage":
                            limits.max_acceleration_structures_per_shader_stage,
                    },
                    "if_requested": reason.code(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;
    use regia_render::hwrt::{AccelLimits, RayTracingProbe, SUN_SHADOW_RT_DEF, SunShadowVariant};

    fn probe(ray_query: bool) -> RayTracingProbe {
        RayTracingProbe {
            backend: wgpu::Backend::Vulkan,
            ray_query,
            vertex_return: false,
            limits: AccelLimits {
                max_blas_primitive_count: 1 << 20,
                max_blas_geometry_count: 64,
                max_tlas_instance_count: 1 << 16,
                max_acceleration_structures_per_shader_stage: 8,
            },
        }
    }

    #[test]
    fn before_any_device_the_report_says_so() {
        let v = report_json(None, HwrtRequest::On);
        assert_eq!(v["probed"], json!(false));
        assert_eq!(v["requested"], json!("on"));
        assert_eq!(v["shadow_path"], json!("compute_fallback"));
        assert!(v["reason"].is_null());
    }

    #[test]
    fn an_enabled_device_traces_with_ray_queries() {
        let p = probe(true);
        let d = p.decide(HwrtRequest::On, false);
        let rq = wgpu::Features::EXPERIMENTAL_RAY_QUERY;
        let v = report_json(Some(HwrtReport::new(HwrtRequest::On, p, d, rq)), HwrtRequest::Off);
        assert_eq!(v["probed"], json!(true));
        assert_eq!(
            v["requested"],
            json!("on"),
            "the device's request, not the process's current one"
        );
        assert_eq!(v["adapter"]["backend"], json!("vulkan"));
        assert_eq!(v["adapter"]["limits"]["max_tlas_instance_count"], json!(1 << 16));
        assert_eq!(v["device_ray_query"], json!(true));
        assert_eq!(v["shadow_path"], json!("ray_query"));
        assert_eq!(v["reason"], json!("hwrt_enabled"));
    }

    #[test]
    fn a_refusal_names_its_reason_and_the_fallback() {
        let p = probe(false);
        let d = p.decide(HwrtRequest::On, false);
        let v = report_json(
            Some(HwrtReport::new(HwrtRequest::On, p, d, wgpu::Features::empty())),
            HwrtRequest::On,
        );
        assert_eq!(v["shadow_path"], json!("compute_fallback"));
        assert_eq!(v["reason"], json!("hwrt_no_adapter_ray_query"));
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
    }

    #[test]
    fn sun_shadows_name_the_variant_its_defs_and_trace_shader() {
        let p = probe(true);
        let report = HwrtReport::new(
            HwrtRequest::On,
            p,
            p.decide(HwrtRequest::On, false),
            wgpu::Features::EXPERIMENTAL_RAY_QUERY,
        );
        let s = select_sun_shadow_variant(
            RtShadowRequest::On,
            Some(&report),
            Some(RenderProfilePreset::DesktopUltra),
        );
        let v = sun_shadows_json(&s, None);
        assert_eq!(v["requested"], json!("on"));
        assert_eq!(v["preset"], json!("DesktopUltra"));
        assert_eq!(v["variant"], json!("ray_query"));
        assert_eq!(v["reason"], json!("rt_shadows_ray_query"));
        assert_eq!(v["forward_defs"], json!([SUN_SHADOW_RT_DEF]));
        assert_eq!(v["trace_shader"], json!("sun_trace_ray_query.wgsl"));
        assert_eq!(
            v["pipeline_key_bits"],
            json!(SunShadowVariant::RayQuery.key_bits())
        );
        assert_eq!(v["applied"], json!(false), "no frame has drawn it");
        assert!(v["frame"].is_null());

        let p = probe(false);
        let refused = HwrtReport::new(
            HwrtRequest::On,
            p,
            p.decide(HwrtRequest::On, false),
            wgpu::Features::empty(),
        );
        let s = select_sun_shadow_variant(RtShadowRequest::On, Some(&refused), None);
        let v = sun_shadows_json(&s, None);
        assert_eq!(v["variant"], json!("screen_trace"));
        assert_eq!(v["reason"], json!("rt_shadows_screen_fallback"));
        assert_eq!(v["forward_defs"], json!([]));
        assert_eq!(v["trace_shader"], json!("sun_trace_screen.wgsl"));
        assert_eq!(v["pipeline_key_bits"], json!(3));

        let s = select_sun_shadow_variant(RtShadowRequest::Compute, None, None);
        let v = sun_shadows_json(&s, None);
        assert!(v["preset"].is_null());
        assert_eq!(v["variant"], json!("compute_fallback"));
        assert_eq!(v["trace_shader"], json!("sun_trace_bvh.wgsl"));
    }

    /// The existing count describes coverage-tested casters on both traced paths (ADR-4557).
    #[test]
    fn masked_caster_counts_are_exposed_for_both_traced_paths() {
        let p = probe(true);
        let device = HwrtReport::new(
            HwrtRequest::On,
            p,
            p.decide(HwrtRequest::On, false),
            wgpu::Features::EXPERIMENTAL_RAY_QUERY,
        );
        for (request, variant) in [
            (RtShadowRequest::Compute, SunShadowVariant::ComputeFallback),
            (RtShadowRequest::On, SunShadowVariant::RayQuery),
        ] {
            let selection = select_sun_shadow_variant(request, Some(&device), None);
            assert_eq!(selection.variant, variant);
            let frame = SunTraceReport {
                selection,
                casters: 5,
                masked_casters: 2,
                skipped_masked: 0,
                ..SunTraceReport::default()
            };
            let report = sun_shadows_json(&selection, Some((frame, true)));
            assert_eq!(report["variant"], variant.as_str());
            assert_eq!(report["frame"]["masked_casters"], 2);
            assert_eq!(report["frame"]["skipped_masked"], 0);
        }
    }

    /// Once a frame drew the request, its choice and counters show; an off profile shows none of
    /// what a renderer did before.
    #[test]
    fn the_frame_replaces_the_selection_once_it_has_drawn() {
        let asked = select_sun_shadow_variant(RtShadowRequest::Compute, None, None);
        let frame = SunTraceReport {
            selection: SunShadowSelection {
                reason: regia_render::hwrt::SunShadowReason::NoDepth,
                variant: SunShadowVariant::ShadowMap,
                ..asked
            },
            traces: 7,
            casters: 3,
            shared_casters: 16,
            set_casters: 2,
            masked_casters: 1,
            tlas_refits: 99,
            sun_maps_skipped: 5,
            ..SunTraceReport::default()
        };
        let v = sun_shadows_json(&asked, Some((frame, false)));
        assert_eq!(v["variant"], json!("shadow_map"));
        assert_eq!(v["reason"], json!("rt_shadows_no_depth"));
        assert_eq!(v["applied"], json!(false));
        assert_eq!(v["frame"]["traces"], json!(7));
        assert_eq!(v["frame"]["casters"], json!(3));
        // The ADR-2310 counters: shared and set casters, the masked proxy, refits, skipped maps.
        assert_eq!(v["frame"]["shared_casters"], json!(16));
        assert_eq!(v["frame"]["set_casters"], json!(2));
        assert_eq!(v["frame"]["masked_casters"], json!(1));
        assert_eq!(v["frame"]["tlas_refits"], json!(99));
        assert_eq!(v["frame"]["sun_maps_skipped"], json!(5));
        assert_eq!(v["frame"]["refit"], json!(false));
        assert_eq!(v["frame"]["instances"], json!(0));

        let off = select_sun_shadow_variant(RtShadowRequest::Off, None, None);
        let v = sun_shadows_json(&off, Some((frame, true)));
        assert_eq!(v["reason"], json!("rt_shadows_not_requested"));
        assert_eq!(v["applied"], json!(false));
        assert!(v["frame"].is_null());
    }

    #[test]
    fn the_switch_sets_the_profile_and_refuses_anything_else() {
        let mut world = World::new();
        assert!(
            set_sun_shadow_trace(&mut world, &json!({ "mode": "on" })).is_err(),
            "no RenderSettings"
        );
        world.insert_resource(RenderSettings::default());
        assert_eq!(
            world.resource::<RenderSettings>().profile.sun_shadow_trace,
            RtShadowRequest::Off
        );
        let v = set_sun_shadow_trace(&mut world, &json!({ "mode": "screen" })).expect("switch");
        assert_eq!(v["before"], json!("off"));
        assert_eq!(v["after"], json!("screen"));
        assert_eq!(
            world.resource::<RenderSettings>().profile.sun_shadow_trace,
            RtShadowRequest::Screen
        );
        for bad in [json!({}), json!({ "mode": 1 }), json!({ "mode": "ray_query" })] {
            let err = set_sun_shadow_trace(&mut world, &bad).expect_err("invalid");
            assert!(format!("{err:?}").contains("mode"), "{bad}");
        }
        assert_eq!(
            world.resource::<RenderSettings>().profile.sun_shadow_trace,
            RtShadowRequest::Screen,
            "a refused call changes nothing"
        );
        let v = render_hwrt(&mut world, &json!({})).expect("report");
        assert_eq!(v["sun_shadows"]["requested"], json!("screen"));
        assert_eq!(v["sun_shadows"]["variant"], json!("screen_trace"));
    }

    #[test]
    fn adapters_list_each_offer_and_what_a_request_would_get() {
        let entry = |index, ray_query| AdapterRayTracing {
            index,
            name: format!("gpu {index}"),
            device_type: wgpu::DeviceType::DiscreteGpu,
            driver: "drv".into(),
            driver_info: "1.0".into(),
            probe: probe(ray_query),
        };
        let v = adapters_json(&[entry(0, true), entry(1, false)]);
        let list = v.as_array().expect("array");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["index"], json!(0));
        assert_eq!(list[0]["device_type"], json!("discrete"));
        assert_eq!(list[0]["backend"], json!("vulkan"));
        assert_eq!(list[0]["if_requested"], json!("hwrt_enabled"));
        assert_eq!(list[1]["ray_query"], json!(false));
        assert_eq!(list[1]["if_requested"], json!("hwrt_no_adapter_ray_query"));
        assert_eq!(adapters_json(&[]), json!([]));
    }

    #[test]
    fn the_method_answers_without_a_renderer() {
        let mut world = World::new();
        let v = render_hwrt(&mut world, &json!({})).expect("render.hwrt never fails");
        assert!(v["probed"].is_boolean());
        assert!(v["shadow_path"].is_string());
        assert!(v["sun_shadows"]["variant"].is_string());
        assert!(v["sun_shadows"]["preset"].is_null(), "no RenderSettings resource");
        assert!(v.get("adapters").is_none(), "adapters are probed only when asked");
    }

    #[test]
    fn a_non_boolean_adapters_flag_is_invalid() {
        let mut world = World::new();
        let err = render_hwrt(&mut world, &json!({ "adapters": "yes" })).expect_err("invalid");
        assert!(format!("{err:?}").contains("adapters"));
    }
}
