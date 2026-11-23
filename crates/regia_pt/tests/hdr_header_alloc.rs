// SPDX-License-Identifier: MPL-2.0
//! audit-v034b-formats M2: the Radiance HDR reader allocated the whole frame (`width × height`
//! RGBE pixels) from the resolution line before reading any pixel data, so a 50-byte file that
//! claims 32768 × 32768 asked for 4 GiB. This binary counts the largest single allocation
//! request while such a file is decoded. Its own test binary: it installs a global allocator.

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): the largest single request of any thread.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

#[test]
fn a_tiny_file_claiming_a_huge_frame_does_not_reserve_it() {
    let file = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 32768 +X 32768\n\x02\x02\x80\x00".to_vec();
    alloc_counter::reset_largest_request();
    let result = regia_pt::hdr::decode(&file);
    let largest = alloc_counter::largest_request();
    assert!(result.is_err(), "the pixel data is missing");
    assert!(
        largest < 16 << 20,
        "decoding a {}-byte file asked for a {largest}-byte allocation",
        file.len()
    );
    // A real image still decodes.
    let rgb = vec![[0.5f32, 0.25, 1.0]; 16 * 4];
    let encoded = regia_pt::hdr::encode_flat(16, 4, &rgb);
    let image = regia_pt::hdr::decode(&encoded).expect("a flat image");
    assert_eq!((image.width, image.height), (16, 4));
}
