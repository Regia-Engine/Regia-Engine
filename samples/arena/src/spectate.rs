// SPDX-License-Identifier: MIT-0
//! Spectator view (ADR-2134): the main camera shows the world's
//! `regia_framework::spectator::SpectatorCamera` instead of the first-person rig while its
//! [`SpectatorFocus`] is enabled. A bot-only match (`--no-local-player`) starts in this view,
//! following bot 1; the agent methods `spectator.get` / `set` / `step` drive the same resource
//! (`book/agents/arena.md`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::CameraSet;
use regia_framework::spectator::{SpectatorCamera, SpectatorConfig, SpectatorInput, SpectatorMode};
use regia_scene::Transform;

use crate::pawn::Pawn;

/// On the main camera: whether it shows the spectator pose and which pawn slot a following
/// spectator tracks. Reflected, so an agent turns the view on with `scene.set_property`.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct SpectatorFocus {
    /// The camera shows the spectator pose (otherwise the first-person rig).
    pub enabled: bool,
    /// The pawn slot a `follow` spectator tracks (1 = the first bot).
    pub slot: u8,
}

impl Default for SpectatorFocus {
    fn default() -> Self {
        Self {
            enabled: false,
            slot: 1,
        }
    }
}

/// Installs the spectator view.
pub struct SpectatePlugin;

impl Plugin for SpectatePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<SpectatorFocus>().add_systems(
            PostUpdate,
            follow_spectator
                .after(CameraSet::Update)
                .before(CameraSet::Cinematic),
        );
    }
}

/// A following spectator three metres behind and above `slot`'s spawn position.
pub fn following_spectator(target: Vec3) -> SpectatorCamera {
    let mut camera = SpectatorCamera::new(SpectatorConfig::default(), target + Vec3::new(0.0, 3.0, 6.0));
    camera.set_target(target);
    camera.follow();
    camera
}

/// `PostUpdate`: moves every enabled [`SpectatorFocus`] camera to the spectator pose, after the
/// rigs and before cinematic shots and shakes.
#[tracing::instrument(skip_all, target = "arena", name = "arena_spectator")]
pub fn follow_spectator(
    time: Res<Time>,
    spectator: Option<ResMut<SpectatorCamera>>,
    mut cameras: Query<(&SpectatorFocus, &mut Transform)>,
    pawns: Query<(Entity, &Pawn, &Transform), Without<SpectatorFocus>>,
) {
    let Some(mut spectator) = spectator else {
        return;
    };
    let dt = time.delta_secs();
    let mut stepped = false;
    for (focus, mut transform) in &mut cameras {
        if !focus.enabled {
            continue;
        }
        if !stepped {
            if spectator.mode() == SpectatorMode::Follow
                && let Some(target) = pawns
                    .iter()
                    .filter(|(_, pawn, _)| pawn.slot == focus.slot)
                    .min_by_key(|(entity, _, _)| entity.index())
                    .map(|(_, _, t)| t.translation)
            {
                spectator.set_target(target);
            }
            spectator.update(&SpectatorInput::default(), dt);
            stepped = true;
        }
        let (position, rotation) = spectator.pose();
        transform.translation = position;
        transform.rotation = rotation;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enabled_focus_puts_the_camera_on_the_spectator_pose() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SpectatePlugin));
        enable_fixed_step(&mut app, 60.0);
        let bot = app
            .world_mut()
            .spawn((
                Pawn {
                    slot: 1,
                    is_bot: true,
                },
                Transform::from_translation(Vec3::new(4.0, 1.6, -2.0)),
            ))
            .id();
        let camera = app
            .world_mut()
            .spawn((
                SpectatorFocus {
                    enabled: true,
                    slot: 1,
                },
                Transform::default(),
            ))
            .id();
        app.insert_resource(following_spectator(Vec3::ZERO));
        for _ in 0..240 {
            app.update();
        }
        let pose = app.world().resource::<SpectatorCamera>().pose();
        let transform = app.world().get::<Transform>(camera).expect("camera");
        assert_eq!(transform.translation, pose.0);
        assert_eq!(transform.rotation, pose.1);
        let bot_pos = app.world().get::<Transform>(bot).expect("bot").translation;
        let anchor = app.world().resource::<SpectatorCamera>().anchor();
        assert!(
            (anchor - bot_pos).length() < 0.5,
            "the anchor {anchor} chased the followed bot {bot_pos}"
        );
    }

    #[test]
    fn a_disabled_focus_leaves_the_camera_alone() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SpectatePlugin));
        let start = Transform::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let camera = app.world_mut().spawn((SpectatorFocus::default(), start)).id();
        app.insert_resource(following_spectator(Vec3::ZERO));
        app.update();
        assert_eq!(app.world().get::<Transform>(camera), Some(&start));
    }
}
