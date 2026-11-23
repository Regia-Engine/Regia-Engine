// SPDX-License-Identifier: MIT-0
//! Airless sky, TAAU on, camera pan versus the lit face inside the silhouette.
//!
//! v0.1.3's motion sharpen lifts a dim edge against a black sky far past the hull. The check
//! fails that build and passes once the sharpen caps brightening at +25 %.

use lookdev::black_sky;

#[test]
fn airless_pan_keeps_silhouette_gain_within_1_3x() {
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let report = black_sky::run(&device).unwrap_or_else(|err| panic!("black sky pan: {err}"));
    eprintln!("black sky: {} — {}", report.evidence(), report.reason);
    assert!(report.passed, "{}", report.reason);
}
