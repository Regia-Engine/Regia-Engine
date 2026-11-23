// SPDX-License-Identifier: MPL-2.0
//! `test_support/allocation_counter.rs` is a byte-identical copy of the audited allocator in
//! `regia_test_utils/src/alloc_counter.rs` (ADR-8246, ADR-8421): the library may not reach outside
//! its folder (`cargo xtask check-packaging`, ADR-1485), and a second allocator implementation is
//! not allowed. Editing one file means copying it to the other.

#[test]
fn the_allocation_counter_is_the_audited_copy() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let ours = std::fs::read(manifest.join("test_support/allocation_counter.rs"));
    let audited = std::fs::read(manifest.join("../regia_test_utils/src/alloc_counter.rs"));
    let (ours, audited) = match (ours, audited) {
        (Ok(ours), Ok(audited)) => (ours, audited),
        (ours, audited) => panic!(
            "reading the two allocator files: {:?} / {:?}",
            ours.err(),
            audited.err()
        ),
    };
    assert!(
        ours == audited,
        "crates/regia_world_geom/test_support/allocation_counter.rs differs from \
         crates/regia_test_utils/src/alloc_counter.rs: copy the audited file over it"
    );
}
