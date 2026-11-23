// SPDX-License-Identifier: MPL-2.0
//! A headless drop test of a ragdoll profile (ADR-2133): the physics-asset editor's preview.
//!
//! [`preview_drop`] builds a private app (physics, animation, the ragdoll plugin and a floor),
//! places the skeleton so its largest shape extent sits `drop_height` above the floor (the joint
//! centre is `drop_height` plus that extent, after the lowest bind-pose Y is subtracted), lets the
//! ragdoll go limp and runs
//! whole fixed steps. It reports whether and when the body came to rest, how fast any body moved,
//! how far a body sank below the floor, and the final pose of every body. Nothing of the caller's
//! world is touched, and the same profile, skeleton and options give the same result bit for bit
//! ([`RagdollPreview::digest`]).

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::{AnimationClip, AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::{Collider, ColliderShape, PhysicsPose, RigidBody, Velocity};
use regia_scene::Transform;
use thiserror::Error;

use super::{Ragdoll, RagdollBodies, RagdollMode, RagdollPlugin, RagdollProfile, RagdollSettings};

/// The most fixed steps one preview runs.
pub const MAX_PREVIEW_STEPS: u32 = 4000;

/// The preview's fixed step rate, steps per second.
pub const PREVIEW_HZ: f64 = 62.5;

/// Kinematic steps before the ragdoll goes limp (the bodies are built in the first).
const WARM_STEPS: u32 = 3;

/// What a preview runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RagdollPreviewOptions {
    /// Fixed steps after the ragdoll goes limp, `1..=MAX_PREVIEW_STEPS` (default 250, 4 s).
    pub steps: u32,
    /// Metres from the floor's top to the lowest joint's largest shape extent, `0..=20` (default 0.3).
    /// The joint centre is this far plus that extent.
    pub drop_height: f32,
    /// A push on the root body at release, N·s (default zero).
    pub push: Vec3,
}

impl Default for RagdollPreviewOptions {
    fn default() -> Self {
        Self {
            steps: 250,
            drop_height: 0.3,
            push: Vec3::ZERO,
        }
    }
}

/// Why a preview could not run. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum RagdollPreviewError {
    /// Options out of range.
    #[error("invalid preview options: {0}")]
    BadOptions(&'static str),
    /// No bone of the profile names a joint of the skeleton.
    #[error("no bone of the profile names a joint of the skeleton")]
    NoBodies,
}

impl RagdollPreviewError {
    /// The stable lowercase reason code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadOptions(_) => "ragdoll_preview_bad_options",
            Self::NoBodies => "ragdoll_preview_no_bodies",
        }
    }
}

/// One body at the end of a preview.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollPreviewBody {
    /// The skeleton joint.
    pub joint: String,
    /// Index of the nearest simulated ancestor in [`RagdollPreview::bodies`].
    pub parent: Option<usize>,
    /// World position (the floor's top is `y = 0`, the rig stands at the origin).
    pub position: Vec3,
    /// World rotation.
    pub rotation: Quat,
}

/// What a preview saw.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollPreview {
    /// Limp steps run.
    pub steps: u32,
    /// The limp step at which the ragdoll settled, if it did.
    pub settled_at: Option<u32>,
    /// Whether it lies face up, once settled.
    pub face_up: Option<bool>,
    /// The fastest any body moved, m/s.
    pub max_speed: f32,
    /// The lowest body centre, metres (a centre below 0 sank into the floor).
    pub lowest: f32,
    /// Every pose and speed stayed finite (false: the simulation blew up).
    pub finite: bool,
    /// The bodies at the end, parents before children.
    pub bodies: Vec<RagdollPreviewBody>,
    /// FNV-1a over every body's final pose bits: equal runs give equal digests.
    pub digest: u64,
}

/// Drops `profile` on `skeleton` onto a floor and simulates it limp (module docs). Errors: options
/// out of range, or a profile with no bone on the skeleton.
pub fn preview_drop(
    skeleton: Arc<Skeleton>,
    profile: &RagdollProfile,
    options: &RagdollPreviewOptions,
) -> Result<RagdollPreview, RagdollPreviewError> {
    if !(1..=MAX_PREVIEW_STEPS).contains(&options.steps) {
        return Err(RagdollPreviewError::BadOptions("steps must be within 1..=4000"));
    }
    if !(options.drop_height.is_finite() && (0.0..=20.0).contains(&options.drop_height)) {
        return Err(RagdollPreviewError::BadOptions(
            "drop_height must be within 0..=20 m",
        ));
    }
    if !(options.push.is_finite() && options.push.length() <= 1.0e4) {
        return Err(RagdollPreviewError::BadOptions(
            "push must be finite and at most 10000 N·s",
        ));
    }
    if !profile
        .bones
        .iter()
        .any(|b| skeleton.joint_names.contains(&b.joint))
    {
        return Err(RagdollPreviewError::NoBodies);
    }
    let _span = tracing::info_span!("ragdoll_preview_drop", steps = options.steps).entered();
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / PREVIEW_HZ,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = PREVIEW_HZ;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins((AnimationPlugin, RagdollPlugin))
        .insert_resource(RagdollSettings::default());
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(50.0, 0.5, 50.0),
            },
            ..Collider::default()
        },
    ));
    let lowest_bind = skeleton
        .bind_globals()
        .iter()
        .map(|g| g.w_axis.y)
        .fold(f32::INFINITY, f32::min);
    let lowest_bind = if lowest_bind.is_finite() { lowest_bind } else { 0.0 };
    let reach = profile
        .bones
        .iter()
        .map(|b| b.shape.extent(b.radius))
        .fold(0.0f32, f32::max);
    let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
    let rig = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, options.drop_height - lowest_bind + reach, 0.0)),
            binding,
            Animator::single(AnimationClip::empty("bind", 1.0), true),
            Ragdoll::new(profile.clone()),
        ))
        .id();
    for _ in 0..WARM_STEPS {
        app.update();
    }
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.mode = RagdollMode::Limp;
    }
    app.update();
    if options.push != Vec3::ZERO {
        let root = app
            .world()
            .get::<RagdollBodies>(rig)
            .and_then(|b| b.bodies.first().map(|b| b.entity));
        if let Some(root) = root
            && let Some(mut velocity) = app.world_mut().get_mut::<Velocity>(root)
        {
            let mass: f32 = profile.bones.first().map_or(1.0, |b| b.mass.max(1.0e-3));
            velocity.linear += options.push / mass;
        }
    }
    let mut out = RagdollPreview {
        steps: options.steps,
        settled_at: None,
        face_up: None,
        max_speed: 0.0,
        lowest: f32::INFINITY,
        finite: true,
        bodies: Vec::new(),
        digest: 0,
    };
    for step in 1..=options.steps {
        app.update();
        let world = app.world();
        let Some(bodies) = world.get::<RagdollBodies>(rig) else {
            continue;
        };
        for body in &bodies.bodies {
            if let Some(v) = world.get::<Velocity>(body.entity) {
                let speed = v.linear.length();
                out.finite &= speed.is_finite() && v.angular.is_finite();
                if speed.is_finite() {
                    out.max_speed = out.max_speed.max(speed);
                }
            }
        }
        if bodies.settled() && out.settled_at.is_none() {
            out.settled_at = Some(step);
            out.face_up = Some(bodies.face_up());
        }
    }
    let world = app.world();
    if let Some(bodies) = world.get::<RagdollBodies>(rig) {
        for body in &bodies.bodies {
            let (position, rotation) = world
                .get::<PhysicsPose>(body.entity)
                .map(|p| (p.translation, p.rotation))
                .or_else(|| {
                    world
                        .get::<Transform>(body.entity)
                        .map(|t| (t.translation, t.rotation))
                })
                .unwrap_or((Vec3::NAN, Quat::IDENTITY));
            out.finite &= position.is_finite() && rotation.is_finite();
            if position.y.is_finite() {
                out.lowest = out.lowest.min(position.y);
            }
            out.bodies.push(RagdollPreviewBody {
                joint: skeleton
                    .joint_names
                    .get(body.joint as usize)
                    .cloned()
                    .unwrap_or_default(),
                parent: body.parent,
                position,
                rotation,
            });
        }
    }
    if out.bodies.is_empty() {
        return Err(RagdollPreviewError::NoBodies);
    }
    out.digest = digest(&out.bodies);
    Ok(out)
}

fn digest(bodies: &[RagdollPreviewBody]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bodies {
        let p = b.position;
        let q = b.rotation;
        for c in [p.x, p.y, p.z, q.x, q.y, q.z, q.w] {
            for byte in c.to_bits().to_le_bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}
