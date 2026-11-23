// SPDX-License-Identifier: MIT-0
//! One frame at the owner's 3840×1600 play resolution.

use lookdev::do27_bench::ULTRAWIDE;
use lookdev::ultrawide;

#[test]
fn a_frame_at_the_owner_resolution_is_not_flat() {
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let report = ultrawide::run(&device).unwrap_or_else(|err| panic!("ultrawide frame: {err}"));
    eprintln!("ultrawide: {} — {}", report.evidence(), report.reason);
    assert_eq!((report.width, report.height), ULTRAWIDE);
    assert!(report.passed, "{}", report.reason);
}
