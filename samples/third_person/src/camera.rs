// SPDX-License-Identifier: MIT-0
//! Per-player cameras: a chase rig with the collision-probing boom, plus the split-screen
//! viewport assignment from the `PlayerManager` layout.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::{CameraMode, CameraRig, ChaseCamera, ThirdPerson};
use regia_framework::player::{Player, PlayerManager};
use regia_render::Camera;
use regia_scene::Transform;

/// Marker for a camera entity belonging to a local player.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerCamera {
    /// Which player this camera renders for.
    pub player: u8,
}

/// Reflected copy of this camera's normalised viewport (`x`, `y`, `w`, `h`).
///
/// `Camera` is not a reflected type, so agent `play.expect` checks read this component.
/// [`sync_viewports`] writes the same rectangle into `Camera.viewport` for the renderer.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SplitViewport {
    /// Left edge, 0–1.
    pub x: f32,
    /// Bottom edge, 0–1.
    pub y: f32,
    /// Width, 0–1.
    pub w: f32,
    /// Height, 0–1.
    pub h: f32,
}

/// Ensures every joined player with a character has a camera rig, and keeps each camera's
/// `Camera::viewport` (and draw order) in sync with the split-screen layout.
#[tracing::instrument(skip_all, target = "third_person", name = "sync_viewports")]
pub fn sync_viewports(world: &mut World) {
    let Some(manager) = world.get_resource::<PlayerManager>() else {
        return;
    };
    let players: Vec<(u8, Entity)> = manager.iter().collect();
    let layout = manager.layout.viewports.clone();

    for (index, player_entity) in players {
        // The camera follows this player's possessed character.
        let Some(player_data) = world.get::<Player>(player_entity) else {
            continue;
        };
        let target = world
            .get::<regia_framework::player::PlayerController>(player_data.controller)
            .map(|pc| pc.view_target)
            .unwrap_or(Entity::PLACEHOLDER);
        if target == Entity::PLACEHOLDER {
            continue;
        }

        // Find or create this player's camera.
        let mut existing: Option<Entity> = None;
        let mut query = world.query_filtered::<(Entity, &PlayerCamera), ()>();
        for (entity, cam) in query.iter(world) {
            if cam.player == index {
                existing = Some(entity);
                break;
            }
        }
        let camera = existing.unwrap_or_else(|| {
            let mode = ThirdPerson {
                boom_length: 4.5,
                socket_offset: Vec3::new(0.0, 0.8, 0.0),
                target_offset: Vec3::new(0.0, 0.5, 0.0),
                probe_radius: 0.2,
                ..ThirdPerson::default()
            };
            world
                .spawn((
                    Transform::default(),
                    CameraRig {
                        mode: CameraMode::ThirdPerson(mode),
                        ..CameraRig::default()
                    },
                    ChaseCamera { target },
                    Camera {
                        fov_y: 60.0_f32.to_radians(),
                        near: 0.05,
                        far: 200.0,
                        // Outdoor sun: a fixed exposure keeps the ground and the Fox readable
                        // in screenshots (auto exposure hunts on the first frames).
                        // EV 13 keeps the sunlit Fox and ground in range on lavapipe.
                        // EV 14.5 crushed the frame toward black; EV 11 clipped the ground
                        // to white and erased the character's colour.
                        exposure_ev: Some(12.7),
                        clear: regia_render::ClearMode::Color([0.45, 0.62, 0.85, 1.0]),
                        ..Camera::default()
                    },
                    PlayerCamera { player: index },
                    SplitViewport::default(),
                    // Scene name (outliner) and the ECS `Name` the agent resolver reads.
                    regia_scene::components::Name::new(format!("player-camera-{index}")),
                    Name::new(format!("player-camera-{index}")),
                ))
                .id()
        });

        // Keep the chase target current (respawns change the character entity).
        if let Some(mut chase) = world.get_mut::<ChaseCamera>(camera) {
            chase.target = target;
        }
        // Split-screen: the layout gives each player a normalised viewport rectangle.
        let (x, y, w, h) = layout.get(&index).copied().unwrap_or((0.0, 0.0, 1.0, 1.0));
        if let Some(mut cam) = world.get_mut::<Camera>(camera) {
            cam.viewport = [x, y, w, h];
            // Lower index draws first so the second view composites on top of a partial clear.
            cam.order = i32::from(index);
        }
        if let Some(mut split) = world.get_mut::<SplitViewport>(camera) {
            *split = SplitViewport { x, y, w, h };
        }
    }
}
