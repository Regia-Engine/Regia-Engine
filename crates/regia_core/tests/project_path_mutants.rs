// SPDX-License-Identifier: MPL-2.0
//! Tests written from the mutation run of `project_path` (ADR-3389): each one pins a behavior a mutant broke
//! while the older tests still passed.

use regia_core::project_path::{ProjectPathError, check_relative};

#[test]
fn com_and_lpt_need_both_the_prefix_and_a_digit_to_be_a_device() {
    for device in ["COM1", "com9", "LPT3", "lpt0.txt", "Com4 "] {
        assert!(
            matches!(check_relative(device), Err(ProjectPathError::Device(_))),
            "{device} is a device name"
        );
    }
    // A digit after another prefix, or a non-digit after COM/LPT, is an ordinary name.
    for name in ["ABC1", "TOP5", "COMX", "LPTs", "COM", "COM12"] {
        assert!(check_relative(name).is_ok(), "{name} is not a device name");
    }
}
