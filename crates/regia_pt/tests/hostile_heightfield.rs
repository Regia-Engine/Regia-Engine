// SPDX-License-Identifier: MPL-2.0
//! Regression: a heightfield whose `width × depth` wraps `u32` must not index out of range.

use regia_pt::PtHeightfield;

fn field(side: u32, len: usize) -> PtHeightfield {
    PtHeightfield {
        width: side,
        depth: side,
        origin: [0.0, 0.0],
        spacing: [1.0, 1.0],
        heights: vec![0.0; len],
        material: 0,
    }
}

#[test]
fn wrapping_sample_count_is_rejected() {
    // 65537² wraps to 131073 in u32; the buffer passes a wrapped length check.
    assert_eq!(field(65537, 131_073).height_at(0.0, 0.0), None);
    // 65536² wraps to 0; an empty buffer must not satisfy the guard.
    assert_eq!(field(65536, 0).height_at(0.0, 0.0), None);
}

#[test]
fn well_formed_field_still_samples() {
    let f = field(4, 16);
    assert_eq!(f.height_at(1.5, 1.5), Some(0.0));
    assert_eq!(f.height(9, 9), 0.0);
}
