// SPDX-License-Identifier: MIT-0
//! Encode time inside `render_into` with 180 moving instances whose hull the probe prepare walks.
//!
//! v0.1.3 walks every vertex every frame, so the hull run's encode p50 sits well above the
//! box control. A per-mesh bounds cache pays that walk once and stays inside the slack.
//! `render_into` p50 and p99 are recorded on the report; lavapipe's p99 tracks submit time,
//! so the assertion is the encode p50.

use lookdev::moving_units;

#[test]
fn many_moving_units_stay_within_the_render_into_budget() {
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let report = moving_units::run(&device).unwrap_or_else(|err| panic!("moving units: {err}"));
    eprintln!("moving units: {} — {}", report.evidence(), report.reason);
    eprintln!(
        "encode p50 {:.2} / p99 {:.2} ms (box encode p99 {:.2})",
        report.hull.encode_p50_ms, report.hull.encode_p99_ms, report.control.encode_p99_ms
    );
    assert!(report.probes, "probes did not run");
    assert!(report.passed, "{}", report.reason);
}
