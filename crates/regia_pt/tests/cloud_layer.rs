// SPDX-License-Identifier: MPL-2.0
//! The path-traced cloud layer (ADR-1834): the tracer's slab against Beer–Lambert and the
//! two-stream limit, and the real-time cloud model's ground light against the tracer's.
//! Needs a GPU adapter (lavapipe is enough).

use std::sync::OnceLock;

use regia_pt::PathTracer;
use regia_pt::cloud::{
    CloudLight, PtCloudLayer, VALIDATION_GROUND_ALBEDO, VALIDATION_SKY_NIT, VALIDATION_SUN_LUX,
    measure_ground_ratio, renderer_ground_ratio, slab_transport_reference,
};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn traced(tau: f32, sun_y: f32, light: CloudLight) -> f64 {
    let layer = PtCloudLayer::overcast(1500.0, 4000.0, tau);
    measure_ground_ratio(tracer(), &layer, sun_y, light, 256, 5)
        .expect("trace")
        .ratio()
}

#[test]
fn a_thin_layer_leaves_the_traced_ground_alone() {
    for light in [CloudLight::SunOnly, CloudLight::SkyOnly, CloudLight::Both] {
        let r = traced(0.02, 0.6, light);
        assert!((r - 1.0).abs() < 0.08, "{light:?}: {r}");
    }
}

#[test]
fn the_traced_layer_agrees_with_the_transport_reference() {
    // The tracer's slab (free-flight sampling, next-event estimation through the layer, lambert
    // ground) against the independent 1D Monte Carlo reference, to the noise of a 64-pixel trace.
    for tau in [0.5_f32, 2.0, 5.0, 10.0] {
        for sun_y in [0.3_f32, 0.8] {
            let layer = PtCloudLayer::overcast(1500.0, 4000.0, tau);
            let sun_pt = traced(tau, sun_y, CloudLight::SunOnly);
            let sun_ref = slab_transport_reference(&layer, Some(sun_y), VALIDATION_GROUND_ALBEDO, 40_000, 1);
            assert!(
                (sun_pt - sun_ref).abs() < 0.12,
                "sun tau {tau} sun_y {sun_y}: pt {sun_pt}, reference {sun_ref}"
            );
        }
        let layer = PtCloudLayer::overcast(1500.0, 4000.0, tau);
        let sky_pt = traced(tau, 0.5, CloudLight::SkyOnly);
        let sky_ref = slab_transport_reference(&layer, None, VALIDATION_GROUND_ALBEDO, 40_000, 2);
        assert!(
            (sky_pt - sky_ref).abs() < 0.12,
            "sky tau {tau}: pt {sky_pt}, reference {sky_ref}"
        );
    }
}

#[test]
fn the_traced_ground_light_falls_with_optical_depth_and_is_never_brighter_than_clear() {
    let mut last = f64::MAX;
    for tau in [0.5_f32, 2.0, 6.0, 12.0] {
        let r = traced(tau, 0.5, CloudLight::Both);
        assert!(r < 1.1, "tau {tau}: {r}");
        assert!(r < last + 0.08, "tau {tau}: {r} after {last}");
        last = r;
    }
}

#[test]
fn the_renderers_model_is_darker_than_the_trace_under_a_moderate_layer() {
    // ADR-1834: at tau 2 to 10 the real-time model (Beer sun term, 0.6 sky dim) lights the
    // ground several stops less than the trace does.
    for tau in [2.0_f32, 5.0] {
        let layer = PtCloudLayer::overcast(1500.0, 4000.0, tau);
        let sky_lux = std::f32::consts::PI * VALIDATION_SKY_NIT;
        let renderer = f64::from(renderer_ground_ratio(&layer, 0.8, VALIDATION_SUN_LUX, sky_lux));
        let pt = traced(tau, 0.8, CloudLight::Both);
        assert!(pt > 2.0 * renderer, "tau {tau}: trace {pt}, renderer {renderer}");
    }
}
