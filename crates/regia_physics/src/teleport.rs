// SPDX-License-Identifier: MPL-2.0
//! The explicit teleport (specs/15 § 1, ADR-7233): move a body to a pose without simulating the
//! way there.
//!
//! [`teleport`] (exclusive world access) and [`TeleportCommands::teleport`] (deferred, from any
//! system) set the entity's `Transform` and add the [`Teleport`] marker, which the fixed-step sync
//! applies before the next step with rapier's `set_position`: the body's current and next poses
//! both become the target, so the jump sweeps nothing (no CCD contact, no contact events between
//! the old and the new pose), a kinematic body gets no velocity from it, and interpolation does
//! not blend across it. [`TeleportVelocity`] keeps the body's velocity or stops it.
//!
//! The marker alone (`insert(Teleport)` after writing `Transform`) still works and keeps the
//! velocity, as before.

use regia_ecs::prelude::*;
use regia_scene::Transform;

use crate::components::{RigidBody, Teleport, Velocity};

/// What happens to a body's velocity across a teleport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TeleportVelocity {
    /// The body keeps its linear and angular velocity (a portal: it comes out moving).
    #[default]
    Keep,
    /// Linear and angular velocity become zero (a respawn: it arrives at rest). Forces and
    /// pending impulses are left alone.
    Clear,
}

impl TeleportVelocity {
    /// The agent-protocol name: `"keep"` or `"clear"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Clear => "clear",
        }
    }

    /// Parses [`Self::as_str`]'s names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "keep" => Some(Self::Keep),
            "clear" => Some(Self::Clear),
            _ => None,
        }
    }
}

/// Teleports `entity` to `pose` (its `Transform`, so parent-relative when it has a parent) on the
/// next fixed step (module docs). Returns `false`, changing nothing, when the entity does not exist
/// or has no [`RigidBody`].
///
/// A non-finite pose is refused at the sync like any non-finite teleport (the body stays where it
/// is; `PhysicsStats::non_finite_inputs` counts it, ADR-1507).
pub fn teleport(world: &mut World, entity: Entity, pose: Transform, velocity: TeleportVelocity) -> bool {
    let Ok(mut target) = world.get_entity_mut(entity) else {
        return false;
    };
    if !target.contains::<RigidBody>() {
        return false;
    }
    target.insert((pose, Teleport));
    if velocity == TeleportVelocity::Clear {
        // A fresh value marks the component changed, so the sync pushes zero velocity after the
        // pose (`sync_velocity_and_forces` runs after `sync_body_properties`).
        target.insert(Velocity::default());
    }
    true
}

/// The deferred form of [`teleport`] on [`EntityCommands`].
pub trait TeleportCommands {
    /// Queues [`teleport`] for this entity (applied when the commands are; an entity without a
    /// body is left alone).
    fn teleport(&mut self, pose: Transform, velocity: TeleportVelocity) -> &mut Self;
}

impl TeleportCommands for EntityCommands<'_> {
    fn teleport(&mut self, pose: Transform, velocity: TeleportVelocity) -> &mut Self {
        let entity = self.id();
        self.commands().queue(move |world: &mut World| {
            if !teleport(world, entity, pose, velocity) {
                tracing::debug!(target: "regia_physics", "teleport of {entity:?} skipped: no RigidBody");
            }
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_names_round_trip() {
        for v in [TeleportVelocity::Keep, TeleportVelocity::Clear] {
            assert_eq!(TeleportVelocity::parse(v.as_str()), Some(v));
        }
        assert_eq!(TeleportVelocity::parse("stop"), None);
        assert_eq!(TeleportVelocity::default(), TeleportVelocity::Keep);
    }
}
