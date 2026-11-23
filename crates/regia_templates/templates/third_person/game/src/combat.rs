// SPDX-License-Identifier: MIT-0
//! Health, damage and the player's three-hit sword combo.
//!
//! The loop every fight uses: something writes a [`DamageEvent`]; [`apply_damage`] subtracts it
//! from the target's [`Health`] and writes a [`Died`] message when it reaches zero; death
//! handling (enemy topples, player respawns) lives in `enemy.rs` and `respawn.rs`. Swap the
//! numbers in [`COMBO`] for your weapon, or write your own `DamageEvent` from a projectile,
//! a trap or a script: nothing else changes.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::Character;
use regia_input::prelude::*;
use regia_scene::Transform;

use crate::enemy::Enemy;
use crate::sound::SoundCue;

/// Full health of the player.
pub const PLAYER_HEALTH: f32 = 100.0;
/// Reach of a sword swing in front of the player (meters).
pub const REACH: f32 = 2.4;
/// Seconds after a swing during which the next press continues the combo.
pub const COMBO_WINDOW: f32 = 0.9;
/// Damage and recovery seconds of the three combo steps.
pub const COMBO: [(f32, f32); 3] = [(10.0, 0.35), (10.0, 0.35), (25.0, 0.6)];

/// Hit points of a player or an enemy.
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

    /// Health as a 0..=1 fraction (for a bar).
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.max <= 0.0 {
            0.0
        } else {
            (self.current / self.max).clamp(0.0, 1.0)
        }
    }
}

/// A request to hurt `target`. Anything that deals damage writes one.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct DamageEvent {
    /// Who is hurt.
    pub target: Entity,
    /// Hit points taken.
    pub amount: f32,
    /// Where the blow came from (the hit reaction pushes away from it).
    pub from: Vec3,
}

/// Written once when a [`Health`] reaches zero.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct Died {
    /// Who died.
    pub entity: Entity,
    /// Where they were.
    pub position: Vec3,
}

/// The player's attack state: combo step, cooldown and the window for the next press.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Fighter {
    /// Seconds until the next swing may start.
    pub cooldown: f32,
    /// Seconds left to continue the combo.
    pub window: f32,
    /// The step of the last swing (0 before any, then 1..=3).
    pub step: u8,
    /// Seconds since the last swing started (drives the sword animation).
    pub swing_time: f32,
}

/// The sword mesh on the player's hand; [`animate_sword`] swings it.
#[derive(Component, Debug, Clone, Copy)]
pub struct Sword;

/// `Update`: counts the fighter's timers down.
pub fn tick_fighter(time: Res<Time>, mut fighters: Query<&mut Fighter>) {
    let dt = time.delta_secs();
    for mut fighter in &mut fighters {
        fighter.cooldown = (fighter.cooldown - dt).max(0.0);
        fighter.window = (fighter.window - dt).max(0.0);
        fighter.swing_time += dt;
    }
}

/// `Update`: the Attack button swings the sword. Enemies within [`REACH`] and in front of the
/// player take the step's damage. A press inside [`COMBO_WINDOW`] continues the combo.
#[tracing::instrument(skip_all, name = "player_attack")]
pub fn player_attack(
    actions: Res<Actions>,
    mut players: Query<(&Transform, &mut Fighter, &Health), With<Character>>,
    enemies: Query<(Entity, &Transform, &Health), With<Enemy>>,
    mut damage: MessageWriter<DamageEvent>,
    mut cues: MessageWriter<SoundCue>,
) {
    if !actions.just_pressed_this_frame("Attack") {
        return;
    }
    for (transform, mut fighter, health) in &mut players {
        if health.is_dead() || fighter.cooldown > 0.0 {
            continue;
        }
        fighter.step = if fighter.window > 0.0 && fighter.step < 3 {
            fighter.step + 1
        } else {
            1
        };
        let (amount, recovery) = COMBO[usize::from(fighter.step) - 1];
        fighter.cooldown = recovery;
        fighter.window = COMBO_WINDOW;
        fighter.swing_time = 0.0;
        cues.write(SoundCue::Swing(transform.translation));
        let forward = (transform.rotation * -Vec3::Z).with_y(0.0).normalize_or_zero();
        for (enemy, enemy_at, enemy_health) in &enemies {
            let to = (enemy_at.translation - transform.translation).with_y(0.0);
            if enemy_health.is_dead() || to.length() > REACH || to.normalize_or_zero().dot(forward) < 0.2 {
                continue;
            }
            damage.write(DamageEvent {
                target: enemy,
                amount,
                from: transform.translation,
            });
        }
    }
}

/// `Update`: subtracts every [`DamageEvent`] from its target's [`Health`] and writes [`Died`]
/// the moment one reaches zero.
#[tracing::instrument(skip_all, name = "apply_damage")]
pub fn apply_damage(
    mut events: MessageReader<DamageEvent>,
    mut healths: Query<(&mut Health, &Transform)>,
    mut died: MessageWriter<Died>,
    mut cues: MessageWriter<SoundCue>,
) {
    for event in events.read() {
        let Ok((mut health, transform)) = healths.get_mut(event.target) else {
            continue;
        };
        if health.is_dead() {
            continue;
        }
        health.current = (health.current - event.amount).max(0.0);
        cues.write(SoundCue::Hit(transform.translation));
        if health.is_dead() {
            died.write(Died {
                entity: event.target,
                position: transform.translation,
            });
        }
    }
}

/// `Update`: swings the sword mesh through an arc while a swing is in progress.
pub fn animate_sword(
    players: Query<(&Fighter, &Children), With<Character>>,
    mut swords: Query<&mut Transform, With<Sword>>,
) {
    for (fighter, children) in &players {
        for &child in children {
            let Ok(mut transform) = swords.get_mut(child) else {
                continue;
            };
            let progress = (fighter.swing_time / 0.3).clamp(0.0, 1.0);
            // Resting: held up. Swinging: sweeps forward and down, a different arc each step.
            let arc = if progress < 1.0 {
                (progress * std::f32::consts::PI).sin()
            } else {
                0.0
            };
            let roll = if fighter.step == 2 { 0.9 } else { 0.0 };
            transform.rotation = regia_core::math::Quat::from_rotation_x(-0.4 - arc * 1.8)
                * regia_core::math::Quat::from_rotation_z(roll * arc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_fraction_is_clamped() {
        let mut health = Health::new(50.0);
        assert!((health.fraction() - 1.0).abs() < f32::EPSILON);
        health.current = -5.0;
        assert!(health.is_dead());
        assert!(health.fraction().abs() < f32::EPSILON);
        assert!(Health::new(0.0).fraction().abs() < f32::EPSILON);
    }

    #[test]
    fn the_combo_ends_with_the_heavy_hit() {
        assert!(COMBO[2].0 > COMBO[0].0);
        assert_eq!(COMBO.len(), 3);
    }
}
