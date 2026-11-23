// SPDX-License-Identifier: MPL-2.0
//! The heap ledger with the tagging allocator installed (mutation testing, ADR-3389): peaks follow the first
//! allocation of a subsystem and a reallocation in place, and the process-wide peak stays a byte count.
//!
//! Each test uses a subsystem no other test in this binary touches, so its first allocation there is the one
//! the test makes.

use regia_core::memory::{self, Subsystem, TaggingAllocator};

#[global_allocator]
static ALLOC: TaggingAllocator = TaggingAllocator::system();

const MIB: usize = 1 << 20;
/// Far above any real peak of this test binary; a peak computed from a product of byte counts lands above it.
const SANE_PEAK: u64 = 64 << 30;

#[test]
fn tracking_is_on_and_live_bytes_include_a_held_block() {
    let held: Vec<u8> = Vec::with_capacity(MIB);
    assert!(memory::tracking_installed());
    let live = memory::live_heap_bytes().expect("the allocator is installed");
    assert!(live >= MIB as u64, "a held 1 MiB block is live: {live}");
    assert!(memory::snapshot().tracking);
    drop(held);
}

#[test]
fn the_first_allocation_of_a_subsystem_sets_its_peak() {
    let size = 4 * MIB;
    memory::scoped(Subsystem::Navigation, || {
        let block: Vec<u8> = Vec::with_capacity(size);
        drop(block);
    });
    let m = memory::snapshot().subsystem(Subsystem::Navigation);
    assert!(
        m.peak_bytes >= size as u64,
        "peak {} after a {size}-byte block",
        m.peak_bytes
    );
    assert!(m.total_bytes >= size as u64);
    assert!(m.total_allocations >= 1);
}

#[test]
fn the_process_peak_is_a_byte_count_not_a_product() {
    memory::scoped(Subsystem::Vfx, || {
        let first: Vec<u8> = Vec::with_capacity(MIB);
        let second: Vec<u8> = Vec::with_capacity(MIB);
        drop((first, second));
    });
    let snap = memory::snapshot();
    assert!(snap.peak_bytes >= snap.live_bytes);
    assert!(
        snap.peak_bytes < SANE_PEAK,
        "process peak {} bytes",
        snap.peak_bytes
    );
}

#[test]
fn growing_a_block_in_place_raises_its_subsystems_peak() {
    let size = 4 * MIB;
    memory::scoped(Subsystem::Terrain, || {
        // Hold a block so the process total is well above zero when the second one grows.
        let held: Vec<u8> = Vec::with_capacity(MIB);
        let mut grown: Vec<u8> = Vec::with_capacity(1);
        grown.reserve_exact(size);
        assert!(grown.capacity() >= size);
        drop((held, grown));
    });
    let snap = memory::snapshot();
    let m = snap.subsystem(Subsystem::Terrain);
    assert!(
        m.peak_bytes >= (MIB + size) as u64,
        "peak {} after holding 1 MiB and growing to {size}",
        m.peak_bytes
    );
    assert!(
        snap.peak_bytes < SANE_PEAK,
        "process peak {} bytes",
        snap.peak_bytes
    );
}

#[test]
fn shrinking_a_block_returns_bytes_to_its_subsystem() {
    let (before, after) = memory::scoped(Subsystem::Animation, || {
        let mut v: Vec<u8> = Vec::with_capacity(2 * MIB);
        let before = memory::snapshot().subsystem(Subsystem::Animation).live_bytes;
        v.shrink_to(MIB / 2);
        let after = memory::snapshot().subsystem(Subsystem::Animation).live_bytes;
        drop(v);
        (before, after)
    });
    assert!(
        before >= after + (MIB + MIB / 2) as u64,
        "live {before} -> {after} after shrinking 2 MiB to 0.5 MiB"
    );
    let m = memory::snapshot().subsystem(Subsystem::Animation);
    assert_eq!(m.live_allocations, 0, "every block of the subsystem was freed");
    assert_eq!(m.live_bytes, 0);
}
