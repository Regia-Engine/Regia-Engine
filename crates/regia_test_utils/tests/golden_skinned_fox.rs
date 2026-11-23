// SPDX-License-Identifier: MPL-2.0
//! Golden: skinned Fox in a stored pose (`specs/13` § 13).
//!
//! Loads Fox from the Khronos glTF sample models, samples its "Walk" clip so the
//! skeleton sits in a stored non-bind pose, and renders it framed to fill a
//! good part of the frame.

use regia_render::{RenderDevice, render_lab_scene};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, golden_path, guard_golden,
};
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};

#[test]
fn golden_skinned_fox() {
    let path = match ensure_model("Fox") {
        Ok(p) => p,
        Err(err) => {
            eprintln!("skip golden_skinned_fox: {err}");
            return;
        }
    };
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_skinned_fox: {err}");
            return;
        }
    };
    let mut scene = match load_gltf_model_with_view(&path, LabView::ModelPortrait, Some(("Walk", 0.5))) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("skip golden_skinned_fox: load failed: {err}");
            return;
        }
    };
    scene.width = 512;
    scene.height = 512;

    let result = render_lab_scene(&device, &scene).expect("render Fox");
    // The Fox must fill at least a fifth of the frame and be well lit; the
    // background is the clear colour, so near-black is allowed there.
    guard_golden(
        &result.pixels,
        result.width,
        result.height,
        "skinned_fox",
        GoldenGuard {
            min_lum: 25.0,
            max_lum: 190.0,
            max_white_pct: 3.5,
            max_black_pct: 75.0,
            min_object_pct: 22.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &result.pixels,
        result.width,
        result.height,
        &golden_path("skinned_fox"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
