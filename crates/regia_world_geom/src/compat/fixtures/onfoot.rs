// SPDX-License-Identifier: MPL-2.0
//! The on-foot sizes the `compat::onfoot` tests and fixtures pass (plan row W-03).
//!
//! These are test values, not engine defaults: [`crate::compat::onfoot::OnFootParams`] has no
//! default, and a host passes its own. They are the sizes the source's on-foot tests were written
//! against (a 0.35 m capsule radius, a 1.85 m walker, a 1.10 m door, a 9 m building rim), so the
//! ported asserts keep their meaning.

use crate::compat::onfoot::OnFootParams;

/// The fixture walker: 0.35 m capsule radius, 1.85 m tall, 1.10 m doors, 9 m building rim.
pub const ON_FOOT_PARAMS: OnFootParams = OnFootParams {
    capsule_radius_m: 0.35,
    height_m: 1.85,
    door_width_m: 1.10,
    building_radius_m: 9.0,
};
