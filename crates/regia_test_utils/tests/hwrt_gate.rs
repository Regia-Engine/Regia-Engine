// SPDX-License-Identifier: MPL-2.0
//! The run-time gate must decide from its own device's features, not the process-wide
//! `last_device_report()` (audit rescue-hwrt-lavapipe-coverage finding 1, fixed with it).
//!
//! Before the fix, `ray_query_device()` read `last_device_report()` after creating its device.
//! That global names whichever device was created most recently, so a sibling test's plain
//! device created in the window between the gate's device construction and the report read made
//! the gate skip on a ray-capable adapter and the test passed having tested nothing. The test
//! below reproduces the interleaving: sibling threads keep overwriting the process-wide report
//! with what a plain device on a ray-capable adapter records (`NotRequested`) while the gate runs.
//!
//! The siblings write the report through `regia_render::hwrt::record_device_report_for_test`, the
//! function every device creation calls, and create no device (ADR-7570). Until gate v050rc3 they
//! created 48 real devices, each with its own instance and dropped at once: on an RTX 5090 the test
//! outlasted the merge gate's 600 s, and on the build server's 5090s one run spun for more than 900 s in
//! the NVIDIA driver livelock `RenderDevice::shared_headless` documents (a driver thread busy in
//! user space, two sibling threads parked on a priority-inheritance futex inside the driver).
//! Keeping those devices alive instead ran the card out of devices (`Device(Lost)` at creation).
//! A sibling now writes the report many times during each gate call instead of 12 times in all.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use regia_render::hwrt::{HwrtReport, HwrtRequest, RayTracingProbe, record_device_report_for_test};

#[test]
fn ray_query_device_ignores_sibling_devices_reports() {
    const SIBLINGS: usize = 3;
    const GATE_CALLS: usize = 4;

    // The report a plain device on a ray-capable adapter records: offered, not requested.
    let probe = RayTracingProbe::from_parts(
        wgpu::Backend::Vulkan,
        wgpu::Features::EXPERIMENTAL_RAY_QUERY,
        &wgpu::Limits::default(),
    );
    let plain = HwrtReport::new(
        HwrtRequest::Off,
        probe,
        probe.decide(HwrtRequest::Off, false),
        wgpu::Features::empty(),
    );
    assert!(!plain.device_ray_query, "{plain:?}");

    let done = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let siblings: Vec<_> = (0..SIBLINGS)
        .map(|_| {
            let done = Arc::clone(&done);
            let writes = Arc::clone(&writes);
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    record_device_report_for_test(plain);
                    writes.fetch_add(1, Ordering::AcqRel);
                    std::thread::yield_now();
                }
            })
        })
        .collect();

    let locks_before = regia_render::loader_list_lock_count();
    let mut failures = Vec::new();
    for call in 0..GATE_CALLS {
        // Each call starts after a sibling write: the siblings are running (a counter, not a sleep).
        let before = writes.load(Ordering::Acquire);
        while writes.load(Ordering::Acquire) == before && !siblings.iter().any(|s| s.is_finished()) {
            std::thread::yield_now();
        }
        match regia_test_utils::hwrt::ray_query_device() {
            Some(device) => {
                assert!(
                    device
                        .device
                        .features()
                        .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
                    "gate returned a device without the ray-query feature"
                );
            }
            None => failures.push(call),
        }
    }
    let locks = regia_render::loader_list_lock_count() - locks_before;

    done.store(true, Ordering::Release);
    for sibling in siblings {
        sibling.join().expect("sibling report-writing thread panicked");
    }
    assert!(writes.load(Ordering::Acquire) >= GATE_CALLS as u64);
    // One instance and one device per gate call (each takes the loader-list lock once, ADR-2113):
    // the siblings create nothing.
    assert!(
        locks <= (2 * GATE_CALLS) as u64,
        "{locks} instance and device creations for {GATE_CALLS} gate calls"
    );
    assert!(
        failures.is_empty(),
        "the gate skipped on a ray-capable adapter: a sibling device's report won the race \
         (calls {failures:?})"
    );
}
