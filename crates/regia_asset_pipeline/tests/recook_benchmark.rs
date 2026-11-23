// SPDX-License-Identifier: MPL-2.0
//! Cached recook of 1,000 unchanged assets must finish in < 1 s (specs/12 § 9 M4).
//!
//! The budget is a release-build number: a debug test binary gets three times it, and the best of
//! three recooks is compared, so one preemption on a loaded machine does not fail a correct cache
//! (audit-v035-flaky H3). The cache-hit count is checked on every recook.

use std::time::{Duration, Instant};

use image::ImageEncoder;
use regia_asset_pipeline::{CookOptions, cook_project};

#[test]
fn cached_recook_1000_assets_under_one_second() {
    // Pid + counter + pre-clean: a killed earlier run with a reused pid leaves no warm cache here.
    let root = regia_test_utils::unique_temp_dir("cook_bench");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");

    let png = minimal_png_1x1();
    for i in 0..1000 {
        let path = assets.join(format!("tex_{i:04}.png"));
        std::fs::write(path, &png).expect("write png");
    }

    let options = CookOptions::for_host(root.clone());
    let first = cook_project(options.clone()).expect("first cook");
    assert_eq!(first.errors.len(), 0, "errors: {:?}", first.errors);
    assert_eq!(first.cooked, 1000);

    let mut best = Duration::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let again = cook_project(options.clone()).expect("recook");
        best = best.min(start.elapsed());
        assert_eq!(again.errors.len(), 0);
        assert_eq!(again.skipped, 1000, "expected full cache hit");
    }
    let budget = if cfg!(debug_assertions) { 3.0 } else { 1.0 };
    assert!(
        best.as_secs_f64() < budget,
        "recook took {:.3}s at best (budget {budget}s)",
        best.as_secs_f64()
    );

    let _ = std::fs::remove_dir_all(&root);
}

fn minimal_png_1x1() -> Vec<u8> {
    let pixels = [255u8, 0, 0, 255];
    let mut out = Vec::new();
    let enc = image::codecs::png::PngEncoder::new(&mut out);
    enc.write_image(&pixels, 1, 1, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}
