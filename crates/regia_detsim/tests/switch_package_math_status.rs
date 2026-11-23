// SPDX-License-Identifier: MPL-2.0
//! Review proof for the v0.7 switch package's F-01 adoption instructions.

#[test]
fn switch_package_does_not_defer_the_already_landed_atan2_change() {
    use regia_detsim::{float::canonical, math};

    // DO27's old wrapper canonicalizes inputs and output around libm. Use a finite
    // hard case so the difference is rounding, not the signed-zero policy.
    let (y, x) = (8.681_013_f32, -8_809_090.0_f32);
    let old = canonical(math::atan2_libm(canonical(y), canonical(x))).to_bits();
    let new = math::atan2(y, x).to_bits();
    assert_eq!(regia_detsim::MATH_BITS_VERSION, 1);
    assert_eq!((old, new), (0x4049_0fd6, 0x4049_0fd7));
    let page = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/user-guide/do27-switch-v07.md"),
    )
    .expect("read the switch package");
    let row = page
        .lines()
        .find(|line| line.contains("(F-01)"))
        .expect("F-01 row");
    assert!(
        !row.contains("would move it before the tag"),
        "F-01 calls canonicalization future work, but atan2({y}, {x}) already moved: {old:08x} -> {new:08x}"
    );
}
