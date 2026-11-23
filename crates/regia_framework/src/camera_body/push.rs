// SPDX-License-Identifier: MPL-2.0
//! The wake as a drag impulse on loose rigid bodies (ADR-1502).
//!
//! A body of reference radius `a` in a gust `u` feels the drag `F = ½ ρ C_d (π a²) |u|² û`
//! (the quadratic drag law). Each fixed step it gets `F · dt` as an
//! [`ExternalImpulse`], added to any impulse already queued.
//! Only dynamic bodies are pushed; kinematic and static bodies are left alone. Each body's
//! impulse depends only on its own position and the wake, so the result does not depend on the
//! query's iteration order.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{BodyKind, Collider, ColliderShape, ExternalImpulse, RigidBody};
use regia_scene::GlobalTransform;
use serde::{Deserialize, Serialize};

use super::CameraBody;

/// How loose rigid bodies take the wake.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PushSpec {
    /// Push dynamic rigid bodies at all.
    pub enabled: bool,
    /// Air density `ρ`, kg/m³ (1.225 at sea level, 15 °C).
    pub air_density: f32,
    /// Drag coefficient `C_d` (about 1 for a tumbling chunk; 0.47 for a sphere).
    pub drag_coefficient: f32,
    /// Reference radius of a body without a collider, metres.
    pub default_radius_m: f32,
    /// Largest impulse one body gets in one step, N·s.
    pub max_impulse: f32,
    /// Scales the drag (1 = the drag law as written).
    pub scale: f32,
}

impl Default for PushSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            air_density: 1.225,
            drag_coefficient: 1.0,
            default_radius_m: 0.25,
            max_impulse: 200.0,
            scale: 1.0,
        }
    }
}

impl PushSpec {
    /// Every value clamped into range (non-finite values take the default).
    #[must_use]
    pub fn clamped(self) -> Self {
        let d = Self::default();
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        Self {
            enabled: self.enabled,
            air_density: fix(self.air_density, 0.0, 10.0, d.air_density),
            drag_coefficient: fix(self.drag_coefficient, 0.0, 4.0, d.drag_coefficient),
            default_radius_m: fix(self.default_radius_m, 0.01, 50.0, d.default_radius_m),
            max_impulse: fix(self.max_impulse, 0.0, 1.0e6, d.max_impulse),
            scale: fix(self.scale, 0.0, 100.0, d.scale),
        }
    }

    /// The impulse a body of reference radius `radius` gets from `gust` over `dt` seconds,
    /// N·s (capped at [`Self::max_impulse`]).
    #[must_use]
    pub fn impulse(&self, gust: Vec3, radius: f32, dt: f32) -> Vec3 {
        let speed = gust.length();
        if !self.enabled || speed <= 0.0 || dt <= 0.0 {
            return Vec3::ZERO;
        }
        let area = std::f32::consts::PI * radius * radius;
        let force = 0.5 * self.air_density * self.drag_coefficient * area * speed * speed * self.scale;
        let j = (force * dt).min(self.max_impulse);
        gust / speed * j
    }
}

/// A body's reference radius for drag: the radius of a sphere of about the same frontal area.
#[must_use]
pub fn reference_radius(shape: &ColliderShape, fallback: f32) -> f32 {
    match shape {
        ColliderShape::Sphere { radius } => *radius,
        ColliderShape::Box { half_extents } => {
            (half_extents.x * half_extents.y * half_extents.z).abs().cbrt()
        }
        ColliderShape::Capsule {
            half_height, radius, ..
        }
        | ColliderShape::Cylinder {
            half_height, radius, ..
        }
        | ColliderShape::Cone {
            half_height, radius, ..
        } => (radius * (half_height + radius)).abs().sqrt(),
        _ => fallback,
    }
}

/// A rigid body the wake may push, and any impulse already queued on it.
type PushedBody = (
    Entity,
    &'static RigidBody,
    &'static GlobalTransform,
    Option<&'static Collider>,
    Option<&'static mut ExternalImpulse>,
);

/// Pushes dynamic bodies in range with the wake (before the physics step).
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_body_push")]
pub(super) fn push_rigid_bodies(
    mut commands: Commands,
    fixed: Option<Res<FixedTime>>,
    mut body: ResMut<CameraBody>,
    mut bodies: Query<PushedBody>,
) {
    let push = body.spec.push;
    let Some(state) = body.state().filter(|_| push.enabled) else {
        body.report.pushed_bodies = 0;
        body.report.pushed_impulse = 0.0;
        return;
    };
    let dt = fixed.map_or(1.0 / 60.0, |f| f.step().as_secs_f32());
    let shape = body.spec.wake;
    let mut count = 0u32;
    let mut total = 0.0f32;
    for (entity, rigid, transform, collider, queued) in &mut bodies {
        if rigid.kind != BodyKind::Dynamic {
            continue;
        }
        let gust = shape.sample(&state, transform.translation()).gust;
        if gust == Vec3::ZERO {
            continue;
        }
        let radius = collider.map_or(push.default_radius_m, |c| {
            reference_radius(&c.shape, push.default_radius_m)
        });
        let j = push.impulse(gust, radius, dt);
        if j == Vec3::ZERO {
            continue;
        }
        count += 1;
        total += j.length();
        match queued {
            Some(mut queued) => queued.impulse += j,
            None => {
                commands.entity(entity).insert(ExternalImpulse {
                    impulse: j,
                    torque_impulse: Vec3::ZERO,
                });
            }
        }
    }
    body.report.pushed_bodies = count;
    body.report.pushed_impulse = total;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_impulse_follows_the_quadratic_law() {
        let push = PushSpec::default();
        let a = push.impulse(Vec3::new(4.0, 0.0, 0.0), 0.5, 1.0 / 60.0);
        let b = push.impulse(Vec3::new(8.0, 0.0, 0.0), 0.5, 1.0 / 60.0);
        assert!((b.x / a.x - 4.0).abs() < 1.0e-4);
        // ½ · 1.225 · 1 · π · 0.25 · 16 / 60
        let expected = 0.5 * 1.225 * std::f32::consts::PI * 0.25 * 16.0 / 60.0;
        assert!((a.x - expected).abs() < 1.0e-5, "{a}");
        assert_eq!(push.impulse(Vec3::ZERO, 0.5, 1.0 / 60.0), Vec3::ZERO);
        let off = PushSpec {
            enabled: false,
            ..push
        };
        assert_eq!(off.impulse(Vec3::X, 0.5, 1.0 / 60.0), Vec3::ZERO);
        let capped = PushSpec {
            max_impulse: 0.01,
            ..push
        };
        assert!((capped.impulse(Vec3::X * 100.0, 1.0, 1.0).length() - 0.01).abs() < 1.0e-6);
    }

    #[test]
    fn reference_radius_of_common_shapes() {
        assert_eq!(reference_radius(&ColliderShape::Sphere { radius: 0.3 }, 1.0), 0.3);
        let cube = ColliderShape::Box {
            half_extents: Vec3::splat(0.5),
        };
        assert!((reference_radius(&cube, 1.0) - 0.5).abs() < 1.0e-6);
    }
}
