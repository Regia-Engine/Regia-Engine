// SPDX-License-Identifier: MPL-2.0
//! The no-feature dependency has no recorder: even unresolved argument tokens disappear.
#[cfg(not(feature = "visual-log"))]
#[test]
fn visual_log_shipping_discards_arguments_and_side_effects() {
    let called = std::cell::Cell::new(false);
    regia_framework::debug_log!(
        nonexistent_entity,
        text,
        nonexistent_recorder,
        nonexistent_position,
        {
            called.set(true);
            "not retained"
        }
    );
    assert!(!called.get());
}
