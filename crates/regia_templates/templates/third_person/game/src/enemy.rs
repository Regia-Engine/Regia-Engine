// SPDX-License-Identifier: MIT-0
//! Enemies: three grunts that wait at their post, chase the player they see, wind up and swing,
//! flinch when hit and topple when they die.
//!
//! The brain is a small state machine ([`EnemyState`]) so this file reads top to bottom. Movement
//! is a straight line on the flat ground, not a path: for enemies that walk around obstacles
//! see the `top_down` template, which drives its enemy with `regia_nav` and `regia_ai`, and the
//! engine's the engine's third-person sample, which uses behavior trees on a navmesh crowd.
//!
//! Which enemies are dead rides in [`CombatProgress`], a saved resource: [`reconcile`] makes the
//! live enemies match it, exactly as `collectible::reconcile` does for coins, so a load brings
//! back an enemy killed after the save.

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::character::Character;
use regia_framework::save_game::SaveableResource;
use regia_render::MeshRenderer;
use regia_scene::{GlobalTransform, Transform, components::Name};

use crate::combat::{DamageEvent, Died, Health};
use crate::level::{cube_mesh, solid};
use crate::sound::SoundCue;

/// Hit points of a grunt.
pub const ENEMY_HEALTH: f32 = 30.0;
/// How far a grunt notices the player (meters).
pub const SIGHT: f32 = 9.0;
/// How far from its post a grunt chases before it gives up (meters).
pub const LEASH: f32 = 16.0;
/// Chase speed (meters per second).
pub const SPEED: f32 = 2.6;
/// Distance at which a grunt starts a swing (meters).
pub const STRIKE_RANGE: f32 = 1.7;
/// Seconds from the start of a swing to the blow.
pub const WINDUP: f32 = 0.5;
/// Seconds a grunt rests after a swing.
pub const RECOVERY: f32 = 0.9;
/// Hit points one blow takes from the player.
pub const STRIKE_DAMAGE: f32 = 8.0;
/// Center height of a grunt above the ground (meters).
const BODY_Y: f32 = 0.8;
/// Seconds a corpse lies before it is removed.
const CORPSE_SECS: f32 = 1.5;

/// What a grunt is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EnemyState {
    /// At its post, watching.
    Idle,
    /// Running at the player.
    Chase,
    /// Mid-swing: the blow lands when the timer runs out.
    Windup,
    /// Resting after a swing.
    Recover,
    /// Dead and toppling.
    Down,
}

/// A grunt's brain.
#[derive(Component, Debug, Clone, Copy)]
pub struct Enemy {
    /// Where it stands when it has nothing to do.
    pub home: Vec3,
    /// The current state.
    pub state: EnemyState,
    /// Seconds left in `Windup`, `Recover` or `Down`.
    pub timer: f32,
    /// Seconds left of a flinch (it does not act while flinching).
    pub flinch: f32,
}

/// Identifies a grunt across saves.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct EnemyId(pub u32);

/// Which enemies are dead, how often the player died and the player's health. Saved with the
/// game ([`SaveableResource`]); `lib.rs` writes the health into it each frame.
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource, Default)]
pub struct CombatProgress {
    /// Ids of the defeated enemies, in the order they fell.
    pub defeated: Vec<u32>,
    /// How many times the player died.
    pub deaths: u32,
    /// The player's hit points.
    pub health: f32,
}

impl Default for CombatProgress {
    fn default() -> Self {
        Self {
            defeated: Vec::new(),
            deaths: 0,
            health: crate::combat::PLAYER_HEALTH,
        }
    }
}

impl SaveableResource for CombatProgress {}

/// Where the grunts stand, in id order. Put your enemy placements here or read them from level
/// data.
#[must_use]
pub fn posts() -> &'static [Vec3] {
    const POSTS: [Vec3; 3] = [
        Vec3::new(-5.0, BODY_Y, -9.0),
        Vec3::new(5.0, BODY_Y, -10.0),
        Vec3::new(-9.0, BODY_Y, 6.0),
    ];
    &POSTS
}

/// How many enemies the level holds.
#[must_use]
pub fn total() -> u32 {
    u32::try_from(posts().len()).unwrap_or(u32::MAX)
}

/// `Startup`: spawns every enemy.
pub fn spawn_all(mut commands: Commands) {
    for (id, post) in (0_u32..).zip(posts().iter().copied()) {
        spawn_one(&mut commands, id, post);
    }
}

fn spawn_one(commands: &mut Commands, id: u32, post: Vec3) {
    let start = Transform::from_translation(post);
    let enemy = commands
        .spawn((
            Name::new(format!("Grunt{id}").as_str()),
            start,
            GlobalTransform(start.compute_affine()),
            Enemy {
                home: post,
                state: EnemyState::Idle,
                timer: 0.0,
                flinch: 0.0,
            },
            EnemyId(id),
            Health::new(ENEMY_HEALTH),
        ))
        .id();
    let red = [0.75, 0.2, 0.18, 1.0];
    let dark = [0.25, 0.1, 0.1, 1.0];
    for (name, at, half, color) in [
        ("Body", Vec3::ZERO, Vec3::new(0.3, 0.5, 0.2), red),
        ("Head", Vec3::new(0.0, 0.7, 0.0), Vec3::splat(0.18), dark),
        ("Fist", Vec3::new(0.32, 0.1, -0.35), Vec3::splat(0.1), dark),
    ] {
        commands.spawn((
            Name::new(name),
            Transform::from_translation(at),
            GlobalTransform(Transform::from_translation(at).compute_affine()),
            MeshRenderer::new(std::sync::Arc::new(cube_mesh(half)), solid(color)),
            ChildOf(enemy),
        ));
    }
}

/// What the brain reads about a player: who, where and how hurt.
type PlayerRow = (Entity, &'static Transform, &'static Health);

/// `Update`: the brain. Reads the player's position, moves the grunt and writes its blow as a
/// [`DamageEvent`].
#[tracing::instrument(skip_all, name = "enemy_brain")]
pub fn think(
    time: Res<Time>,
    players: Query<PlayerRow, (With<Character>, Without<Enemy>)>,
    mut enemies: Query<(&mut Enemy, &mut Transform), Without<Character>>,
    mut damage: MessageWriter<DamageEvent>,
) {
    let dt = time.delta_secs();
    let player = players.iter().find(|(_, _, health)| !health.is_dead());
    for (mut enemy, mut transform) in &mut enemies {
        if enemy.state == EnemyState::Down {
            continue;
        }
        if enemy.flinch > 0.0 {
            enemy.flinch -= dt;
            continue;
        }
        let target = player.map(|(entity, at, _)| (entity, at.translation));
        let to_target = target.map(|(_, at)| (at - transform.translation).with_y(0.0));
        let distance = to_target.map_or(f32::MAX, Vec3::length);
        let from_home = (transform.translation - enemy.home).with_y(0.0).length();
        match enemy.state {
            EnemyState::Idle => {
                if distance < SIGHT {
                    enemy.state = EnemyState::Chase;
                } else if from_home > 0.2 {
                    // Walk back to the post.
                    let back = (enemy.home - transform.translation)
                        .with_y(0.0)
                        .normalize_or_zero();
                    transform.translation += back * SPEED * dt;
                }
            }
            EnemyState::Chase => {
                let (Some(_), Some(to)) = (target, to_target) else {
                    enemy.state = EnemyState::Idle;
                    continue;
                };
                if from_home > LEASH || distance > SIGHT * 1.6 {
                    enemy.state = EnemyState::Idle;
                } else if distance <= STRIKE_RANGE {
                    enemy.state = EnemyState::Windup;
                    enemy.timer = WINDUP;
                } else {
                    let dir = to.normalize_or_zero();
                    transform.translation += dir * SPEED * dt;
                    face(&mut transform, dir);
                }
            }
            EnemyState::Windup => {
                enemy.timer -= dt;
                if enemy.timer <= 0.0 {
                    if let Some((entity, _)) = target
                        && distance <= STRIKE_RANGE + 0.4
                    {
                        damage.write(DamageEvent {
                            target: entity,
                            amount: STRIKE_DAMAGE,
                            from: transform.translation,
                        });
                    }
                    enemy.state = EnemyState::Recover;
                    enemy.timer = RECOVERY;
                }
            }
            EnemyState::Recover => {
                enemy.timer -= dt;
                if enemy.timer <= 0.0 {
                    enemy.state = EnemyState::Chase;
                }
            }
            EnemyState::Down => {}
        }
        transform.translation.y = BODY_Y;
    }
}

/// Turns `transform` to look along `dir` (horizontal).
fn face(transform: &mut Transform, dir: Vec3) {
    if dir.length_squared() > 1e-6 {
        transform.rotation = Quat::from_rotation_y(f32::atan2(-dir.x, -dir.z));
    }
}

/// `Update`: a hit makes a grunt flinch, step back and notice the player.
pub fn react_to_hits(
    mut events: MessageReader<DamageEvent>,
    mut enemies: Query<(&mut Enemy, &mut Transform)>,
) {
    for event in events.read() {
        let Ok((mut enemy, mut transform)) = enemies.get_mut(event.target) else {
            continue;
        };
        if enemy.state == EnemyState::Down {
            continue;
        }
        let away = (transform.translation - event.from)
            .with_y(0.0)
            .normalize_or_zero();
        transform.translation += away * 0.6;
        enemy.flinch = 0.3;
        enemy.state = EnemyState::Chase;
    }
}

/// `Update`: a dead grunt topples, is recorded in [`CombatProgress`] and is removed after a
/// moment.
pub fn handle_deaths(
    mut died: MessageReader<Died>,
    mut enemies: Query<(&mut Enemy, &EnemyId)>,
    mut progress: ResMut<CombatProgress>,
    mut cues: MessageWriter<SoundCue>,
) {
    for event in died.read() {
        let Ok((mut enemy, id)) = enemies.get_mut(event.entity) else {
            continue;
        };
        enemy.state = EnemyState::Down;
        enemy.timer = CORPSE_SECS;
        if !progress.defeated.contains(&id.0) {
            progress.defeated.push(id.0);
        }
        cues.write(SoundCue::Fall(event.position));
        info!("enemy {} defeated", id.0);
    }
}

/// `Update`: lays a dead grunt on its back, then removes the corpse.
pub fn topple_corpses(
    mut commands: Commands,
    time: Res<Time>,
    mut corpses: Query<(Entity, &mut Enemy, &mut Transform)>,
) {
    for (entity, mut enemy, mut transform) in &mut corpses {
        if enemy.state != EnemyState::Down {
            continue;
        }
        enemy.timer -= time.delta_secs();
        let fallen = (1.0 - enemy.timer / CORPSE_SECS).clamp(0.0, 1.0);
        let y = transform.rotation.to_euler(regia_core::math::EulerRot::YXZ).0;
        transform.rotation = Quat::from_rotation_y(y) * Quat::from_rotation_x(-fallen.min(0.35) / 0.35 * 1.5);
        transform.translation.y = BODY_Y - fallen.min(0.35) / 0.35 * 0.45;
        if enemy.timer <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

/// `Update`: when the progress changed (a death or a load), removes enemies that should be
/// gone and respawns the ones a load brought back.
#[tracing::instrument(skip_all, name = "reconcile_enemies")]
pub fn reconcile(
    mut commands: Commands,
    progress: Res<CombatProgress>,
    enemies: Query<(Entity, &EnemyId, &Enemy)>,
) {
    if !progress.is_changed() {
        return;
    }
    for (entity, id, enemy) in &enemies {
        if progress.defeated.contains(&id.0) && enemy.state != EnemyState::Down {
            commands.entity(entity).despawn();
        }
    }
    for (id, post) in (0_u32..).zip(posts().iter().copied()) {
        let present = enemies.iter().any(|(_, enemy_id, _)| enemy_id.0 == id);
        if !present && !progress.defeated.contains(&id) {
            spawn_one(&mut commands, id, post);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_post_is_on_the_ground_plate() {
        for post in posts() {
            assert!(post.x.abs() < 20.0 && post.z.abs() < 20.0);
        }
        assert_eq!(total(), 3);
    }

    #[test]
    fn facing_points_minus_z_along_the_move() {
        let mut transform = Transform::default();
        face(&mut transform, -Vec3::Z);
        let forward = transform.rotation * -Vec3::Z;
        assert!((forward - -Vec3::Z).length() < 1e-4);
        face(&mut transform, Vec3::X);
        let forward = transform.rotation * -Vec3::Z;
        assert!((forward - Vec3::X).length() < 1e-4);
    }
}
