// SPDX-License-Identifier: MPL-2.0
//! The `motion_matching` debug overlay (`docs/specs/52-motion-matching.md` § 2.11): for every
//! entity with a [`MotionMatching`] and a [`GlobalTransform`], the desired trajectory as a
//! polyline with facing ticks, the matched (playing) frame's own trajectory in gray, and the
//! playing frame's foot positions. Off by default; `debug.set_gizmos { "group":
//! "motion_matching", "enabled": true }` turns it on. Drawn by
//! [`crate::debug_gizmos::DebugGizmosPlugin`].
//!
//! The character frame is the entity's ground position (Y dropped) turned to its facing (the
//! transform's `+Z` projected on the ground), matching the database's character frame.

use regia_animation::motion_matching::{MotionMatching, TrajectoryPoint};
use regia_app::prelude::*;
use regia_core::math::{Color, Isometry, Quat, Vec2, Vec3};
use regia_render::gizmos::{GizmoBuffer, GizmoConfigGroup, Gizmos, MotionMatchingGizmos};
use regia_scene::GlobalTransform;

/// The desired trajectory.
pub const DESIRED_COLOR: Color = Color::rgb(0.2, 0.9, 1.0);
/// The playing frame's own trajectory.
pub const MATCHED_COLOR: Color = Color::rgb(0.55, 0.55, 0.55);
/// Foot positions of the playing frame.
pub const FOOT_COLOR: Color = Color::rgb(1.0, 0.6, 0.15);
/// Length of a facing tick, meters.
pub const TICK_LENGTH: f32 = 0.25;
/// Radius of a foot marker, meters.
pub const FOOT_RADIUS: f32 = 0.05;
/// Height above the ground the trajectories are drawn at, meters.
pub const LIFT: f32 = 0.02;

/// Draws every motion matcher (`PostUpdate`, when the group is enabled).
pub fn draw_motion_matching(
    matchers: Query<(&MotionMatching, &GlobalTransform)>,
    mut gizmos: Gizmos<MotionMatchingGizmos>,
) {
    for (matching, transform) in &matchers {
        let (_, rotation, translation) = transform.0.to_scale_rotation_translation();
        let facing = rotation * Vec3::Z;
        let yaw = if facing.x * facing.x + facing.z * facing.z > 1e-12 {
            facing.x.atan2(facing.z)
        } else {
            0.0
        };
        draw_matcher(
            &mut gizmos,
            matching,
            Vec3::new(translation.x, 0.0, translation.z),
            yaw,
        );
    }
}

/// Draws one matcher whose character frame sits at ground point `origin` with facing `yaw`
/// (radians about +Y, 0 = +Z). Returns nothing; draws nothing for an empty trajectory.
pub fn draw_matcher<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    matching: &MotionMatching,
    origin: Vec3,
    yaw: f32,
) {
    let turn = Quat::from_rotation_y(yaw);
    let world = |p: Vec2| origin + turn * Vec3::new(p.x, LIFT, p.y);
    let direction = |d: Vec2| turn * Vec3::new(d.x, 0.0, d.y);
    draw_path(gizmos, &matching.trajectory, &world, &direction, DESIRED_COLOR);
    let Some(playback) = matching.matcher.playback() else {
        return;
    };
    let Some(frame) = matching.database.frame_query(playback.frame) else {
        return;
    };
    draw_path(gizmos, &frame.trajectory, &world, &direction, MATCHED_COLOR);
    for foot in &frame.feet {
        let at = origin + turn * foot.position;
        gizmos
            .sphere(Isometry::from_translation(at), FOOT_RADIUS, FOOT_COLOR)
            .resolution(8);
    }
}

fn draw_path<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    points: &[TrajectoryPoint],
    world: &impl Fn(Vec2) -> Vec3,
    direction: &impl Fn(Vec2) -> Vec3,
    color: Color,
) {
    if points.is_empty() {
        return;
    }
    gizmos.linestrip(
        std::iter::once(world(Vec2::ZERO)).chain(points.iter().map(|p| world(p.position))),
        color,
    );
    for point in points {
        gizmos.ray(
            world(point.position),
            direction(point.direction) * TICK_LENGTH,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
    use regia_animation::motion_matching::{FeatureSchema, MatcherSettings, MotionClip, MotionDatabase};
    use regia_animation::pose::translated;
    use regia_animation::skeleton::Skeleton;
    use regia_render::gizmos::{DefaultGizmoConfigGroup, GizmoConfigStore, GizmoPlugin, GizmoStorage};

    use super::*;
    use crate::debug_gizmos::DebugGizmosPlugin;

    fn matching() -> MotionMatching {
        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "foot_l".into(), "foot_r".into()],
            vec![None, Some(0), Some(0)],
            vec![
                translated(Vec3::ZERO),
                translated(Vec3::new(-0.1, 0.0, 0.0)),
                translated(Vec3::new(0.1, 0.0, 0.0)),
            ],
        )
        .expect("skeleton");
        let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
        let mut clip = AnimationClip::empty("walk", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times.iter().map(|&t| Vec3::new(0.0, 0.0, 1.5 * t)).collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let database = MotionDatabase::build(
            &skeleton,
            &[MotionClip {
                clip: &clip,
                looping: true,
            }],
            FeatureSchema::new(0, vec![1, 2]),
        )
        .expect("database");
        let mut matching = MotionMatching::new(Arc::new(database), MatcherSettings::default());
        matching.trajectory = (1..=3).map(|i| TrajectoryPoint::ahead(0.5 * i as f32)).collect();
        matching
    }

    fn segments<G: GizmoConfigGroup>(b: &GizmoBuffer<G>) -> usize {
        let strips: usize = b
            .strip_positions
            .split(|p| p.is_nan())
            .map(|r| r.len().saturating_sub(1))
            .sum();
        b.list_positions.len() / 2 + strips
    }

    #[test]
    fn the_overlay_draws_both_paths_and_the_feet() {
        let mut m = matching();
        let mut before = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_matcher(&mut before, &m, Vec3::ZERO, 0.0);
        // Nothing plays yet: the desired path (3 segments) and its 3 facing ticks only.
        assert_eq!(segments(&before), 6);
        m.step(1.0 / 30.0).expect("step");
        let mut after = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_matcher(&mut after, &m, Vec3::new(2.0, 0.0, 1.0), 0.5);
        assert!(segments(&after) > 12, "matched path plus two foot spheres");
        // The desired path starts at the character's ground point.
        assert!(
            after
                .strip_positions
                .iter()
                .any(|p| (*p - Vec3::new(2.0, LIFT, 1.0)).length() < 1e-5)
        );
    }

    #[test]
    fn the_overlay_only_draws_when_enabled() {
        let mut app = App::new();
        app.add_plugins(GizmoPlugin).add_plugins(DebugGizmosPlugin);
        app.world_mut().spawn((matching(), GlobalTransform::IDENTITY));
        app.run_schedule(PostUpdate);
        assert!(
            app.world()
                .resource::<GizmoStorage<MotionMatchingGizmos>>()
                .is_empty()
        );
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("motion_matching", true);
        app.run_schedule(PostUpdate);
        assert!(
            !app.world()
                .resource::<GizmoStorage<MotionMatchingGizmos>>()
                .is_empty()
        );
    }
}
