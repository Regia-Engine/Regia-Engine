// SPDX-License-Identifier: MPL-2.0
//! Transform sync, interpolation and debris cleanup systems.
//!
//! [`interpolate_transforms`] runs in `PostUpdate/PhysicsSync` and blends the rendered
//! `Transform` of dynamic bodies between the two most recent fixed-step poses using
//! `FixedTime::overstep_fraction()` (spec 15 § 1; per-body opt-out: [`NoInterpolation`],
//! and teleports pin both poses). The debris systems are the M14 lifecycle: spawn times,
//! lifetime expiry and the FIFO body budget.
//!
//! The interpolation must run **after** the fixed step wrote the raw stepped pose into
//! `Transform` (the schedule order guarantees this: `FixedUpdate` runs before `PostUpdate`).

use regia_app::FixedTime;
use regia_ecs::prelude::*;
use regia_scene::GlobalTransform;
use tracing::instrument;

use crate::components::{DebrisTag, NoInterpolation, PhysicsPose, PreviousPhysicsPose, RigidBodyHandle};
use crate::world::{BodyBudget, PhysicsSettings, PhysicsWorld};

/// Whether a chunk stamped at `spawn` (physics clock) is past `lifetime` seconds at `now`. The
/// step length is a whole number of nanoseconds (1/60 s rounds to 16 666 667 ns), so sixty steps
/// are 20 ns longer than a second; a tolerance of a millionth of the lifetime (at least 1 µs)
/// absorbs that rounding, so a 1 s chunk at 60 Hz lives exactly 60 steps after its first.
pub(crate) fn debris_expired(now: f64, spawn: f64, lifetime: f64) -> bool {
    // A NaN lifetime would never compare true; treat it as already expired.
    lifetime.is_nan() || now - spawn > lifetime + (lifetime.abs() * 1e-6).max(1e-6)
}

/// Records the spawn time of new debris that no fixed step has simulated yet.
///
/// Debris lifetimes run on the physics clock ([`PhysicsWorld::fixed_elapsed_secs`]), and a
/// chunk's clock starts at the end of the first step that simulates it: `step_physics` stamps the
/// chunks it steps, and this system stamps the others with the end of the next step. Either way
/// the chunk leaves on the same step at any frame rate (ADR-1660). Runs before
/// [`despawn_expired_debris`].
#[instrument(skip_all, target = "regia_physics")]
pub fn init_debris_spawn_times(
    fixed: Option<Res<FixedTime>>,
    physics: Option<Res<PhysicsWorld>>,
    mut query: Query<&mut DebrisTag, (With<RigidBodyHandle>, Added<DebrisTag>)>,
) {
    let _span = tracing::debug_span!("init_debris_spawn_times").entered();
    let Some(physics) = physics else {
        return;
    };
    let step = fixed.map(|f| f.step()).unwrap_or_default();
    let next_step_end = physics.fixed_elapsed.saturating_add(step).as_secs_f64();
    for mut tag in &mut query {
        if tag.spawn_time_secs.is_none() {
            tag.spawn_time_secs = Some(next_step_end);
        }
    }
}

/// Checks all debris entities for lifetime expiry on the physics clock
/// ([`PhysicsWorld::fixed_elapsed_secs`]) and despawns them. `step_physics` already expires
/// chunks on the step their lifetime ends; this catches chunks it does not see.
#[instrument(skip_all, target = "regia_physics")]
pub fn despawn_expired_debris(
    query: Query<(Entity, &RigidBodyHandle, &DebrisTag)>,
    mut physics: Option<ResMut<PhysicsWorld>>,
    mut budget: ResMut<BodyBudget>,
    mut commands: Commands,
) {
    let _span = tracing::debug_span!("despawn_expired_debris").entered();
    let Some(ref mut pw) = physics else {
        return;
    };
    let now = pw.fixed_elapsed_secs();
    let mut expired = Vec::new();
    for (entity, marker, tag) in &query {
        let Some(spawn_time) = tag.spawn_time_secs else {
            // Spawn time not yet recorded — skip (init_debris_spawn_times will set it soon).
            continue;
        };
        if debris_expired(now, spawn_time, tag.lifetime_secs) {
            budget.remove(entity);
            expired.push((entity, marker.to_handle()));
        }
    }
    // Rapier reuses freed handles in removal order, so this order must not follow the query's,
    // nor entity ids: handle order is insertion order (ADR-2396).
    expired.sort_by_key(|row| row.1.into_raw_parts());
    for (entity, handle) in expired {
        pw.remove_body(handle);
        commands.entity(entity).despawn();
    }
}

/// Enforces the body budget by despawning oldest debris exceeding the cap.
#[instrument(skip_all, target = "regia_physics")]
pub fn enforce_body_budget(
    settings: Res<PhysicsSettings>,
    mut budget: ResMut<BodyBudget>,
    mut physics: Option<ResMut<PhysicsWorld>>,
    debris_query: Query<&RigidBodyHandle, With<DebrisTag>>,
    mut commands: Commands,
) {
    let _span = tracing::debug_span!("enforce_body_budget").entered();
    if settings.max_debris_bodies == 0 {
        return;
    }
    let overflow = budget.enforce(settings.max_debris_bodies);
    if let Some(ref mut pw) = physics {
        for entity in &overflow {
            if let Ok(marker) = debris_query.get(*entity) {
                pw.remove_body(marker.to_handle());
            }
        }
    }
    for entity in overflow {
        commands.entity(entity).despawn();
    }
}

/// The bodies `interpolate_transforms` writes: both poses, the rendered transform, the opt-out
/// marker and the parent (a parented body's `Transform` is local).
type InterpolatedBodiesQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static PhysicsPose,
        &'static PreviousPhysicsPose,
        &'static mut regia_scene::Transform,
        Option<&'static NoInterpolation>,
        Option<&'static ChildOf>,
    ),
>;

/// Blends the rendered `Transform` of dynamic bodies between their previous and current
/// fixed-step poses (spec 15 § 1 interpolation).
///
/// Runs in `PostUpdate/PhysicsSync` every frame — including frames where no fixed step
/// ran, where the blend simply advances from the previous pose towards the current one.
/// Opt out per body with [`NoInterpolation`]; [`Teleport`](crate::components::Teleport)
/// pins both poses so there is no blend across a teleport.
#[instrument(skip_all, target = "regia_physics")]
pub fn interpolate_transforms(
    fixed: Res<FixedTime>,
    mut bodies: InterpolatedBodiesQuery,
    parents: Query<&'static GlobalTransform>,
) {
    let _span = tracing::debug_span!("interpolate_transforms").entered();
    let alpha = fixed.overstep_fraction();
    for (pose, previous, mut transform, opt_out, parent) in &mut bodies {
        if opt_out.is_some() {
            continue;
        }
        // A body at rest (asleep, or not moved this step) blends to its own pose: skip the
        // slerp and leave the component unchanged, so thousands of sleeping bodies are not
        // flagged as changed every frame.
        let world = if previous.translation == pose.translation && previous.rotation == pose.rotation {
            (pose.translation, pose.rotation)
        } else {
            (
                previous.translation.lerp(pose.translation, alpha),
                previous.rotation.slerp(pose.rotation, alpha),
            )
        };
        // The poses are world-space; a parented body's `Transform` is local to its parent.
        let parent_global = parent.and_then(|p| parents.get(p.parent()).ok());
        // A singular parent (zero scale) has no local pose: keep the `Transform` as it was.
        let Some((translation, rotation)) = crate::step::local_pose(world, parent_global) else {
            continue;
        };
        if transform.translation != translation || transform.rotation != rotation {
            transform.translation = translation;
            transform.rotation = rotation;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_nan_debris_lifetime_is_expired() {
        assert!(super::debris_expired(0.0, 0.0, f64::NAN));
        assert!(!super::debris_expired(1e9, 0.0, f64::INFINITY));
    }

    use super::*;
    use regia_app::prelude::*;
    use regia_core::math::{Quat, Vec3};
    use regia_scene::Transform;

    /// An app with the interpolation system only (no full plugin — isolated).
    fn app_with_poses() -> App {
        let mut app = App::new();
        app.add_systems(PostUpdate, interpolate_transforms);
        app.insert_resource(FixedTime::default());
        app
    }

    fn spawn_posed(app: &mut App, prev: Vec3, current: Vec3) -> Entity {
        app.world_mut()
            .spawn((
                Transform {
                    translation: current,
                    ..Default::default()
                },
                PhysicsPose {
                    translation: current,
                    rotation: Quat::IDENTITY,
                },
                PreviousPhysicsPose {
                    translation: prev,
                    rotation: Quat::IDENTITY,
                },
            ))
            .id()
    }

    #[test]
    fn interpolation_blends_by_overstep_fraction() {
        let mut app = app_with_poses();
        let e = spawn_posed(&mut app, Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0));
        // Half a step over: alpha 0.5 → halfway between the poses.
        let mut fixed = FixedTime::default();
        fixed.accumulate(std::time::Duration::from_secs_f64(0.5 / fixed.hz));
        app.insert_resource(fixed);
        app.update();
        let t = app.world().get::<Transform>(e).expect("transform").translation;
        assert!((t.x - 5.0).abs() < 1e-4, "halfway blend: {t}");
    }

    #[test]
    fn interpolation_of_a_parented_body_writes_a_local_transform() {
        let mut app = app_with_poses();
        let parent = app
            .world_mut()
            .spawn(GlobalTransform(regia_core::math::Affine3A::from_translation(
                Vec3::new(10.0, 0.0, 0.0),
            )))
            .id();
        let e = spawn_posed(&mut app, Vec3::new(12.0, 0.0, 0.0), Vec3::new(12.0, 0.0, 0.0));
        app.world_mut().entity_mut(parent).add_child(e);
        app.update();
        let t = app.world().get::<Transform>(e).expect("transform").translation;
        assert!(
            (t - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-4,
            "local pose under the parent: {t}"
        );
    }

    #[test]
    fn interpolation_under_a_scaled_parent_keeps_the_body_rotation() {
        let mut app = app_with_poses();
        let parent = app
            .world_mut()
            .spawn(GlobalTransform(
                regia_core::math::Affine3A::from_scale_rotation_translation(
                    Vec3::splat(2.0),
                    Quat::IDENTITY,
                    Vec3::new(10.0, 0.0, 0.0),
                ),
            ))
            .id();
        let spin = Quat::from_rotation_y(0.7);
        let pose = (Vec3::new(12.0, 0.0, 0.0), spin);
        let e = app
            .world_mut()
            .spawn((
                Transform::default(),
                PhysicsPose {
                    translation: pose.0,
                    rotation: pose.1,
                },
                PreviousPhysicsPose {
                    translation: pose.0,
                    rotation: pose.1,
                },
            ))
            .id();
        app.world_mut().entity_mut(parent).add_child(e);
        app.update();
        let t = *app.world().get::<Transform>(e).expect("transform");
        // World x 12 is 2 m past the parent, which scales by 2: local x 1.
        assert!(
            (t.translation - Vec3::new(1.0, 0.0, 0.0)).length() < 1e-4,
            "{:?}",
            t.translation
        );
        assert!(t.rotation.angle_between(spin) < 1e-4, "{:?}", t.rotation);
    }

    /// Audit v033b (High): under a parent whose scale is zero, the inverse parent matrix is NaN
    /// and the interpolated local pose was written into the `Transform`. It keeps its value.
    #[test]
    fn interpolation_under_a_zero_scale_parent_keeps_the_transform() {
        let mut app = app_with_poses();
        let parent = app
            .world_mut()
            .spawn(GlobalTransform(regia_core::math::Affine3A::from_scale(
                Vec3::ZERO,
            )))
            .id();
        let e = spawn_posed(&mut app, Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 0.0, 0.0));
        app.world_mut().entity_mut(parent).add_child(e);
        let mut fixed = FixedTime::default();
        fixed.accumulate(std::time::Duration::from_secs_f64(0.5 / fixed.hz));
        app.insert_resource(fixed);
        app.update();
        let t = *app.world().get::<Transform>(e).expect("transform");
        assert!(
            t.translation.is_finite() && t.rotation.is_finite(),
            "no NaN from a singular parent: {t:?}"
        );
        assert_eq!(
            t.translation,
            Vec3::new(2.0, 0.0, 0.0),
            "the Transform is unchanged"
        );
    }

    #[test]
    fn interpolation_opt_out_keeps_stepped_pose() {
        let mut app = app_with_poses();
        let e = app
            .world_mut()
            .spawn((
                Transform {
                    translation: Vec3::new(10.0, 0.0, 0.0),
                    ..Default::default()
                },
                PhysicsPose {
                    translation: Vec3::new(10.0, 0.0, 0.0),
                    rotation: Quat::IDENTITY,
                },
                PreviousPhysicsPose {
                    translation: Vec3::ZERO,
                    rotation: Quat::IDENTITY,
                },
                NoInterpolation,
            ))
            .id();
        let mut fixed = FixedTime::default();
        fixed.accumulate(std::time::Duration::from_secs_f64(0.5 / fixed.hz));
        app.insert_resource(fixed);
        app.update();
        let t = app.world().get::<Transform>(e).expect("transform").translation;
        assert!((t.x - 10.0).abs() < 1e-6, "opt-out keeps the pose: {t}");
    }

    #[test]
    fn debris_lifetime_absorbs_the_step_length_rounding() {
        // 1/60 s is 16 666 667 ns: sixty steps are 1.00000002 s, still within a 1 s lifetime.
        let step = std::time::Duration::from_secs_f64(1.0 / 60.0).as_secs_f64();
        let spawn = step;
        assert!(!debris_expired(spawn + 60.0 * step, spawn, 1.0));
        assert!(debris_expired(spawn + 61.0 * step, spawn, 1.0));
        assert!(!debris_expired(0.0, 0.0, 0.0));
        assert!(debris_expired(step, 0.0, 0.0));
    }
}
