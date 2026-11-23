// SPDX-License-Identifier: MPL-2.0
//! Physics events (specs/15 § 3): ECS messages raised by the step, plus the bridge that
//! buffers rapier's collision/contact-force events during `PhysicsPipeline::step`.
//!
//! The bridge implements rapier's `EventHandler` over a mutex-guarded buffer (a field of
//! [`PhysicsWorld`](crate::world::PhysicsWorld), split-borrowed for the step) and the
//! `flush_physics_events` system drains it into `Messages<T>` after the step. Sensor
//! colliders turn Started/Stopped pairs into trigger events.
//!
//! Provenance: event names and thresholds follow rapier's `EventHandler` /
//! `ActiveEvents::CONTACT_FORCE_EVENTS` public API (Apache-2.0); trigger-vs-collision
//! splitting on `Collider::is_sensor` is Regia Engine's.

use std::sync::{Mutex, MutexGuard};

use rapier3d::geometry::ColliderHandle;
use regia_ecs::prelude::*;

/// Two colliders began touching (specs/15 § 3).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct CollisionStarted {
    /// One participant.
    pub entity_a: Entity,
    /// The other participant.
    pub entity_b: Entity,
}

/// Two colliders stopped touching (specs/15 § 3).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct CollisionEnded {
    /// One participant.
    pub entity_a: Entity,
    /// The other participant.
    pub entity_b: Entity,
}

/// An entity entered a sensor collider (specs/15 § 3).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct TriggerEnter {
    /// The entity owning the sensor collider.
    pub trigger: Entity,
    /// The entity that entered the volume.
    pub other: Entity,
}

/// An entity left a sensor collider (specs/15 § 3).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct TriggerExit {
    /// The entity owning the sensor collider.
    pub trigger: Entity,
    /// The entity that left the volume.
    pub other: Entity,
}

/// The total force on a contact pair crossed a collider's `contact_force_threshold`
/// (specs/15 § 3).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct ContactForce {
    /// One participant.
    pub entity_a: Entity,
    /// The other participant.
    pub entity_b: Entity,
    /// Total force magnitude on the contact pair (newtons).
    pub total_force: f32,
}

/// A joint's constraint force crossed its `break_force` and the joint was removed
/// (specs/15 § 2).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct JointBroken {
    /// The entity that carried the [`Joint`](crate::components::Joint).
    pub entity: Entity,
    /// The body it was connected to.
    pub connected: Entity,
}

/// One raw event buffered during a step, before entity mapping.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RawEvent {
    /// A collider pair started or stopped touching (either side may be a sensor).
    Pair {
        /// `true` when the pair began touching.
        started: bool,
        /// First collider.
        a: ColliderHandle,
        /// Second collider.
        b: ColliderHandle,
    },
    /// A contact pair's total force crossed its threshold.
    Force {
        /// First collider.
        a: ColliderHandle,
        /// Second collider.
        b: ColliderHandle,
        /// Total force magnitude (newtons).
        magnitude: f32,
    },
    /// A joint broke (entities already resolved).
    BrokenJoint {
        /// The entity that carried the joint.
        entity: Entity,
        /// The connected body's entity.
        connected: Entity,
    },
}

/// The step's event buffer (a field of `PhysicsWorld`).
#[derive(Default)]
pub(crate) struct EventBuffer {
    raw: Mutex<Vec<RawEvent>>,
}

impl EventBuffer {
    /// Takes all buffered events, leaving the buffer empty.
    pub(crate) fn drain(&self) -> Vec<RawEvent> {
        let mut guard = self.lock();
        std::mem::take(&mut *guard)
    }

    /// Buffers one event (the step handler and the joint-break check).
    pub(crate) fn push(&self, event: RawEvent) {
        self.lock().push(event);
    }

    /// Locks without propagating poisoning (the critical sections are panic-free pushes).
    fn lock(&self) -> MutexGuard<'_, Vec<RawEvent>> {
        self.raw.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Rapier `EventHandler` for one step: buffers collision and contact-force events.
pub(crate) struct StepEventHandler<'a> {
    /// Where events are buffered.
    pub buffer: &'a EventBuffer,
}

impl rapier3d::pipeline::EventHandler for StepEventHandler<'_> {
    fn handle_collision_event(
        &self,
        _bodies: &rapier3d::dynamics::RigidBodySet,
        _colliders: &rapier3d::geometry::ColliderSet,
        event: rapier3d::geometry::CollisionEvent,
        _contact_pair: Option<&rapier3d::geometry::ContactPair>,
    ) {
        let (handle1, handle2) = match event {
            rapier3d::geometry::CollisionEvent::Started(h1, h2, _)
            | rapier3d::geometry::CollisionEvent::Stopped(h1, h2, _) => (h1, h2),
        };
        self.buffer.push(RawEvent::Pair {
            started: event.started(),
            a: handle1,
            b: handle2,
        });
    }

    fn handle_contact_force_event(
        &self,
        _dt: rapier3d::math::Real,
        _bodies: &rapier3d::dynamics::RigidBodySet,
        _colliders: &rapier3d::geometry::ColliderSet,
        contact_pair: &rapier3d::geometry::ContactPair,
        total_force_magnitude: rapier3d::math::Real,
    ) {
        self.buffer.push(RawEvent::Force {
            a: contact_pair.collider1,
            b: contact_pair.collider2,
            magnitude: total_force_magnitude,
        });
    }

    fn handle_soft_body_tear_event(
        &self,
        _soft_bodies: &rapier3d::dynamics::SoftBodySet,
        _event: &rapier3d::dynamics::SoftBodyTearEvent,
    ) {
        // Regia Engine does not simulate soft bodies (specs/15); nothing to report.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_drains_once() {
        let buffer = EventBuffer::default();
        buffer.push(RawEvent::Force {
            a: ColliderHandle::from_raw_parts(1, 0),
            b: ColliderHandle::from_raw_parts(2, 0),
            magnitude: 5.0,
        });
        buffer.push(RawEvent::Force {
            a: ColliderHandle::from_raw_parts(1, 0),
            b: ColliderHandle::from_raw_parts(2, 0),
            magnitude: 6.0,
        });
        assert_eq!(buffer.drain().len(), 2);
        assert!(buffer.drain().is_empty(), "drain is destructive");
    }
}
