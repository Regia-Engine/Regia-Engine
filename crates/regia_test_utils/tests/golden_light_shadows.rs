// SPDX-License-Identifier: MPL-2.0
//! Golden: directional, point and spot shadows (`specs/13` § 13).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_light_shadows};

#[test]
fn golden_light_shadows() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_light_shadows: {err}");
            return;
        }
    };
    let pixels = render_light_shadows(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    // A ~100 klx sun through EV100 exposure: the lit floor must read
    // mid-bright, not flat white, with the sphere and local-light shadows
    // clearly darker.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "light_shadows",
        GoldenGuard {
            min_lum: 40.0,
            max_lum: 200.0,
            max_white_pct: 3.0,
            max_black_pct: 25.0,
            min_object_pct: 75.0,
        },
    );
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("light_shadows"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
