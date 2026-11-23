// SPDX-License-Identifier: MPL-2.0
//! ADR-2530: the bus meter tap allocates nothing in steady state.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `regia_physics/tests/cloth_alloc.rs`).

use kira::Frame;
use regia_audio::bus_meter::BusMeterBuilder;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

#[test]
fn metering_allocates_nothing_after_the_tap_is_built() {
    let (builder, handle) = BusMeterBuilder::new();
    let mut tap = builder.into_tap();
    let dt = 1.0 / 48_000.0;
    let block: Vec<Frame> = (0..256)
        .map(|i| {
            let s = 0.5 * (i as f32 * 0.131).sin();
            Frame { left: s, right: -s }
        })
        .collect();
    // Warm-up: the first block fixes the hop length.
    tap.process_block(&block, dt);
    let before = allocations();
    // Ten seconds of audio: far past the 3 s short-term ring, many hop wraps.
    for _ in 0..(10 * 48_000 / 256) {
        tap.process_block(&block, dt);
        let _ = handle.read();
    }
    assert_eq!(
        allocations() - before,
        0,
        "the meter tap must not allocate in steady state"
    );
    assert!(handle.read().peak_db.is_finite());
}
