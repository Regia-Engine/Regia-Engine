// SPDX-License-Identifier: MPL-2.0
//! Spec 54 § 4.1: an uncaptured validation or internal error runs the breadcrumb bug-bundle hook
//! (the feedback plugin registers it); an out-of-memory error never does (it signals memory
//! pressure instead). No GPU: the uncaptured-error path is called with wgpu's error values.
//!
//! Its own test binary: the hook and the error counters are process-wide.

use std::sync::Mutex;

use regia_render::gpu_health;

static CALLS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

fn hook(kind: &'static str) {
    CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(kind);
}

fn calls() -> Vec<&'static str> {
    CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[test]
fn validation_and_internal_errors_run_the_hook_and_out_of_memory_does_not() {
    gpu_health::set_validation_bundle_hook(hook);
    gpu_health::note_uncaptured_error(&wgpu::Error::OutOfMemory {
        source: Box::new(std::io::Error::other("test source")),
    });
    assert_eq!(
        calls(),
        Vec::<&str>::new(),
        "out of memory writes no breadcrumb bundle"
    );
    assert_eq!(gpu_health::gpu_oom_count(), 1);

    gpu_health::note_uncaptured_error(&wgpu::Error::Validation {
        source: Box::new(std::io::Error::other("test source")),
        description: "test validation".to_owned(),
    });
    gpu_health::note_uncaptured_error(&wgpu::Error::Internal {
        source: Box::new(std::io::Error::other("test source")),
        description: "test internal".to_owned(),
    });
    assert_eq!(calls(), ["validation", "internal"]);
    assert_eq!(
        gpu_health::gpu_error_count(),
        3,
        "every uncaptured error is counted"
    );
}
