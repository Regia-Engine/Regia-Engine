// SPDX-License-Identifier: MPL-2.0
//! `regia_test_utils::alloc_counter` installed as the global allocator, the way every allocation test
//! binary installs it (ADR-3291): what the counters report for real `Vec`, `Box` and thread work.
//!
//! Its own test binary: it installs the counting global allocator. The tests of this binary run
//! beside each other, so the process-wide checks are lower bounds.

use regia_test_utils::alloc_counter::{self, AllocStats, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

#[test]
fn a_vec_that_grows_is_one_allocation_and_one_realloc() {
    let (v, stats) = alloc_counter::measure(|| {
        let mut v: Vec<u64> = Vec::with_capacity(4);
        v.extend(0..4);
        // Exactly 16 slots: one `realloc` from 32 to 128 bytes, its growth 96.
        v.reserve_exact(12);
        v
    });
    assert_eq!(v.capacity(), 16);
    assert_eq!(
        stats,
        AllocStats {
            allocs: 2,
            bytes: 128
        }
    );
    let ((), freed) = alloc_counter::measure(|| drop(v));
    assert_eq!(freed.allocs, 0, "a free is not a request");
}

#[test]
fn zeroed_vectors_count_once() {
    let (v, stats) = alloc_counter::measure(|| vec![0u8; 4096]);
    assert_eq!(v.len(), 4096);
    assert_eq!(
        stats,
        AllocStats {
            allocs: 1,
            bytes: 4096
        }
    );
}

#[test]
fn live_bytes_peak_inside_and_return_after() {
    let base = alloc_counter::thread_live_bytes();
    let ((), peak) = alloc_counter::peak_bytes(|| {
        let big = vec![1u8; 1 << 20];
        assert_eq!(big.len(), 1 << 20);
    });
    assert!(peak >= 1 << 20, "{peak}");
    assert_eq!(alloc_counter::thread_live_bytes(), base);
}

#[test]
fn a_counted_worker_thread_shows_in_counted_stats_only_while_counted() {
    let before = alloc_counter::counted_stats();
    let handle = std::thread::spawn(|| {
        // `black_box` keeps both boxes: only their first elements are read, so an optimized build
        // may otherwise remove the allocations (the v050rc1b gate counted none).
        alloc_counter::count_this_thread();
        let kept = std::hint::black_box(Box::new([7u64; 4]));
        alloc_counter::stop_counting_this_thread();
        let after = std::hint::black_box(Box::new([8u64; 4]));
        kept[0] + after[0]
    });
    assert_eq!(handle.join().expect("worker"), 15);
    let counted = alloc_counter::counted_stats().since(before);
    // The only counted request in this binary: the worker's first box.
    assert_eq!(counted, AllocStats { allocs: 1, bytes: 32 });
}

#[test]
fn the_process_counters_see_other_threads() {
    let before = alloc_counter::process_stats();
    let ((), mine) = alloc_counter::measure(|| {
        std::thread::spawn(|| drop(std::hint::black_box(vec![3u8; 1000])))
            .join()
            .expect("thread");
    });
    let all = alloc_counter::process_stats().since(before);
    assert!(all.allocs > mine.allocs, "{all:?} vs {mine:?}");
    assert!(all.bytes >= 1000 + mine.bytes, "{all:?} vs {mine:?}");
}
