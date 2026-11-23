// SPDX-License-Identifier: MPL-2.0
//! Golden for the ON variant of the shadow casters' two-phase cull (ADR-2072): the light-shadow
//! scene on the GPU-driven path (the lab baseline draws shadows on the CPU path, which the cull
//! never takes), with the cull on, at the second frame (phase 1 tests the first frame's pyramid).
//!
//! The non-ignored test needs no reference image: the culled frames must equal the unculled
//! frames byte for byte. The golden itself is ignored until the lead blesses it on the lavapipe
//! reference:
//!
//! ```text
//! REGIA_BLESS=1 cargo test -p regia_test_utils --test golden_shadow_occlusion -- --ignored
//! ```

use regia_render::{FrameQuality, FrameRenderer, IndirectStrategy, RenderDevice};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, light_shadows_scene};

/// Two frames of the light-shadow scene on the GPU-driven path; the second frame's pixels.
fn render(device: &RenderDevice, culled: bool) -> (Vec<u8>, FrameRenderer) {
    let lab = light_shadows_scene();
    let quality = FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_shadow_occlusion(culled);
    renderer.set_shadow_occlusion_min_casters(Some(1));
    renderer.render_offscreen(device, &frame).expect("first frame");
    let pixels = renderer
        .render_offscreen(device, &frame)
        .expect("second frame")
        .pixels;
    (pixels, renderer)
}

fn device() -> Option<RenderDevice> {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip golden_shadow_occlusion: {err}");
            return None;
        }
    };
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip golden_shadow_occlusion: no GPU-driven shadow path on this device");
        return None;
    }
    Some(device)
}

#[test]
fn culled_light_shadows_equal_the_unculled_frame() {
    let Some(device) = device() else {
        return;
    };
    let (culled, renderer) = render(&device, true);
    let stats = renderer.shadow_occlusion_stats();
    assert!(stats.active, "{stats:?}");
    assert!(
        stats.kinds.iter().any(|k| k.active && k.history_jobs > 0),
        "the second frame tested last frame's pyramid: {stats:?}"
    );
    let (plain, _) = render(&device, false);
    let differ = culled
        .chunks(4)
        .zip(plain.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differ, 0, "the shadow cull changed {differ} pixels");
}

#[test]
#[ignore = "the lead blesses light_shadows_culled on the lavapipe reference first"]
fn golden_light_shadows_culled() {
    let Some(device) = device() else {
        return;
    };
    let (pixels, _) = render(&device, true);
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("light_shadows_culled"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
