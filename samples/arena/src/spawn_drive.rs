// SPDX-License-Identifier: MIT-0
//! The `spawns` scripted drive (`play.hitch_report { "drive": "spawns" }`,
//! `book/agents/hitches.md`): while [`SpawnDrive::active`], every [`SpawnDrive::every_s`] the
//! arena spawns what a game spawns on an event — a crate (mesh, material, rigid body, collider),
//! a one-shot effect, a scorch decal and a positional sound — and despawns the oldest crate past
//! [`SpawnDrive::max_live`].
//!
//! The crates share the arena's mesh and material and the effects come from the showcase
//! library, so after the warm-up no spawn should create a GPU resource: the hitch report's
//! `device_calls` stay zero and every frame stays under 50 ms (`specs/27` § 2.4). A game maps the
//! agent's `HitchDrive { name: "spawns" }` onto [`SpawnDrive::active`]
//! (`samples/arena/tests/spawn_hitch.rs`).

use std::collections::VecDeque;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};

use crate::audio::ArenaAudio;
use crate::meshes::ArenaAssets;

/// The drive's name in `play.hitch_report { "drive": … }`.
pub const DRIVE_NAME: &str = "spawns";

/// The spawn drive's state.
#[derive(Resource, Debug, Clone)]
pub struct SpawnDrive {
    /// Spawning while true.
    pub active: bool,
    /// Seconds between spawns.
    pub every_s: f32,
    /// Crates kept alive; the oldest is despawned past this.
    pub max_live: usize,
    effects: Vec<String>,
    next_s: Option<f32>,
    spawned: u64,
    live: VecDeque<Entity>,
}

impl Default for SpawnDrive {
    fn default() -> Self {
        Self {
            active: false,
            every_s: 0.25,
            max_live: 48,
            effects: regia_vfx::showcase_effects()
                .into_iter()
                .map(|def| def.name)
                .collect(),
            next_s: None,
            spawned: 0,
            live: VecDeque::new(),
        }
    }
}

impl SpawnDrive {
    /// Spawn events so far.
    #[must_use]
    pub fn spawned(&self) -> u64 {
        self.spawned
    }

    /// Where spawn `n` lands: a golden-angle spiral over the arena floor (deterministic).
    #[must_use]
    pub fn position(n: u64) -> Vec3 {
        let angle = n as f32 * 2.399_963;
        let radius = 2.0 + (n % 6) as f32;
        Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius)
    }
}

/// Installs [`SpawnDrive`] (inactive) and [`drive_spawns`].
pub struct SpawnDrivePlugin;

impl Plugin for SpawnDrivePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpawnDrive>()
            .add_systems(Update, drive_spawns);
    }
}

/// `Update`: one spawn event when the drive is active and its period has elapsed.
pub fn drive_spawns(world: &mut World) {
    let now = world.resource::<Time>().elapsed_secs();
    let n = {
        let Some(mut drive) = world.get_resource_mut::<SpawnDrive>() else {
            return;
        };
        if !drive.active {
            drive.next_s = None;
            return;
        }
        let next = *drive.next_s.get_or_insert(now);
        if now < next {
            return;
        }
        drive.next_s = Some(next + drive.every_s.max(0.01));
        drive.spawned += 1;
        drive.spawned - 1
    };
    let _span = tracing::info_span!("spawn_drive", n).entered();
    let at = SpawnDrive::position(n);

    // The crate: the arena's shared mesh and material, a dynamic body and a box collider.
    let mesh_material = world
        .get_resource::<ArenaAssets>()
        .map(|a| (a.crate_mesh.clone(), a.crate_material.clone()));
    let entity = crate::props::spawn_crate(world, 1_000 + n as usize, at.x, at.z, mesh_material.as_ref());

    // A showcase effect and a scorch under it.
    let effect = world.get_resource::<SpawnDrive>().and_then(|drive| {
        drive
            .effects
            .get(n as usize % drive.effects.len().max(1))
            .cloned()
    });
    if let Some(effect) = effect {
        world.write_message(regia_vfx::SpawnEffect {
            effect,
            position: at + Vec3::Y * 0.5,
            forward: Vec3::Y,
            seed: Some(n),
        });
    }
    world.write_message(regia_vfx::SpawnDecal {
        position: at + Vec3::Y * 0.01,
        rotation: Quat::from_rotation_arc(Vec3::Z, Vec3::Y),
        half_size: Vec3::new(1.0, 1.0, 0.5),
        color: [0.05, 0.04, 0.03, 0.85],
        lifetime: Some(20.0),
    });

    // A positional one-shot.
    let clip = world.get_resource::<ArenaAudio>().map(|a| a.explosion.clone());
    if let (Some(clip), Some(mut audio)) = (clip, world.get_resource_mut::<regia_audio::Audio>()) {
        let _ = audio.play_at(&clip, at);
    }

    // Keep the live set bounded: the oldest crate goes.
    let stale = {
        let Some(mut drive) = world.get_resource_mut::<SpawnDrive>() else {
            return;
        };
        drive.live.push_back(entity);
        let mut stale = Vec::new();
        while drive.live.len() > drive.max_live {
            if let Some(old) = drive.live.pop_front() {
                stale.push(old);
            }
        }
        stale
    };
    for old in stale {
        world.despawn(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_positions_stay_on_the_floor_inside_the_arena() {
        for n in 0..200 {
            let p = SpawnDrive::position(n);
            assert_eq!(p.y, 0.0);
            assert!(p.length() >= 2.0 - 1e-4 && p.length() <= 7.0 + 1e-4, "{n}: {p}");
        }
        assert_ne!(SpawnDrive::position(0), SpawnDrive::position(1));
    }

    #[test]
    fn the_drive_knows_effects_to_spawn() {
        let drive = SpawnDrive::default();
        assert!(!drive.effects.is_empty());
        assert!(!drive.active);
    }
}
