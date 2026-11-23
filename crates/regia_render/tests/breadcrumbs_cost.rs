// SPDX-License-Identifier: MPL-2.0
//! Spec 54 tests 19 and 21 as counts: a disabled writer allocates nothing and writes nothing on
//! the marker path; an enabled one allocates nothing per marker after warm-up (names interned).
//!
//! Its own test binary: it installs a counting global allocator.

use regia_render::breadcrumbs::{BreadcrumbConfig, Breadcrumbs, CrumbList};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// This thread's allocation requests so far.
fn allocs() -> u64 {
    alloc_counter::thread_allocs()
}

#[test]
fn disabled_writer_marker_path_allocates_and_writes_nothing() {
    let b = Breadcrumbs::new(BreadcrumbConfig {
        enabled: false,
        ..BreadcrumbConfig::default()
    });
    let mut list = CrumbList::new();
    let before = allocs();
    for _ in 0..1000 {
        drop(b.pass(&mut list, "gbuffer"));
    }
    assert_eq!(allocs(), before, "no allocation");
    assert!(list.is_empty(), "no list write");
}

#[test]
fn enabled_writer_allocates_nothing_per_marker_after_warm_up() {
    let b = Breadcrumbs::default();
    let mut list = CrumbList::new();
    drop(b.pass(&mut list, "gbuffer")); // interns the name
    list.clear();
    let before = allocs();
    for _ in 0..200 {
        drop(b.pass(&mut list, "gbuffer"));
    }
    assert_eq!(allocs(), before, "no heap allocation per marker");
    assert_eq!(list.len(), 400, "two 8-byte list writes per pass");
}
