// SPDX-License-Identifier: MPL-2.0
//! The listener follows the camera (`docs/specs/16-audio.md` § 3). `regia_audio` does not know the
//! renderer's [`Camera`], so this system copies the active camera's transform into
//! [`AudioActiveCamera`] for the audio sync. [`DefaultPlugins`](crate::DefaultPlugins) installs it.
//! An `AudioListener` on any entity takes over, and `AudioListenerFollow::Off` turns the follow
//! off; both are decided in `regia_audio::audio_sync`.

use regia_app::prelude::*;
use regia_audio::{AudioActiveCamera, AudioCameraPose};
use regia_render::{Camera, RenderTarget};
use regia_scene::GlobalTransform;

/// Installs [`publish_active_camera`] before the audio sync.
#[derive(Default)]
pub struct CameraAudioListenerPlugin;

impl Plugin for CameraAudioListenerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            publish_active_camera
                .in_set(PostUpdateSet::AudioSync)
                .before(regia_audio::audio_sync),
        );
    }
}

/// `PostUpdate`/`AudioSync` system: the active camera is the one the frame presents first, the
/// lowest `order` drawing to the window (the lowest entity on a tie). A camera that does not
/// draw to the window is ignored. With none drawing to the window, the ears stay at the origin
/// (`AudioActiveCamera` is `None`).
///
/// The pose comes from [`GlobalTransform::to_transform`], which keeps an identity facing when the
/// scale has collapsed. A non-finite translation or rotation is skipped, so it cannot poison the
/// listener; the next finite window camera is heard instead.
#[tracing::instrument(target = "regia", skip_all, level = "trace")]
pub fn publish_active_camera(
    cameras: Query<(Entity, &Camera, &GlobalTransform)>,
    active: Option<ResMut<AudioActiveCamera>>,
) {
    let Some(mut active) = active else {
        return;
    };
    let chosen = cameras
        .iter()
        .filter(|(_, camera, _)| matches!(camera.target, RenderTarget::Window))
        .filter_map(|(entity, camera, global)| {
            finite_camera_pose(entity, global).map(|pose| (camera.order, entity, pose))
        })
        .min_by_key(|(order, entity, _)| (*order, *entity))
        .map(|(_, _, pose)| pose);
    if active.0 != chosen {
        active.0 = chosen;
    }
}

/// The listener pose of `global`, or `None` when the translation or rotation is not finite.
///
/// A collapsed scale still yields a pose: `GlobalTransform::to_transform` returns an identity
/// rotation instead of reciprocating a zero axis.
fn finite_camera_pose(entity: Entity, global: &GlobalTransform) -> Option<AudioCameraPose> {
    let transform = global.to_transform();
    let position = transform.translation;
    let rotation = transform.rotation;
    if !position.is_finite() || !rotation.is_finite() || !rotation.is_normalized() {
        return None;
    }
    Some(AudioCameraPose {
        entity,
        position,
        rotation,
    })
}
