// SPDX-License-Identifier: MPL-2.0
//! Read-only views of a [`FootPlacement`]'s resolved legs, pins and last applied plan for debug
//! draw and tools (the locomotion gizmos, ADR-8729). Nothing here changes the planter.

use regia_core::math::Vec3;

use super::FootPlacement;
use crate::foot_ik::FootPlant;

/// One resolved leg of a [`FootPlacement`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootLegJoints {
    /// Hip (chain root) joint id.
    pub hip: u32,
    /// Knee (chain mid) joint id.
    pub knee: u32,
    /// Ankle (chain tip) joint id.
    pub ankle: u32,
    /// The leg's graph pole offset from the knee, skeleton units (zero: the pose-mode pole).
    pub pole_offset: Vec3,
    /// Thigh plus shin in the bind pose, skeleton units: the leg's reach.
    pub length: f32,
    /// Bind ankle height over the sole, skeleton units.
    pub ankle_height: f32,
}

impl FootPlacement {
    /// Leg `leg`'s resolved joints, or `None` before the first resolve or when the leg names
    /// did not resolve (`foot_legs_unresolved`).
    #[must_use]
    pub fn resolved_leg(&self, leg: usize) -> Option<FootLegJoints> {
        if !self.legs_ok {
            return None;
        }
        let resolved = self.resolved.get(leg)?;
        Some(FootLegJoints {
            hip: resolved.chain.root,
            knee: resolved.chain.mid,
            ankle: resolved.chain.tip,
            pole_offset: resolved.pole_offset,
            length: resolved.length,
            ankle_height: resolved.ankle_height,
        })
    }

    /// The world-space pin holding leg `leg`'s foot while its lock weight is above zero, else
    /// `None` (see [`crate::foot_ik::FootPlanter::pin`]).
    #[must_use]
    pub fn pin(&self, leg: usize) -> Option<Vec3> {
        self.planter.pin(leg)
    }

    /// The plan the last pose-mode frame applied: object space, skeleton units, targets for a
    /// rig not moved by the pelvis offset. Empty (weight 0, no legs) in graph mode and before
    /// the first pose-mode frame.
    #[must_use]
    pub fn applied_plan(&self) -> &FootPlant {
        &self.applied
    }

    /// The object-space knee pole the pose mode solves with: the first nonzero leg
    /// `pole_offset`, else −Z.
    #[must_use]
    pub fn pose_pole(&self) -> Vec3 {
        self.resolved
            .iter()
            .find(|leg| leg.pole_offset != Vec3::ZERO)
            .map_or(Vec3::NEG_Z, |leg| leg.pole_offset)
    }
}
