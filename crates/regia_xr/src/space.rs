// SPDX-License-Identifier: MPL-2.0
//! Tracking spaces and the rig origin (spec 58 § 2.5).
//!
//! Poses from the runtime are in the tracking space. The `XrRig` entity's world transform is the
//! tracking-space origin; a rig-local offset (recentering, the emulated floor) sits between the rig
//! and the tracked poses: `rig-local pose = offset ∘ tracked pose`. OpenXR and the engine share the
//! basis (right-handed, +Y up, −Z forward, meters), so no basis change is applied.

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::Pose;
use serde::{Deserialize, Serialize};

/// The reference frame tracked poses are expressed in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum TrackingSpace {
    /// Seated: origin at the head's start position.
    Local,
    /// Room scale: origin on the floor in the play area.
    #[default]
    Stage,
    /// Origin on the floor under the start position; emulated from `Local` plus the configured eye
    /// height when the runtime does not offer it.
    LocalFloor,
}

impl TrackingSpace {
    /// Every space, in order.
    pub const ALL: [TrackingSpace; 3] = [
        TrackingSpace::Local,
        TrackingSpace::Stage,
        TrackingSpace::LocalFloor,
    ];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            TrackingSpace::Local => "local",
            TrackingSpace::Stage => "stage",
            TrackingSpace::LocalFloor => "local_floor",
        }
    }

    /// The space named `name`.
    pub fn from_name(name: &str) -> Option<TrackingSpace> {
        TrackingSpace::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Whether the space's origin is on the floor.
    pub fn is_floor(self) -> bool {
        matches!(self, TrackingSpace::Stage | TrackingSpace::LocalFloor)
    }
}

/// The space actually used for `requested`, given the spaces the runtime `supported`. Fallback
/// order: Stage → LocalFloor → Local; LocalFloor → Local. `Local` is always available (every
/// OpenXR runtime offers it), so it is the answer when nothing in the chain is supported.
pub fn resolve_tracking_space(requested: TrackingSpace, supported: &[TrackingSpace]) -> TrackingSpace {
    let chain: &[TrackingSpace] = match requested {
        TrackingSpace::Stage => &[
            TrackingSpace::Stage,
            TrackingSpace::LocalFloor,
            TrackingSpace::Local,
        ],
        TrackingSpace::LocalFloor => &[TrackingSpace::LocalFloor, TrackingSpace::Local],
        TrackingSpace::Local => &[TrackingSpace::Local],
    };
    chain
        .iter()
        .copied()
        .find(|s| supported.contains(s))
        .unwrap_or(TrackingSpace::Local)
}

/// The rig-local offset that puts a floor under `Local` poses: `eye_height_m` up, applied only to
/// the rig offset (never to the views), when the user wants a floor (`requested` is a floor space)
/// but the runtime fell back to `Local`.
pub fn floor_offset(requested: TrackingSpace, actual: TrackingSpace, eye_height_m: f32) -> Vec3 {
    if actual == TrackingSpace::Local && requested.is_floor() {
        Vec3::new(0.0, eye_height_m, 0.0)
    } else {
        Vec3::ZERO
    }
}

/// The yaw (rotation about +Y) of `rotation`, radians: the heading of its −Z axis on the floor.
pub fn yaw_of(rotation: Quat) -> f32 {
    let forward = rotation * Vec3::NEG_Z;
    // Heading 0 looks down −Z; a positive yaw turns toward −X (right-handed about +Y).
    (-forward.x).atan2(-forward.z)
}

/// The rig-local offset that recenters `head` (a tracking-space pose): after it, the head's
/// position on the floor plane is the rig origin and its yaw is the rig's forward. Height is kept.
pub fn recenter_offset(head: &Pose) -> Pose {
    let yaw = yaw_of(head.orientation);
    let rotation = Quat::from_rotation_y(-yaw);
    let flat = Vec3::new(head.position.x, 0.0, head.position.z);
    Pose {
        position: -(rotation * flat),
        orientation: rotation,
    }
}

/// Play-area bounds of the stage space, meters (absent when the runtime does not report them).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Reflect)]
pub struct PlayArea {
    /// Extent along X.
    pub width_m: f32,
    /// Extent along Z.
    pub depth_m: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_falls_back_through_local_floor_to_local() {
        use TrackingSpace::{Local, LocalFloor, Stage};
        assert_eq!(resolve_tracking_space(Stage, &[Local, LocalFloor, Stage]), Stage);
        assert_eq!(resolve_tracking_space(Stage, &[Local, LocalFloor]), LocalFloor);
        assert_eq!(resolve_tracking_space(Stage, &[Local]), Local);
        assert_eq!(resolve_tracking_space(Stage, &[]), Local);
        assert_eq!(resolve_tracking_space(LocalFloor, &[Local, Stage]), Local);
        assert_eq!(resolve_tracking_space(Local, &[Stage]), Local);
    }

    #[test]
    fn the_floor_is_emulated_only_on_a_local_fallback() {
        use TrackingSpace::{Local, LocalFloor, Stage};
        assert_eq!(floor_offset(Stage, Local, 1.65), Vec3::new(0.0, 1.65, 0.0));
        assert_eq!(floor_offset(LocalFloor, Local, 1.5), Vec3::new(0.0, 1.5, 0.0));
        assert_eq!(
            floor_offset(Local, Local, 1.65),
            Vec3::ZERO,
            "seated keeps the head origin"
        );
        assert_eq!(floor_offset(Stage, Stage, 1.65), Vec3::ZERO);
    }

    #[test]
    fn recenter_aligns_head_yaw_and_position_with_the_rig() {
        for yaw in [0.0_f32, 0.7, -2.0, 3.0] {
            let head = Pose::new(
                Vec3::new(0.4, 1.7, -1.2),
                Quat::from_rotation_y(yaw) * Quat::from_rotation_x(0.3),
            );
            let offset = recenter_offset(&head);
            let centered = offset.compose(&head);
            assert!(
                centered.position.x.abs() < 1e-5 && centered.position.z.abs() < 1e-5,
                "{yaw}"
            );
            assert!((centered.position.y - 1.7).abs() < 1e-5, "height kept");
            assert!(yaw_of(centered.orientation).abs() < 1e-5, "{yaw}");
            let pitch_forward = centered.orientation * Vec3::NEG_Z;
            assert!(pitch_forward.y > 0.2, "pitch kept");
        }
    }

    #[test]
    fn names_round_trip() {
        for s in TrackingSpace::ALL {
            assert_eq!(TrackingSpace::from_name(s.name()), Some(s));
        }
    }
}
