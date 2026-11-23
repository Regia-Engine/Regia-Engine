// SPDX-License-Identifier: MIT-0
//! The parts of the animation lab its tests run without downloaded content (ADR-7216).
//!
//! [`reach`] is the spec 17 § 6 moving-target case: two-bone IK and look-at on a code-built rig,
//! and the turret aiming at a moving `AimTarget`, on the sample's checked-in graphs. The binary
//! prints its report next to the downloaded-model cases; `tests/reach_acceptance.rs` asserts it.
//!
//! [`locomotion`] is the spec 17 § 6 walker-gait case: a `GaitRig` hexapod over steps and a slope
//! and a `FootPlacement` biped on stairs, driven by a scripted mover. `tests/locomotion_acceptance.rs`
//! asserts its numbers and `tests/locomotion_agent.rs` drives it through the agent protocol.

pub mod locomotion;
pub mod reach;
