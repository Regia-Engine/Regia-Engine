// SPDX-License-Identifier: MPL-2.0
//! The golden of the radiance cache's ON variant (ADR-2085): the color room with the probes settled and
//! the cache at rest. Off in every preset, so this is the only golden the cache has.
//!
//! Ignored until the lead blesses it on the lavapipe reference:
//! `REGIA_BLESS=1 cargo test -p regia_test_utils --test radiance_cache_golden -- --ignored`,
//! then remove the `ignore` here.

use regia_render::{FrameQuality, FrameRenderer, GiMode, RenderDevice, RenderProfile, RenderProfilePreset};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, color_room};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png_on, golden_path};

#[test]
#[ignore = "needs the lead to bless goldens/radiance_cache_on.png"]
fn radiance_cache_on_golden() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip radiance_cache_on_golden: {err}");
            return;
        }
    };
    let scene = color_room();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.msaa_samples = 1;
    quality.render_scale_permille = 1000;
    quality.horizontal_plus = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.ssr = false;
    quality.reflection_probes = false;
    quality.gi = GiMode::Probes;
    quality.gi_probes = true;
    quality.screen_space_gi = true;
    quality.baked_gi = false;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(scene.ddgi.clone());
    renderer.set_ddgi_enabled(Some(true));
    renderer.set_radiance_cache(true);
    let frame = scene.lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..80 {
        pixels = renderer
            .render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels;
    }
    let status = renderer.radiance_cache_status();
    assert!(status.active && status.idle, "{status:?}");
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &pixels,
        GI_WIDTH,
        GI_HEIGHT,
        &golden_path("radiance_cache_on"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .unwrap_or_else(|err| panic!("radiance_cache_on: {err}"));
}
