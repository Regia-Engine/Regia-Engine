// SPDX-License-Identifier: MPL-2.0
//! Without the tagging allocator the heap ledger says it is not tracking (mutation testing, ADR-3389): this
//! binary installs no global allocator.

use regia_core::memory;

#[test]
fn without_the_allocator_tracking_is_off_and_live_bytes_are_unknown() {
    let held: Vec<u8> = Vec::with_capacity(1 << 16);
    assert!(!memory::tracking_installed());
    assert_eq!(memory::live_heap_bytes(), None);
    let snap = memory::snapshot();
    assert!(!snap.tracking);
    assert_eq!(snap.live_bytes, 0);
    assert!(snap.table().contains("not tracked"));
    drop(held);
}
