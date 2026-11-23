// SPDX-License-Identifier: MIT-0
//! Paged shadows stay cached while 200 units jitter by a fraction of a millimetre,
//! and a half-metre step dirties pages. Lavapipe is enough: the check is the CPU
//! page-cache report, not a picture.

use lookdev::jitter_units;

#[test]
fn sub_millimetre_jitter_does_not_churn_paged_shadows() {
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let report = jitter_units::run(&device).unwrap_or_else(|err| panic!("jitter units: {err}"));
    eprintln!("jitter units: {} — {}", report.evidence(), report.reason);
    assert!(report.passed, "{}", report.reason);
}
