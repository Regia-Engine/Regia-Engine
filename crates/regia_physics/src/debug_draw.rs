// SPDX-License-Identifier: MPL-2.0
//! Physics debug draw of contacts, joints and centers of mass (spec 15 § 3, ADR-2424).
//!
//! [`PhysicsDebugDraw`] is a resource with a switch and a set of [`PhysicsDebugKinds`]. While it is
//! `on`, [`collect_physics_debug`] (a `PostUpdate` system, read-only on the rapier world) rebuilds a
//! list of [`PhysicsDebugLine`]s each frame. While it is off the system does not run at all, so the
//! feature costs one resource lookup in the run condition and nothing else
//! ([`PhysicsDebugStats::collections`] stays 0, which the tests count). `regia_framework`'s
//! `physics_debug_gizmos` draws the lines through the gizmo buffer, because the physics crate does not
//! know the renderer (`docs/02-architecture.md` § 1). Collider outlines are the separate
//! `PhysicsGizmos` overlay.
//!
//! Provenance: reads rapier3d's public contact manifolds, joint anchors and center of mass
//! (Apache-2.0); the drawing is original.

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use tracing::debug_span;

use crate::convert::from_rapier_pose;
use crate::world::PhysicsWorld;

/// Line color of a contact normal (linear RGB).
pub const CONTACT_COLOR: [f32; 3] = [1.0, 0.25, 0.1];
/// Line color of a joint (body to anchor and the gap between the two anchors).
pub const JOINT_COLOR: [f32; 3] = [0.2, 0.6, 1.0];
/// Line color of a center-of-mass cross.
pub const COM_COLOR: [f32; 3] = [1.0, 0.9, 0.2];

/// Length of a drawn contact normal, meters.
pub const CONTACT_NORMAL_LENGTH: f32 = 0.25;
/// Half-length of each arm of a center-of-mass cross, meters.
pub const COM_CROSS_HALF: f32 = 0.08;
/// Most lines one frame collects; the rest are counted in [`PhysicsDebugStats::dropped`].
pub const MAX_DEBUG_LINES: usize = 50_000;

/// What the physics debug draw shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicsDebugKinds {
    /// Contact points with their normals.
    pub contacts: bool,
    /// Joints: body to anchor on both sides, and the gap between the anchors.
    pub joints: bool,
    /// Centers of mass of dynamic bodies.
    pub centers_of_mass: bool,
}

impl Default for PhysicsDebugKinds {
    /// Everything on.
    fn default() -> Self {
        Self::ALL
    }
}

impl PhysicsDebugKinds {
    /// Every kind.
    pub const ALL: Self = Self {
        contacts: true,
        joints: true,
        centers_of_mass: true,
    };

    /// No kind.
    pub const NONE: Self = Self {
        contacts: false,
        joints: false,
        centers_of_mass: false,
    };

    /// Parses a kind name: `contacts`, `joints` or `com` (alias `centers_of_mass`); sets it on
    /// `self`. Returns false for an unknown name.
    pub fn enable(&mut self, name: &str) -> bool {
        match name {
            "contacts" => self.contacts = true,
            "joints" => self.joints = true,
            "com" | "centers_of_mass" => self.centers_of_mass = true,
            _ => return false,
        }
        true
    }

    /// The enabled kinds' names, in a fixed order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contacts {
            out.push("contacts");
        }
        if self.joints {
            out.push("joints");
        }
        if self.centers_of_mass {
            out.push("com");
        }
        out
    }
}

/// One debug line segment in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicsDebugLine {
    /// Start.
    pub from: Vec3,
    /// End.
    pub to: Vec3,
    /// Linear RGB.
    pub color: [f32; 3],
}

/// Counters of the debug draw (tests and the agent read these).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhysicsDebugStats {
    /// Times the collection ran. Stays 0 while the draw is off.
    pub collections: u64,
    /// Lines collected over all runs.
    pub lines_emitted: u64,
    /// Lines left out because a frame hit [`MAX_DEBUG_LINES`].
    pub dropped: u64,
}

/// The physics debug draw switch and its last collected lines.
#[derive(Resource, Debug, Default, Clone)]
pub struct PhysicsDebugDraw {
    on: bool,
    kinds: PhysicsDebugKinds,
    lines: Vec<PhysicsDebugLine>,
    stats: PhysicsDebugStats,
}

impl PhysicsDebugDraw {
    /// Turns the draw on or off; turning it off drops the collected lines.
    pub fn set_on(&mut self, on: bool) {
        self.on = on;
        if !on {
            self.lines.clear();
        }
    }

    /// Whether the draw is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Chooses what is drawn.
    pub fn set_kinds(&mut self, kinds: PhysicsDebugKinds) {
        self.kinds = kinds;
    }

    /// What is drawn.
    #[must_use]
    pub fn kinds(&self) -> PhysicsDebugKinds {
        self.kinds
    }

    /// The lines of the last collection.
    #[must_use]
    pub fn lines(&self) -> &[PhysicsDebugLine] {
        &self.lines
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> PhysicsDebugStats {
        self.stats
    }
}

/// Run condition: the debug draw is on.
#[must_use]
pub fn physics_debug_on(draw: Option<Res<PhysicsDebugDraw>>) -> bool {
    draw.is_some_and(|d| d.on)
}

/// Pushes `line` unless the frame is full.
fn push(lines: &mut Vec<PhysicsDebugLine>, dropped: &mut u64, from: Vec3, to: Vec3, color: [f32; 3]) {
    if lines.len() >= MAX_DEBUG_LINES {
        *dropped += 1;
    } else {
        lines.push(PhysicsDebugLine { from, to, color });
    }
}

/// Collects the lines of `kinds` for `world` into `lines` (cleared first); returns how many were
/// dropped at the cap. Pure over the rapier world: nothing is stepped or written.
pub fn collect_lines(
    world: &PhysicsWorld,
    kinds: PhysicsDebugKinds,
    lines: &mut Vec<PhysicsDebugLine>,
) -> u64 {
    lines.clear();
    let mut dropped = 0;
    let to_vec = crate::convert::from_rapier_vec3;
    if kinds.contacts {
        for pair in world.narrow_phase.contact_pairs() {
            let Some(collider) = world.colliders.get(pair.collider1) else {
                continue;
            };
            let (t, r) = from_rapier_pose(collider.position());
            for manifold in pair.manifolds() {
                let normal = to_vec(manifold.data.normal);
                for point in &manifold.points {
                    if point.dist > 0.0 {
                        continue; // predicted, not touching
                    }
                    let at = t + r * to_vec(point.local_p1);
                    push(
                        lines,
                        &mut dropped,
                        at,
                        at + normal * CONTACT_NORMAL_LENGTH,
                        CONTACT_COLOR,
                    );
                }
            }
        }
    }
    if kinds.joints {
        for (_, joint) in world.impulse_joints.iter() {
            let (Some(a), Some(b)) = (world.bodies.get(joint.body1()), world.bodies.get(joint.body2()))
            else {
                continue;
            };
            let (ta, ra) = from_rapier_pose(a.position());
            let (tb, rb) = from_rapier_pose(b.position());
            let anchor_a = ta + ra * to_vec(joint.data.local_anchor1());
            let anchor_b = tb + rb * to_vec(joint.data.local_anchor2());
            push(lines, &mut dropped, ta, anchor_a, JOINT_COLOR);
            push(lines, &mut dropped, tb, anchor_b, JOINT_COLOR);
            push(lines, &mut dropped, anchor_a, anchor_b, JOINT_COLOR);
        }
    }
    if kinds.centers_of_mass {
        for (_, body) in world.bodies.iter() {
            if !body.is_dynamic() {
                continue;
            }
            let c = to_vec(body.center_of_mass());
            for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                push(
                    lines,
                    &mut dropped,
                    c - axis * COM_CROSS_HALF,
                    c + axis * COM_CROSS_HALF,
                    COM_COLOR,
                );
            }
        }
    }
    dropped
}

/// Rebuilds [`PhysicsDebugDraw`]'s lines (`PostUpdate`, only while the draw is on).
pub fn collect_physics_debug(world: Res<PhysicsWorld>, mut draw: ResMut<PhysicsDebugDraw>) {
    let _span = debug_span!(target: "regia_physics", "physics_debug_collect").entered();
    let draw = &mut *draw;
    let dropped = collect_lines(&world, draw.kinds, &mut draw.lines);
    draw.stats.collections += 1;
    draw.stats.lines_emitted += draw.lines.len() as u64;
    draw.stats.dropped += dropped;
}
