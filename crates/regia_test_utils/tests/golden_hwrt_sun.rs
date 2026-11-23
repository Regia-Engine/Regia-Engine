// SPDX-License-Identifier: MPL-2.0
//! Golden pair (ADR-2035): the light-shadow scene with the sun traced by hardware ray queries, and
//! the same scene with the shadow map, drawn on the same device.
//!
//! Ignored until the lead blesses the two images on the lavapipe reference (lavapipe has
//! `VK_KHR_ray_query` since Mesa 24.1): the gate has no reference for them yet, and it needs a
//! device that asked for ray queries. `REGIA_HWRT_EVIDENCE_DIR` names a directory to leave both
//! frames in, for a look by eye.
//!
//! ```text
//! REGIA_BLESS=1 cargo test -p regia_test_utils --test golden_hwrt_sun -- --ignored
//! ```

use regia_render::hwrt::{HwrtReason, RtShadowRequest, SunShadowVariant, last_device_report};
use regia_render::{FrameQuality, FrameRenderer, RenderDevice};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, light_shadows_scene};

fn render(device: &RenderDevice, request: RtShadowRequest) -> (Vec<u8>, SunShadowVariant) {
    let lab = light_shadows_scene();
    let mut quality = FrameQuality::LAB;
    quality.sun_shadow_trace = request;
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    (pixels, renderer.sun_trace_report().selection.variant)
}

fn leave_evidence(name: &str, pixels: &[u8]) {
    if let Ok(dir) = std::env::var("REGIA_HWRT_EVIDENCE_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        image::RgbaImage::from_raw(GOLDEN_WIDTH, GOLDEN_HEIGHT, pixels.to_vec())
            .expect("frame size")
            .save(&path)
            .expect("write evidence");
    }
}

#[test]
#[ignore = "the lead blesses this pair first, on a device with ray queries (book/agents/hwrt.md)"]
fn golden_hwrt_sun_pair() {
    let device = RenderDevice::headless_with_ray_queries().expect("device");
    let report = last_device_report().expect("report");
    assert_eq!(
        report.reason(),
        HwrtReason::Enabled,
        "run on an adapter with ray queries: {}",
        report.reason().message()
    );
    let (traced, variant) = render(&device, RtShadowRequest::On);
    assert_eq!(variant, SunShadowVariant::RayQuery);
    let (mapped, variant) = render(&device, RtShadowRequest::Off);
    assert_eq!(variant, SunShadowVariant::ShadowMap);
    assert_ne!(traced, mapped, "the pair must show the two variants");
    leave_evidence("hwrt_sun_ray_query", &traced);
    leave_evidence("hwrt_sun_shadow_map", &mapped);
    for (name, pixels) in [("hwrt_sun_ray_query", &traced), ("hwrt_sun_shadow_map", &mapped)] {
        compare_rgba8_to_png(
            pixels,
            GOLDEN_WIDTH,
            GOLDEN_HEIGHT,
            &golden_path(name),
            DEFAULT_SSIM_THRESHOLD,
        )
        .expect("golden compare");
    }
}

/// The frames for a look by eye: every variant, compared with nothing.
#[test]
#[ignore = "only leaves the pair in REGIA_HWRT_EVIDENCE_DIR"]
fn hwrt_sun_pair_evidence() {
    let device = RenderDevice::headless_with_ray_queries().expect("device");
    println!("adapter: {}", device.adapter.get_info().name);
    let (traced, _) = render(&device, RtShadowRequest::On);
    let (mapped, _) = render(&device, RtShadowRequest::Off);
    let (screen, _) = render(&device, RtShadowRequest::Screen);
    let (bvh, _) = render(&device, RtShadowRequest::Compute);
    leave_evidence("hwrt_sun_ray_query", &traced);
    leave_evidence("hwrt_sun_shadow_map", &mapped);
    leave_evidence("hwrt_sun_screen_trace", &screen);
    leave_evidence("hwrt_sun_compute", &bvh);
}
