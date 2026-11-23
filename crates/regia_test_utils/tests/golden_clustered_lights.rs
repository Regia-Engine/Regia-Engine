// SPDX-License-Identifier: MPL-2.0
//! Golden: clustered point and spot lights (`specs/13` § 5).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_clustered_lights};

#[test]
fn golden_clustered_lights() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_clustered_lights: {err}");
            return;
        }
    };
    let pixels = render_clustered_lights(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    // Two spheres in a dark room, each lit by one coloured local light: the
    // pools of light must read clearly but the room stays dark.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "clustered_lights",
        GoldenGuard {
            min_lum: 22.0,
            max_lum: 170.0,
            max_white_pct: 3.0,
            max_black_pct: 55.0,
            min_object_pct: 45.0,
        },
    );
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("clustered_lights"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
