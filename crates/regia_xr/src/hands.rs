// SPDX-License-Identifier: MPL-2.0
//! Hand tracking snapshots (spec 58 § 2.7, optional): 26 joints per hand located at the predicted
//! display time, with radius and validity flags. The joint set and order are the OpenXR hand
//! tracking extension's (public specification); a runtime without it reports `Unsupported`.

use regia_app::prelude::*;
use regia_platform::xr::{Hand, Pose};

/// Joints per hand.
pub const HAND_JOINT_COUNT: usize = 26;

/// A hand joint, in the standard's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HandJoint {
    /// The palm joint.
    Palm,
    /// The wrist joint.
    Wrist,
    /// The thumb metacarpal joint.
    ThumbMetacarpal,
    /// The thumb proximal joint.
    ThumbProximal,
    /// The thumb distal joint.
    ThumbDistal,
    /// The thumb tip joint.
    ThumbTip,
    /// The index metacarpal joint.
    IndexMetacarpal,
    /// The index proximal joint.
    IndexProximal,
    /// The index intermediate joint.
    IndexIntermediate,
    /// The index distal joint.
    IndexDistal,
    /// The index tip joint.
    IndexTip,
    /// The middle metacarpal joint.
    MiddleMetacarpal,
    /// The middle proximal joint.
    MiddleProximal,
    /// The middle intermediate joint.
    MiddleIntermediate,
    /// The middle distal joint.
    MiddleDistal,
    /// The middle tip joint.
    MiddleTip,
    /// The ring metacarpal joint.
    RingMetacarpal,
    /// The ring proximal joint.
    RingProximal,
    /// The ring intermediate joint.
    RingIntermediate,
    /// The ring distal joint.
    RingDistal,
    /// The ring tip joint.
    RingTip,
    /// The little metacarpal joint.
    LittleMetacarpal,
    /// The little proximal joint.
    LittleProximal,
    /// The little intermediate joint.
    LittleIntermediate,
    /// The little distal joint.
    LittleDistal,
    /// The little tip joint.
    LittleTip,
}

impl HandJoint {
    /// Every joint, in index order.
    pub const ALL: [HandJoint; HAND_JOINT_COUNT] = [
        HandJoint::Palm,
        HandJoint::Wrist,
        HandJoint::ThumbMetacarpal,
        HandJoint::ThumbProximal,
        HandJoint::ThumbDistal,
        HandJoint::ThumbTip,
        HandJoint::IndexMetacarpal,
        HandJoint::IndexProximal,
        HandJoint::IndexIntermediate,
        HandJoint::IndexDistal,
        HandJoint::IndexTip,
        HandJoint::MiddleMetacarpal,
        HandJoint::MiddleProximal,
        HandJoint::MiddleIntermediate,
        HandJoint::MiddleDistal,
        HandJoint::MiddleTip,
        HandJoint::RingMetacarpal,
        HandJoint::RingProximal,
        HandJoint::RingIntermediate,
        HandJoint::RingDistal,
        HandJoint::RingTip,
        HandJoint::LittleMetacarpal,
        HandJoint::LittleProximal,
        HandJoint::LittleIntermediate,
        HandJoint::LittleDistal,
        HandJoint::LittleTip,
    ];

    /// The joint's index (0 = palm).
    pub fn index(self) -> usize {
        self as usize
    }
}

/// One located joint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointLocation {
    /// Pose in tracking space.
    pub pose: Pose,
    /// Joint radius, meters.
    pub radius: f32,
    /// The position is valid.
    pub position_valid: bool,
    /// The orientation is valid.
    pub orientation_valid: bool,
    /// Tracked (true) rather than inferred.
    pub tracked: bool,
}

impl JointLocation {
    /// An invalid joint at the origin.
    pub const INVALID: JointLocation = JointLocation {
        pose: Pose::IDENTITY,
        radius: 0.0,
        position_valid: false,
        orientation_valid: false,
        tracked: false,
    };

    fn finite(&self) -> bool {
        self.pose.position.is_finite() && self.pose.orientation.is_finite() && self.radius.is_finite()
    }
}

/// Whether hand tracking is available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum HandTrackingSupport {
    /// The runtime does not advertise the extension (or it failed and was disabled).
    #[default]
    Unsupported,
    /// Joints are located each frame.
    Supported,
}

impl HandTrackingSupport {
    /// The lowercase name `xr.get` reports.
    pub fn name(self) -> &'static str {
        match self {
            HandTrackingSupport::Unsupported => "unsupported",
            HandTrackingSupport::Supported => "supported",
        }
    }
}

/// Both hands' joints at the last locate.
#[derive(Debug, Clone, PartialEq, Resource)]
pub struct XrHands {
    /// Whether hand tracking is available.
    pub support: HandTrackingSupport,
    joints: [[JointLocation; HAND_JOINT_COUNT]; 2],
    bad_joints: u64,
}

impl Default for XrHands {
    fn default() -> Self {
        XrHands {
            support: HandTrackingSupport::Unsupported,
            joints: [[JointLocation::INVALID; HAND_JOINT_COUNT]; 2],
            bad_joints: 0,
        }
    }
}

impl XrHands {
    /// The joints of `hand`.
    pub fn joints(&self, hand: Hand) -> &[JointLocation; HAND_JOINT_COUNT] {
        &self.joints[hand.index()]
    }

    /// One joint.
    pub fn joint(&self, hand: Hand, joint: HandJoint) -> JointLocation {
        self.joints[hand.index()][joint.index()]
    }

    /// Located joints rejected because a value was not finite (the previous joint was kept).
    pub fn bad_joints(&self) -> u64 {
        self.bad_joints
    }

    /// Stores a located hand. A joint with a non-finite value is rejected and its previous
    /// location kept (counted in [`XrHands::bad_joints`]). Returns how many were rejected.
    pub fn update(&mut self, hand: Hand, located: &[JointLocation; HAND_JOINT_COUNT]) -> u64 {
        let mut rejected = 0;
        for (slot, new) in self.joints[hand.index()].iter_mut().zip(located) {
            if new.finite() {
                *slot = *new;
            } else {
                rejected += 1;
            }
        }
        self.bad_joints += rejected;
        rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::Vec3;

    #[test]
    fn there_are_26_joints_in_order() {
        assert_eq!(HandJoint::ALL.len(), 26);
        for (i, j) in HandJoint::ALL.iter().enumerate() {
            assert_eq!(j.index(), i);
        }
        assert_eq!(HandJoint::LittleTip.index(), 25);
    }

    #[test]
    fn non_finite_joints_are_rejected_and_the_previous_kept() {
        let mut hands = XrHands::default();
        let mut located = [JointLocation {
            pose: Pose::new(Vec3::new(0.1, 1.0, -0.2), Default::default()),
            radius: 0.01,
            position_valid: true,
            orientation_valid: true,
            tracked: true,
        }; HAND_JOINT_COUNT];
        assert_eq!(hands.update(Hand::Left, &located), 0);
        located[HandJoint::IndexTip.index()].pose.position.x = f32::NAN;
        located[HandJoint::Palm.index()].radius = 0.02;
        assert_eq!(hands.update(Hand::Left, &located), 1);
        assert_eq!(hands.bad_joints(), 1);
        assert_eq!(hands.joint(Hand::Left, HandJoint::IndexTip).pose.position.x, 0.1);
        assert_eq!(hands.joint(Hand::Left, HandJoint::Palm).radius, 0.02);
        assert!(!hands.joint(Hand::Right, HandJoint::Palm).position_valid);
    }
}
