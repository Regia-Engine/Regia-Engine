// SPDX-License-Identifier: MPL-2.0
//! Golden: every greybox primitive (`regia_render::primitives`, ported from Bevy's meshers) in
//! one row on a floor, sun + sky lit — catches winding, normal, tangent and silhouette regressions.

use regia_render::{RenderDevice, render_lab_scene};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden,
};
use regia_test_utils::scenes::{PRIMITIVES_ROW_HEIGHT, PRIMITIVES_ROW_WIDTH, primitives_row_scene};

const WIDTH: u32 = PRIMITIVES_ROW_WIDTH;
const HEIGHT: u32 = PRIMITIVES_ROW_HEIGHT;

#[test]
fn golden_primitives_row() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_primitives_row: {err}");
            return;
        }
    };
    let frame = render_lab_scene(&device, &primitives_row_scene()).expect("render primitives row");
    assert_eq!(frame.pixels.len(), (WIDTH * HEIGHT * 4) as usize);
    // Keep a copy to look at whatever the compare says.
    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("primitives_row.png");
    if let Some(img) = image::RgbaImage::from_raw(WIDTH, HEIGHT, frame.pixels.clone()) {
        let _ = img.save(&out);
        eprintln!("primitives row written to {}", out.display());
    }
    guard_golden(
        &frame.pixels,
        WIDTH,
        HEIGHT,
        "primitives_row",
        GoldenGuard {
            min_lum: 25.0,
            max_lum: 200.0,
            max_white_pct: 3.0,
            max_black_pct: 40.0,
            min_object_pct: 50.0,
        },
    );
    compare_rgba8_to_png(
        &frame.pixels,
        WIDTH,
        HEIGHT,
        &golden_path("primitives_row"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
