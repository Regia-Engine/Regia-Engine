// SPDX-License-Identifier: MPL-2.0
//! Golden: opaque, mask and sorted blend (`specs/13` § 6).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden, ssim_threshold_for,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_alpha_modes};

#[test]
fn golden_alpha_modes() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_alpha_modes: {err}");
            return;
        }
    };
    // Hardware GPUs rasterize the alpha-test edge coverage differently from the lavapipe
    // reference (ADR-025): this scene class compares with the documented hardware threshold.
    let hardware_gpu = device.adapter.get_info().device_type != wgpu::DeviceType::Cpu;
    let pixels = render_alpha_modes(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    // Unlit quads: the mask quad's checkerboard is intentionally pure white in
    // its on-cells, so the near-white band is looser than for lit scenes.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "alpha_modes",
        GoldenGuard {
            min_lum: 30.0,
            max_lum: 160.0,
            max_white_pct: 20.0,
            max_black_pct: 60.0,
            min_object_pct: 40.0,
        },
    );
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("alpha_modes"),
        ssim_threshold_for("alpha_modes", hardware_gpu),
    )
    .expect("golden compare");
}
