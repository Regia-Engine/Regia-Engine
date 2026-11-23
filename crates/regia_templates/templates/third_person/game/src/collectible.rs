// SPDX-License-Identifier: MIT-0
//! Collectibles: golden cubes the character picks up by walking into them.
//!
//! The saveable state is [`CollectProgress`] (which ids are gone). [`reconcile`] makes the live
//! entities match it, so a load brings back a coin collected after the save and removes one
//! collected before it. To react to a pickup (play a sound, grant an item), add your code where
//! [`collect_nearby`] pushes the id.

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::character::Character;
use regia_framework::save_game::SaveableResource;
use regia_render::MeshRenderer;
use regia_scene::{GlobalTransform, Transform, components::Name};

use crate::level::{cube_mesh, solid};

/// Horizontal pickup distance from the character's axis (meters).
const PICKUP_RADIUS: f32 = 0.9;
/// Vertical pickup distance from the character's center (meters).
const PICKUP_HEIGHT: f32 = 1.4;

/// Marks a collectible and identifies it across saves.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct CollectibleId(pub u32);

/// Which collectibles are gone. Saved with the game; [`reconcile`] applies it to the world.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct CollectProgress {
    /// Ids that have been collected, in collection order.
    pub collected: Vec<u32>,
}

impl SaveableResource for CollectProgress {}

/// Where the level's collectibles sit (character-center heights), in id order.
/// Put your pickups here, or spawn them from your level data.
#[must_use]
pub fn spots() -> &'static [Vec3] {
    const SPOTS: [Vec3; 4] = [
        Vec3::new(0.0, 0.9, -3.0),
        Vec3::new(-6.0, 0.9, -5.0),
        Vec3::new(6.0, 0.9, 6.0),
        // On top of the stairs.
        Vec3::new(8.0, 1.7, 1.6),
    ];
    &SPOTS
}

/// How many collectibles the level holds.
#[must_use]
pub fn total() -> u32 {
    u32::try_from(spots().len()).unwrap_or(u32::MAX)
}

/// Spawns every collectible.
pub fn spawn_all(commands: &mut Commands) {
    for (id, position) in (0_u32..).zip(spots().iter().copied()) {
        spawn_one(commands, id, position);
    }
}

fn spawn_one(commands: &mut Commands, id: u32, position: Vec3) {
    commands.spawn((
        Name::new(format!("Coin{id}").as_str()),
        Transform::from_translation(position),
        GlobalTransform(Transform::from_translation(position).compute_affine()),
        CollectibleId(id),
        MeshRenderer::new(
            std::sync::Arc::new(cube_mesh(Vec3::splat(0.22))),
            solid([0.95, 0.75, 0.15, 1.0]),
        ),
    ));
}

/// `Update`: records every collectible a character is touching in [`CollectProgress`].
#[tracing::instrument(skip_all, name = "collect_nearby")]
pub fn collect_nearby(
    characters: Query<&Transform, With<Character>>,
    coins: Query<(&CollectibleId, &Transform)>,
    mut progress: ResMut<CollectProgress>,
    mut cues: MessageWriter<crate::sound::SoundCue>,
) {
    for (id, coin) in &coins {
        if progress.collected.contains(&id.0) {
            continue;
        }
        let touched = characters.iter().any(|character| {
            let delta = coin.translation - character.translation;
            delta.with_y(0.0).length() <= PICKUP_RADIUS && delta.y.abs() <= PICKUP_HEIGHT
        });
        if touched {
            progress.collected.push(id.0);
            cues.write(crate::sound::SoundCue::Pickup(coin.translation));
            info!("collected coin {}", id.0);
        }
    }
}

/// `Update` (after [`collect_nearby`]): when the progress changed (a pickup or a load), despawns
/// the collected coins and respawns the missing ones.
#[tracing::instrument(skip_all, name = "reconcile_collectibles")]
pub fn reconcile(
    mut commands: Commands,
    progress: Res<CollectProgress>,
    coins: Query<(Entity, &CollectibleId)>,
) {
    if !progress.is_changed() {
        return;
    }
    for (entity, id) in &coins {
        if progress.collected.contains(&id.0) {
            commands.entity(entity).despawn();
        }
    }
    for (id, position) in (0_u32..).zip(spots().iter().copied()) {
        let present = coins.iter().any(|(_, coin)| coin.0 == id);
        if !present && !progress.collected.contains(&id) {
            spawn_one(&mut commands, id, position);
        }
    }
}

/// `Update`: spins the remaining coins (visual only).
pub fn spin(time: Res<Time>, mut coins: Query<&mut Transform, With<CollectibleId>>) {
    let angle = time.elapsed_secs() * 1.5;
    for mut transform in &mut coins {
        transform.rotation = Quat::from_rotation_y(angle);
    }
}
