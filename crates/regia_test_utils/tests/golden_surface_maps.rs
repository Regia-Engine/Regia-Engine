// SPDX-License-Identifier: MPL-2.0
//! Golden: normal map, occlusion and emissive (`specs/13` § 6).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_surface_maps};

#[test]
fn golden_surface_maps() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_surface_maps: {err}");
            return;
        }
    };
    let pixels = render_surface_maps(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    // The mapped plane fills the frame; the wave normal map and the occlusion
    // gradient must keep the shading visibly non-flat.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "surface_maps",
        GoldenGuard {
            min_lum: 40.0,
            max_lum: 210.0,
            max_white_pct: 3.0,
            max_black_pct: 25.0,
            min_object_pct: 75.0,
        },
    );
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("surface_maps"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
