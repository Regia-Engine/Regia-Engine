// SPDX-License-Identifier: MIT-0
//! Health, damage, the player's attack and the player's death and respawn.
//!
//! Anything that hurts writes a [`DamageEvent`]; [`apply_damage`] subtracts it from the target's
//! [`Health`] and writes [`Died`] when it reaches zero. The enemy's brain (`enemy.rs`) writes
//! events for its blows; [`player_attack`] writes them for the player's.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{Character, CharacterState};
use regia_framework::rules::spawn::RespawnPolicy;
use regia_input::prelude::*;
use regia_scene::Transform;

use crate::enemy::Enemy;

/// Full health of the player.
pub const PLAYER_HEALTH: f32 = 100.0;
/// Reach of the player's attack (meters).
pub const REACH: f32 = 2.4;
/// Damage of one attack.
pub const ATTACK_DAMAGE: f32 = 12.0;
/// Seconds between two attacks.
pub const ATTACK_COOLDOWN: f32 = 0.4;
/// Where the player starts and respawns.
pub const SPAWN_POINT: Vec3 = Vec3::new(0.0, 1.2, 0.0);
/// When the player comes back: 120 fixed steps (2 s at [`TICK_HZ`]) after dying. `Instant`,
/// `Waves { period }` and `Never` (stay down) work too.
pub const RESPAWN: RespawnPolicy = RespawnPolicy::Delay { ticks: 120 };
/// The step rate [`RESPAWN`]'s ticks are counted at.
pub const TICK_HZ: f32 = 60.0;

/// Seconds between a death at `now` (seconds since start) and standing up again under `policy`;
/// `None` when the policy never brings the player back.
#[must_use]
pub fn respawn_secs(policy: &RespawnPolicy, now: f32) -> Option<f32> {
    let tick = (now.max(0.0) * TICK_HZ) as u64;
    let due = policy.due(tick, u32::MAX)?;
    Some(due.saturating_sub(tick) as f32 / TICK_HZ)
}

/// Hit points of the player or the enemy.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct Health {
    /// Hit points left.
    pub current: f32,
    /// Hit points at full health.
    pub max: f32,
}

impl Default for Health {
    fn default() -> Self {
        Self::new(PLAYER_HEALTH)
    }
}

impl Health {
    /// Full health of `max` points.
    #[must_use]
    pub const fn new(max: f32) -> Self {
        Self { current: max, max }
    }

    /// Whether no hit points are left.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.current <= 0.0
    }

    /// Health as a 0..=1 fraction (the bar's fill).
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.max <= 0.0 {
            0.0
        } else {
            (self.current / self.max).clamp(0.0, 1.0)
        }
    }
}

/// A request to hurt `target`.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct DamageEvent {
    /// Who is hurt.
    pub target: Entity,
    /// Hit points taken.
    pub amount: f32,
}

/// Written once when a [`Health`] reaches zero.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct Died {
    /// Who died.
    pub entity: Entity,
}

/// The player's attack cooldown and respawn countdown.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Fighter {
    /// Seconds until the next attack may start.
    pub cooldown: f32,
    /// Seconds until the player stands up again; `None` while alive.
    pub respawn_in: Option<f32>,
}

/// `Update`: the Attack button hurts every enemy within [`REACH`].
pub fn player_attack(
    actions: Res<Actions>,
    time: Res<Time>,
    mut players: Query<(&Transform, &mut Fighter, &Health), With<Character>>,
    enemies: Query<(Entity, &Transform, &Health), With<Enemy>>,
    mut damage: MessageWriter<DamageEvent>,
) {
    for (transform, mut fighter, health) in &mut players {
        fighter.cooldown = (fighter.cooldown - time.delta_secs()).max(0.0);
        if !actions.just_pressed_this_frame("Attack") || fighter.cooldown > 0.0 || health.is_dead() {
            continue;
        }
        fighter.cooldown = ATTACK_COOLDOWN;
        for (enemy, enemy_at, enemy_health) in &enemies {
            let flat = (enemy_at.translation - transform.translation).with_y(0.0);
            if !enemy_health.is_dead() && flat.length() <= REACH {
                damage.write(DamageEvent {
                    target: enemy,
                    amount: ATTACK_DAMAGE,
                });
            }
        }
    }
}

/// `Update`: subtracts every [`DamageEvent`] and writes [`Died`] for the one that empties a bar.
pub fn apply_damage(
    mut events: MessageReader<DamageEvent>,
    mut healths: Query<&mut Health>,
    mut died: MessageWriter<Died>,
) {
    for event in events.read() {
        let Ok(mut health) = healths.get_mut(event.target) else {
            continue;
        };
        if health.is_dead() {
            continue;
        }
        health.current = (health.current - event.amount).max(0.0);
        if health.is_dead() {
            died.write(Died { entity: event.target });
        }
    }
}

/// `Update`: when the player dies a countdown starts; at zero the player stands at the spawn
/// point with full health.
pub fn respawn_player(
    mut died: MessageReader<Died>,
    time: Res<Time>,
    mut players: Query<
        (
            Entity,
            &mut Transform,
            &mut CharacterState,
            &mut Health,
            &mut Fighter,
        ),
        With<Character>,
    >,
    mut progress: ResMut<crate::enemy::CombatProgress>,
) {
    let fallen: Vec<Entity> = died.read().map(|event| event.entity).collect();
    for (entity, mut transform, mut state, mut health, mut fighter) in &mut players {
        if fallen.contains(&entity) {
            // A policy that never respawns leaves the player down.
            fighter.respawn_in = Some(respawn_secs(&RESPAWN, time.elapsed_secs()).unwrap_or(f32::INFINITY));
            progress.deaths += 1;
        }
        if let Some(left) = fighter.respawn_in {
            let left = left - time.delta_secs();
            if left > 0.0 {
                fighter.respawn_in = Some(left);
            } else {
                fighter.respawn_in = None;
                transform.translation = SPAWN_POINT;
                state.velocity = Vec3::ZERO;
                *health = Health::new(PLAYER_HEALTH);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_respawn_policy_sets_the_countdown() {
        assert_eq!(respawn_secs(&RESPAWN, 3.0), Some(2.0));
        assert_eq!(respawn_secs(&RespawnPolicy::Never, 3.0), None);
    }

    #[test]
    fn health_fraction_is_clamped() {
        let mut health = Health::new(40.0);
        health.current = 10.0;
        assert!((health.fraction() - 0.25).abs() < 1e-6);
        health.current = -3.0;
        assert!(health.is_dead() && health.fraction().abs() < 1e-6);
    }
}
