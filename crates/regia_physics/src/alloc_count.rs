// SPDX-License-Identifier: MPL-2.0
//! Counting allocator for `cargo test --lib` perf regressions: the shared
//! `regia_test_utils::alloc_counter::CountingAlloc` the allocation tests install (ADR-3291), so
//! this crate's `src` has no `unsafe` of its own (ADR-7386). Integration-test binaries compile
//! this crate without `cfg(test)`, so they keep their own allocators.

use regia_test_utils::alloc_counter::{CountingAlloc, thread_allocs};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// Heap allocations (and reallocations) on this thread since the thread started.
pub(crate) fn allocations() -> usize {
    usize::try_from(thread_allocs()).unwrap_or(usize::MAX)
}
