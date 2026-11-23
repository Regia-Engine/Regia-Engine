// SPDX-License-Identifier: MPL-2.0
//! Golden for the on variant of spatiotemporal blue noise (ADR-2075): the light-shadow scene on
//! Desktop High's screen-space passes with the table.
//!
//! Ignored until the lead blesses the image on the lavapipe reference: the gate has no reference for
//! it yet. Every other golden draws with the knob off.
//!
//! ```text
//! REGIA_BLESS=1 cargo test -p regia_test_utils --test golden_blue_noise -- --ignored
//! ```

use regia_render::{FrameQuality, FrameRenderer, RenderDevice, RenderProfile, RenderProfilePreset};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, light_shadows_scene};

#[test]
#[ignore = "the lead blesses this image first (book/agents/blue-noise.md)"]
fn golden_blue_noise_on() {
    let device = RenderDevice::headless().expect("device");
    let lab = light_shadows_scene();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.volumetric_clouds = false;
    quality.volumetric_fog = false;
    quality.blue_noise = true;
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    }
    assert!(renderer.blue_noise_active());
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("blue_noise_on"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
