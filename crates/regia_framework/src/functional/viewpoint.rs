// SPDX-License-Identifier: MPL-2.0
//! The viewpoint service of functional tests (spec 41 § 2.5): a test's `viewpoint` puts the local
//! camera at an entity's world pose at Begin; teardown puts it back, however the run ended.
//!
//! The local camera is the window camera with the lowest `order` (the one the exposure zones
//! read), else the lowest-order camera of any target, so a headless run has one too; ties go to
//! the lower entity. A camera rig or driver writes `Transform` every frame, so
//! [`hold_viewpoint`] keeps such a camera at the viewpoint while the override lasts. A camera with
//! neither stays where the test moves it.
//!
//! Poses come from the `Transform` hierarchy, not from `GlobalTransform`: Begin can run in the frame
//! a target spawned or moved, before propagation, and a headless test world may not propagate.

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_render::{Camera, RenderTarget};
use regia_scene::Transform;

use crate::camera::driver::CameraDriver;
use crate::camera::{CameraRig, CameraSet};

/// The camera a test moved and what to put back.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct ViewpointOverride {
    /// The local camera.
    pub camera: Entity,
    /// Its `Transform` before the test.
    pub previous: Transform,
    /// The local translation and rotation of the viewpoint.
    pub pose: (Vec3, Quat),
}

/// The local camera of this world (see the module docs).
#[must_use]
pub fn local_camera(world: &mut World) -> Option<Entity> {
    let mut query = world.query::<(Entity, &Camera)>();
    query
        .iter(world)
        .min_by_key(|(entity, camera)| {
            (
                !matches!(camera.target, RenderTarget::Window),
                camera.order,
                entity.index().index(),
            )
        })
        .map(|(entity, _)| entity)
}

/// `entity`'s world transform, composed up its `ChildOf` chain. `None` without a `Transform`.
fn world_affine(world: &World, entity: Entity) -> Option<Affine3A> {
    let mut affine = world.get::<Transform>(entity)?.compute_affine();
    let mut at = entity;
    while let Some(link) = world.get::<ChildOf>(at) {
        at = link.parent();
        let parent = world
            .get::<Transform>(at)
            .map_or(Affine3A::IDENTITY, Transform::compute_affine);
        affine = parent * affine;
    }
    Some(affine)
}

/// `pose` in the space of `camera`'s parent (the world when it has none).
fn local_pose(world: &World, camera: Entity, (translation, rotation): (Vec3, Quat)) -> (Vec3, Quat) {
    let Some(parent) = world
        .get::<ChildOf>(camera)
        .and_then(|link| world_affine(world, link.parent()))
    else {
        return (translation, rotation);
    };
    let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
    (
        parent.inverse().transform_point3(translation),
        (parent_rotation.inverse() * rotation).normalize(),
    )
}

/// Moves the local camera to `target`'s world pose (`Some`) or puts it back (`None`). The default
/// `viewpoint` hook of [`TestHooks`](super::TestHooks). Restoring with nothing to restore is fine.
pub fn set_viewpoint(world: &mut World, target: Option<Entity>) -> Result<(), String> {
    let Some(target) = target else {
        if let Some(held) = world.remove_resource::<ViewpointOverride>()
            && let Some(mut transform) = world.get_mut::<Transform>(held.camera)
        {
            *transform = held.previous;
        }
        return Ok(());
    };
    let (_, rotation, translation) = world_affine(world, target)
        .ok_or_else(|| format!("entity {target} has no transform"))?
        .to_scale_rotation_translation();
    let camera = local_camera(world).ok_or_else(|| "this world has no camera".to_owned())?;
    let previous = world
        .get::<Transform>(camera)
        .copied()
        .ok_or_else(|| format!("camera {camera} has no transform"))?;
    let pose = local_pose(world, camera, (translation, rotation));
    if let Some(mut transform) = world.get_mut::<Transform>(camera) {
        transform.translation = pose.0;
        transform.rotation = pose.1;
    }
    // A second test in a row must restore what was there before the first.
    let previous = world
        .get_resource::<ViewpointOverride>()
        .filter(|held| held.camera == camera)
        .map_or(previous, |held| held.previous);
    world.insert_resource(ViewpointOverride {
        camera,
        previous,
        pose,
    });
    Ok(())
}

/// Cameras something writes every frame.
type DrivenCameras<'w, 's> = Query<'w, 's, &'static mut Transform, Or<(With<CameraRig>, With<CameraDriver>)>>;

/// `PostUpdate`, after the camera rigs and before transform propagation: a rig-driven local camera
/// stays at the viewpoint while a test holds it.
pub fn hold_viewpoint(held: Res<ViewpointOverride>, mut cameras: DrivenCameras) {
    if let Ok(mut transform) = cameras.get_mut(held.camera) {
        transform.translation = held.pose.0;
        transform.rotation = held.pose.1;
    }
}

/// Schedules [`hold_viewpoint`].
pub(crate) fn install(app: &mut App) {
    app.add_systems(
        PostUpdate,
        hold_viewpoint
            .run_if(resource_exists::<ViewpointOverride>)
            .after(CameraSet::Update)
            .before(PostUpdateSet::TransformPropagate),
    );
}
