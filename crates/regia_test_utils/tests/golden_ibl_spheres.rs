// SPDX-License-Identifier: MPL-2.0
//! Golden: IBL-only spheres (`specs/13` § 8).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden, ssim_threshold_for,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_ibl_spheres};

#[test]
fn golden_ibl_spheres() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_ibl_spheres: {err}");
            return;
        }
    };
    // Hardware GPUs filter the (CPU-built, byte-identical) prefiltered mip chain differently
    // from the lavapipe reference (ADR-025): documented hardware threshold for this class.
    let hardware_gpu = device.adapter.get_info().device_type != wgpu::DeviceType::Cpu;
    let pixels = render_ibl_spheres(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    // Two sky-lit spheres against the dark background: both must be visible.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "ibl_spheres",
        GoldenGuard {
            min_lum: 20.0,
            max_lum: 150.0,
            max_white_pct: 3.0,
            max_black_pct: 65.0,
            min_object_pct: 35.0,
        },
    );
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("ibl_spheres"),
        ssim_threshold_for("ibl_spheres", hardware_gpu),
    )
    .expect("golden compare");
}
