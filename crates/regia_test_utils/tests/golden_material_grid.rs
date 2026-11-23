// SPDX-License-Identifier: MPL-2.0
//! Golden: metal × roughness sphere grid (`specs/13` § 13).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, golden_path, guard_golden,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_material_sphere_grid};

#[test]
fn golden_material_sphere_grid() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_material_sphere_grid: {err}");
            return;
        }
    };
    let pixels = render_material_sphere_grid(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    // A 5×5 sun-lit sphere grid must dominate the frame and read mid-bright.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "material_sphere_grid",
        GoldenGuard {
            min_lum: 30.0,
            max_lum: 200.0,
            max_white_pct: 3.0,
            max_black_pct: 65.0,
            min_object_pct: 35.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("material_sphere_grid"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
