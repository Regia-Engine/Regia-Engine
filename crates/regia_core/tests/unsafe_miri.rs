// SPDX-License-Identifier: MPL-2.0
//! Tests of `regia_core`'s `unsafe` that exist for the Miri subset (`docs/06-testing-and-ci.md`
//! § 14, ADR-2726), through the public API. They also run natively.

#![allow(unsafe_code)] // calls a `GlobalAlloc` by hand (docs/04-conventions.md § 5)

use std::alloc::{GlobalAlloc, Layout};

use regia_core::memory::TaggingAllocator;
use regia_core::profiling::thread_cpu_time;

/// The clock call writes its own out parameter and nothing else. Miri's clock shim may refuse this
/// clock id with `EINVAL`: `None`, and the value is never read.
#[test]
fn the_thread_clock_writes_only_its_out_parameter() {
    let (first, second) = (thread_cpu_time(), thread_cpu_time());
    if let (Some(first), Some(second)) = (first, second) {
        assert!(second >= first, "{second:?} < {first:?}");
    }
}

/// A live block on its way to the thread that frees it. A pointer stays a pointer: Miri tracks
/// provenance, and one rebuilt from an integer would hide what this test checks.
struct Block(*mut u8);

// SAFETY: the sender gives up the block when it sends it; only the receiver uses it from then on.
unsafe impl Send for Block {}

/// Two threads share one allocator, and a block allocated on one is grown and freed on the other,
/// as the global allocator sees it. Under Miri a race on the header or the ledger is an error.
#[test]
fn a_block_can_change_threads_between_alloc_and_free() {
    const ROUNDS: usize = 16;
    static ALLOC: TaggingAllocator = TaggingAllocator::system();
    let layout = Layout::from_size_align(24, 32).expect("layout");
    let (sender, receiver) = std::sync::mpsc::channel::<Block>();
    let freer = std::thread::spawn(move || {
        let mut freed = 0;
        for Block(block) in receiver {
            // SAFETY: `block` came from `ALLOC.alloc(layout)`, and the sender gave up its copy.
            let grown = unsafe { ALLOC.realloc(block, layout, 48) };
            assert!(!grown.is_null());
            // SAFETY: the first 24 bytes moved with the block.
            assert_eq!(unsafe { grown.add(23).read() }, 0x5C);
            let grown_layout = Layout::from_size_align(48, 32).expect("layout");
            // SAFETY: `grown` is live with `grown_layout`.
            unsafe { ALLOC.dealloc(grown, grown_layout) };
            freed += 1;
        }
        freed
    });
    for _ in 0..ROUNDS {
        // SAFETY: a non-zero size; the receiver frees the block.
        let block = unsafe { ALLOC.alloc(layout) };
        assert!(!block.is_null());
        // SAFETY: 24 writable bytes.
        unsafe { block.write_bytes(0x5C, 24) };
        sender.send(Block(block)).expect("the thread is alive");
    }
    drop(sender);
    assert_eq!(freer.join().expect("the thread ends"), ROUNDS);
}
