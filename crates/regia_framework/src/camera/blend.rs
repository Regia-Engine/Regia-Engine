// SPDX-License-Identifier: MPL-2.0
//! `CameraDirector::blend_to` and one director per local player (spec 14 § B5, ADR-7203).
//!
//! A blend moves a camera from the pose it has when the blend starts to a target pose over
//! `duration` seconds; the eased fraction `curve(elapsed / duration)` picks the point between
//! the two (translation `lerp`, rotation `det_nlerp`). At `elapsed >= duration` the pose is the
//! target exactly. A pose target then holds until [`CameraDirector::clear_blend`] or the next
//! blend; an entity target keeps following that entity's [`Transform`] (it is the director's
//! view target).
//!
//! Which camera a director moves: the shared [`CameraDirector`] resource blends every
//! [`CameraRig`] or [`CinematicCamera`] camera that carries no [`PlayerCamera`]; local player
//! `n`'s director (on the [`PlayerManager`]) blends the cameras marked `PlayerCamera(n)`.
//! Blends run after the rigs and the cinematic players and before the shakes, so a shake still
//! shows on a blended camera. Player directors carry their own blends, FOV kick and shakes: a
//! `PlayerCamera(n)` camera feels only player `n`'s director shakes, the other cameras the shared
//! director's (ADR-7203, ADR-7263; see [`super::sensor`]).

use regia_app::prelude::*;
use regia_core::math::{Easing, Quat, Vec3, det_nlerp};
use regia_scene::Transform;

use super::{CameraDirector, CameraRig, CinematicCamera};
use crate::player::PlayerManager;

/// A camera pose: where it is and where it looks.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct CameraPose {
    /// World position.
    pub translation: Vec3,
    /// World rotation.
    pub rotation: Quat,
}

impl CameraPose {
    /// A pose at `translation` with the identity rotation (looking down −Z).
    #[must_use]
    pub fn at(translation: Vec3) -> Self {
        Self {
            translation,
            rotation: Quat::IDENTITY,
        }
    }

    /// The pose of a transform (scale is not part of a camera pose).
    #[must_use]
    pub fn of(transform: &Transform) -> Self {
        Self {
            translation: transform.translation,
            rotation: transform.rotation,
        }
    }
}

/// What a blend goes to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlendTarget {
    /// A fixed pose.
    Pose(CameraPose),
    /// Another entity's [`Transform`] (a camera, a marker), followed as it moves.
    Entity(Entity),
}

impl From<CameraPose> for BlendTarget {
    fn from(pose: CameraPose) -> Self {
        Self::Pose(pose)
    }
}

impl From<Entity> for BlendTarget {
    fn from(entity: Entity) -> Self {
        Self::Entity(entity)
    }
}

/// A running (or finished and holding) blend.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveBlend {
    target: BlendTarget,
    duration: f32,
    curve: Easing,
    elapsed: f32,
    /// Each camera's pose when the blend first moved it, by camera entity
    /// ([`Entity::PLACEHOLDER`] for the pure [`CameraDirector::blend_pose`] calls).
    from: Vec<(Entity, CameraPose)>,
}

impl ActiveBlend {
    /// Advances the clock; it stops at `duration`, so the end is exact.
    fn advance(&mut self, delta: f32) {
        if delta.is_finite() && delta > 0.0 {
            self.elapsed += delta;
        }
        if self.elapsed > self.duration {
            self.elapsed = self.duration;
        }
    }

    fn done(&self) -> bool {
        self.duration <= 0.0 || self.elapsed >= self.duration
    }

    /// The pose of `camera` (now at `current`) at the current clock.
    fn pose(&mut self, camera: Entity, current: CameraPose, target: CameraPose) -> CameraPose {
        let from = match self.from.iter().find(|(entity, _)| *entity == camera) {
            Some((_, pose)) => *pose,
            None => {
                self.from.push((camera, current));
                current
            }
        };
        if self.done() {
            return target;
        }
        let t = self.curve.apply(self.elapsed / self.duration);
        CameraPose {
            translation: from.translation.lerp(target.translation, t),
            rotation: det_nlerp(from.rotation, target.rotation, t),
        }
    }
}

impl CameraDirector {
    /// Blends the director's cameras to `target` over `duration` seconds along `curve`, each
    /// starting from wherever it is on the next blend pass. A new blend replaces a running one
    /// (it starts from the current, partly blended pose). A `duration` of zero or less, or one
    /// that is not finite, cuts to the target.
    pub fn blend_to(&mut self, target: impl Into<BlendTarget>, duration: f32, curve: Easing) {
        let duration = if duration.is_finite() {
            duration.max(0.0)
        } else {
            0.0
        };
        self.blend = Some(ActiveBlend {
            target: target.into(),
            duration,
            curve,
            elapsed: 0.0,
            from: Vec::new(),
        });
    }

    /// Whether a blend is running or holding its target.
    #[must_use]
    pub fn is_blending(&self) -> bool {
        self.blend.is_some()
    }

    /// The blend's time fraction `elapsed / duration` in `0..=1` (`1` once it reached the
    /// target), or `None` without a blend.
    #[must_use]
    pub fn blend_progress(&self) -> Option<f32> {
        self.blend.as_ref().map(|blend| {
            if blend.duration <= 0.0 {
                1.0
            } else {
                (blend.elapsed / blend.duration).clamp(0.0, 1.0)
            }
        })
    }

    /// The blend's target (the director's view target), or `None` without a blend.
    #[must_use]
    pub fn view_target(&self) -> Option<BlendTarget> {
        self.blend.as_ref().map(|blend| blend.target)
    }

    /// Ends the blend: the camera's own rig or player drives it again from the next frame.
    pub fn clear_blend(&mut self) {
        self.blend = None;
    }

    /// Advances the blend by `delta` seconds and returns this frame's pose of a camera now at
    /// `current` going to a pose target (the start pose is the `current` of the first call).
    /// `None` without a blend, or when the target is an entity (use [`Self::blend_pose_towards`]).
    pub fn blend_pose(&mut self, current: CameraPose, delta: f32) -> Option<CameraPose> {
        let BlendTarget::Pose(target) = self.blend.as_ref()?.target else {
            return None;
        };
        self.blend_pose_towards(current, target, delta)
    }

    /// Like [`Self::blend_pose`] with the target pose given (an entity target's pose this frame).
    pub fn blend_pose_towards(
        &mut self,
        current: CameraPose,
        target: CameraPose,
        delta: f32,
    ) -> Option<CameraPose> {
        let blend = self.blend.as_mut()?;
        blend.advance(delta);
        Some(blend.pose(Entity::PLACEHOLDER, current, target))
    }
}

/// Marks a camera entity as local player `n`'s camera: that player's director (on the
/// [`PlayerManager`]) blends it, and the shared director's blends leave it alone.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct PlayerCamera(pub u8);

/// The pose a blend goes to this frame, `None` when an entity target has no transform.
fn target_pose(world: &World, target: BlendTarget) -> Option<CameraPose> {
    match target {
        BlendTarget::Pose(pose) => Some(pose),
        BlendTarget::Entity(entity) => world.get::<Transform>(entity).map(CameraPose::of),
    }
}

/// Advances one director by `delta` once and moves each of its cameras from its own start pose.
fn step_director(world: &mut World, director: &mut CameraDirector, cameras: &[Entity], delta: f32) {
    let Some(blend) = director.blend.as_mut() else {
        return;
    };
    blend.advance(delta);
    let Some(target) = target_pose(world, blend.target) else {
        return;
    };
    for camera in cameras {
        let Some(current) = world.get::<Transform>(*camera).map(CameraPose::of) else {
            continue;
        };
        let pose = blend.pose(*camera, current, target);
        if let Some(mut transform) = world.get_mut::<Transform>(*camera) {
            transform.translation = pose.translation;
            transform.rotation = pose.rotation;
        }
    }
}

/// One blend pass: every director with a blend advances by `delta` seconds and moves its
/// cameras (see the module docs). [`super::CameraPlugin`] runs it in `PostUpdate` with the
/// frame's delta.
pub fn apply_camera_blends(world: &mut World, delta: f32) {
    let shared_blends = world
        .get_resource::<CameraDirector>()
        .is_some_and(CameraDirector::is_blending);
    let player_blends = world
        .get_resource::<PlayerManager>()
        .is_some_and(|manager| manager.directors.values().any(CameraDirector::is_blending));
    if !shared_blends && !player_blends {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_framework", "camera_blends").entered();
    let mut cameras: Vec<(Entity, Option<u8>)> = {
        let mut query = world.query_filtered::<(Entity, Option<&PlayerCamera>), (
            With<Transform>,
            Or<(With<CameraRig>, With<CinematicCamera>, With<PlayerCamera>)>,
        )>();
        query
            .iter(world)
            .map(|(entity, player)| (entity, player.map(|p| p.0)))
            .collect()
    };
    // Query order follows archetypes; blend in entity order.
    cameras.sort_unstable_by_key(|(entity, _)| *entity);
    if shared_blends && let Some(mut director) = world.remove_resource::<CameraDirector>() {
        let shared: Vec<Entity> = cameras
            .iter()
            .filter(|(_, player)| player.is_none())
            .map(|(entity, _)| *entity)
            .collect();
        step_director(world, &mut director, &shared, delta);
        world.insert_resource(director);
    }
    if player_blends && let Some(mut manager) = world.remove_resource::<PlayerManager>() {
        for (index, director) in &mut manager.directors {
            let mine: Vec<Entity> = cameras
                .iter()
                .filter(|(_, player)| *player == Some(*index))
                .map(|(entity, _)| *entity)
                .collect();
            step_director(world, director, &mine, delta);
        }
        world.insert_resource(manager);
    }
}

/// `PostUpdate` system: [`apply_camera_blends`] with the frame's delta.
pub(super) fn blend_cameras(world: &mut World) {
    let delta = world.get_resource::<Time>().map_or(0.0, Time::delta_secs);
    apply_camera_blends(world, delta);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{CameraDirector, CameraPlugin};
    use crate::player::{PlayerKind, PlayerManager, PlayerPlugin};
    use regia_core::math::{Easing, Quat, Vec3};
    use regia_scene::Transform;

    #[test]
    fn a_blend_reaches_the_target_at_exactly_duration_with_the_curve_midpoint() {
        let mut director = CameraDirector::default();
        let from = CameraPose {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
        };
        let to = CameraPose {
            translation: Vec3::new(8.0, 0.0, -4.0),
            rotation: Quat::from_rotation_y(1.0),
        };
        director.blend_to(to, 2.0, Easing::QuadIn);
        assert!(director.is_blending());
        // Half way in time: QuadIn(0.5) = 0.25 of the way.
        let half = director.blend_pose(from, 0.5).expect("blending");
        let half = director.blend_pose(half, 0.5).expect("blending");
        let expected = Easing::QuadIn.apply(0.5);
        assert_eq!(expected, 0.25);
        assert!(
            half.translation.abs_diff_eq(Vec3::new(2.0, 0.0, -1.0), 1.0e-6),
            "{:?}",
            half.translation
        );
        assert_eq!(director.blend_progress(), Some(0.5));
        // Exactly at `duration` the pose is the target, bit for bit.
        let end = director.blend_pose(half, 1.0).expect("ends");
        assert_eq!(end.translation, to.translation);
        assert_eq!(end.rotation, to.rotation);
        assert_eq!(director.blend_progress(), Some(1.0));
        // A finished pose blend holds its target until cleared.
        let held = director.blend_pose(from, 0.25).expect("held");
        assert_eq!(held.translation, to.translation);
        director.clear_blend();
        assert!(director.blend_pose(from, 0.25).is_none());
    }

    #[test]
    fn two_players_blend_independently() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin, PlayerPlugin));
        let mut manager = app
            .world_mut()
            .remove_resource::<PlayerManager>()
            .expect("manager");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("p0");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("p1");
        let cam0 = app
            .world_mut()
            .spawn((Transform::default(), PlayerCamera(0)))
            .id();
        let cam1 = app
            .world_mut()
            .spawn((Transform::default(), PlayerCamera(1)))
            .id();
        manager.camera_director_mut(0).expect("p0 director").blend_to(
            CameraPose::at(Vec3::new(4.0, 0.0, 0.0)),
            1.0,
            Easing::Linear,
        );
        manager.camera_director_mut(1).expect("p1 director").blend_to(
            CameraPose::at(Vec3::new(0.0, 0.0, -8.0)),
            2.0,
            Easing::Linear,
        );
        app.world_mut().insert_resource(manager);
        // Four fixed 0.25 s steps of the blend pass `CameraPlugin` runs in `PostUpdate`.
        for _ in 0..4 {
            apply_camera_blends(app.world_mut(), 0.25);
        }
        let p0 = app.world().get::<Transform>(cam0).expect("cam0").translation;
        let p1 = app.world().get::<Transform>(cam1).expect("cam1").translation;
        assert_eq!(p0, Vec3::new(4.0, 0.0, 0.0), "player 0 finished its 1 s blend");
        assert!(
            p1.abs_diff_eq(Vec3::new(0.0, 0.0, -4.0), 1.0e-5),
            "player 1 is half way: {p1:?}"
        );
        let manager = app.world().resource::<PlayerManager>();
        assert_eq!(
            manager
                .camera_director(0)
                .and_then(CameraDirector::blend_progress),
            Some(1.0)
        );
        assert_eq!(
            manager
                .camera_director(1)
                .and_then(CameraDirector::blend_progress),
            Some(0.5)
        );
        // The shared director did not blend.
        assert!(!app.world().resource::<CameraDirector>().is_blending());
        // A player who leaves takes their director along.
        let mut manager = app
            .world_mut()
            .remove_resource::<PlayerManager>()
            .expect("manager");
        assert!(manager.remove_player(app.world_mut(), 1));
        assert!(manager.camera_director(1).is_none());
        app.world_mut().insert_resource(manager);
    }
}
