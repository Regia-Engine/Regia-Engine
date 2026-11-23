// SPDX-License-Identifier: MIT-0
//! Player death and respawn.
//!
//! When the player's [`Health`] reaches zero, [`on_player_death`] starts a
//! timer from the [`RESPAWN`] policy ("You died" shows on the HUD, input is ignored). [`run_respawn`]
//! then puts the player back at [`SPAWN_POINT`] with full health. Change [`RESPAWN`] to
//! `RespawnPolicy::Instant`, `Waves { period }` (everyone on the next multiple of `period`) or
//! `Never` (the player stays down). To respawn at checkpoints instead, store the last checkpoint in
//! `CombatProgress` (it is saved) and read it here.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{Character, CharacterState};
use regia_framework::rules::spawn::RespawnPolicy;
use regia_scene::Transform;

use crate::combat::{Died, Health, PLAYER_HEALTH};
use crate::enemy::CombatProgress;
use crate::sound::SoundCue;

/// Where the player starts and respawns.
pub const SPAWN_POINT: Vec3 = Vec3::new(0.0, 1.2, 5.0);
/// When the player comes back: 150 fixed steps (2.5 s at [`TICK_HZ`]) after dying.
pub const RESPAWN: RespawnPolicy = RespawnPolicy::Delay { ticks: 150 };
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

/// The respawn countdown; `Some` while the player is dead.
#[derive(Resource, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct Respawn {
    /// Seconds until the player stands up again.
    pub left: Option<f32>,
}

/// `Update`: starts the countdown when the player dies and counts the death.
pub fn on_player_death(
    time: Res<Time>,
    mut died: MessageReader<Died>,
    players: Query<(), With<Character>>,
    mut respawn: ResMut<Respawn>,
    mut progress: ResMut<CombatProgress>,
    mut cues: MessageWriter<SoundCue>,
) {
    for event in died.read() {
        if players.get(event.entity).is_ok() {
            // A policy that never respawns leaves the player down.
            respawn.left = Some(respawn_secs(&RESPAWN, time.elapsed_secs()).unwrap_or(f32::INFINITY));
            progress.deaths += 1;
            cues.write(SoundCue::Fall(event.position));
            info!("player died ({} so far)", progress.deaths);
        }
    }
}

/// `Update`: counts the respawn timer down, then restores the player.
pub fn run_respawn(
    time: Res<Time>,
    mut respawn: ResMut<Respawn>,
    mut players: Query<(&mut Transform, &mut CharacterState, &mut Health), With<Character>>,
) {
    let Some(left) = respawn.left else {
        return;
    };
    let left = left - time.delta_secs();
    if left > 0.0 {
        respawn.left = Some(left);
        return;
    }
    respawn.left = None;
    for (mut transform, mut state, mut health) in &mut players {
        transform.translation = SPAWN_POINT;
        state.velocity = Vec3::ZERO;
        *health = Health::new(PLAYER_HEALTH);
    }
}

/// Whether the player is dead and waiting to respawn (gameplay input is ignored meanwhile).
#[must_use]
pub fn is_down(respawn: &Respawn) -> bool {
    respawn.left.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_while_the_countdown_runs() {
        assert!(is_down(&Respawn { left: Some(1.0) }));
        assert!(!is_down(&Respawn::default()));
    }

    #[test]
    fn the_policy_sets_the_countdown() {
        assert_eq!(respawn_secs(&RESPAWN, 10.0), Some(2.5));
        assert_eq!(respawn_secs(&RespawnPolicy::Instant, 10.0), Some(0.0));
        assert_eq!(
            respawn_secs(&RespawnPolicy::Waves { period: 600 }, 4.0),
            Some(6.0)
        );
        assert_eq!(respawn_secs(&RespawnPolicy::Never, 4.0), None);
    }
}
