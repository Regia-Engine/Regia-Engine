// SPDX-License-Identifier: MPL-2.0
//! Golden: post stack on/off (`specs/13` § 13).

use regia_render::RenderDevice;
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden,
};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, render_post_stack};

#[test]
fn golden_post_stack() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_post_stack: {err}");
            return;
        }
    };
    let pixels = render_post_stack(&device).expect("render scene");
    assert_eq!(pixels.len(), (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize);
    let half = (GOLDEN_WIDTH / 2) as usize;
    let mut left = 0u64;
    let mut right = 0u64;
    for y in 0..GOLDEN_HEIGHT as usize {
        for x in 0..GOLDEN_WIDTH as usize {
            let i = (y * GOLDEN_WIDTH as usize + x) * 4;
            let luma = pixels[i] as u64 + pixels[i + 1] as u64 + pixels[i + 2] as u64;
            if x < half {
                left += luma;
            } else {
                right += luma;
            }
        }
    }
    assert!(left != right, "post on/off halves must differ");
    // The bloom side has an intentional white core around the lamp; the
    // exposure-matched off side keeps the whole image in a sane band.
    guard_golden(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        "post_stack",
        GoldenGuard {
            min_lum: 12.0,
            max_lum: 150.0,
            max_white_pct: 12.0,
            max_black_pct: 85.0,
            min_object_pct: 15.0,
        },
    );
    compare_rgba8_to_png(
        &pixels,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("post_stack"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
