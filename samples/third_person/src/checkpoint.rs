// SPDX-License-Identifier: MIT-0
//! Checkpoints and combat progress (ADR-2596). Standing on a checkpoint pad makes it the respawn
//! point; [`CombatProgress`] (a saveable resource) remembers the active checkpoint and the
//! defeated enemies, so a save keeps both. The pads are a `regia_rules::race` course
//! ([`course`], ADR-8695): each pad is a checkpoint volume and the touch test is the race's
//! volume test.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::Character;
use regia_framework::prelude::SaveableResource;
use regia_framework::rules::objective::HillVolume;
use regia_framework::rules::race::{Checkpoint as CourseGate, RaceSettings};
use regia_scene::Transform;

/// Checkpoint pads: ground center of each (index order). Pad 0 is the start.
pub const PADS: [Vec3; 3] = [
    Vec3::new(-4.0, 0.25, 0.0),
    Vec3::new(6.0, 0.25, 8.0),
    Vec3::new(22.0, 1.2, 0.0),
];

/// A pad within this horizontal distance (on each axis) activates (m).
pub const PAD_RADIUS: f32 = 1.4;

/// Half the height band a pad reaches above and below its ground center (m).
pub const PAD_REACH: f32 = 2.5;

/// The pads as a race course: pad `i` is checkpoint `i`, its volume the pad's footprint and
/// height band, and its `next` the following pad (the last links back to the start).
pub fn course() -> RaceSettings {
    let n = PADS.len() as u32;
    RaceSettings {
        checkpoints: PADS
            .iter()
            .enumerate()
            .map(|(i, pad)| CourseGate {
                id: i as u32,
                volume: HillVolume {
                    min: [pad.x - PAD_RADIUS, pad.y - PAD_REACH, pad.z - PAD_RADIUS],
                    max: [pad.x + PAD_RADIUS, pad.y + PAD_REACH, pad.z + PAD_RADIUS],
                },
                next: vec![(i as u32 + 1) % n],
            })
            .collect(),
        ..RaceSettings::default()
    }
}

/// Saveable combat progress.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct CombatProgress {
    /// The active checkpoint (index into [`PADS`]).
    pub checkpoint: u32,
    /// Ids of defeated enemies (`enemy::roster`), in defeat order.
    pub defeated: Vec<u32>,
}

impl SaveableResource for CombatProgress {}

/// A checkpoint pad entity.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct Checkpoint {
    /// Index into [`PADS`].
    pub index: u32,
}

/// Seconds the "Checkpoint reached" toast stays up.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct CheckpointToast(pub f32);

/// `Startup`: the pads (flat boxes; no collider, they are only markers).
pub fn setup(world: &mut World) {
    for (i, pad) in PADS.iter().enumerate() {
        world.spawn((
            Transform::from_translation(*pad + Vec3::new(0.0, 0.03, 0.0)),
            regia_render::MeshRenderer::new(
                crate::level::box_mesh(Vec3::new(0.9, 0.03, 0.9)),
                crate::level::solid_material([0.2, 0.55, 0.9, 1.0]),
            ),
            Checkpoint { index: i as u32 },
            Name::new(format!("checkpoint-{i}")),
        ));
    }
}

/// Where a downed character respawns: above the active pad.
pub fn respawn_point(world: &World) -> Vec3 {
    let index = world
        .get_resource::<CombatProgress>()
        .map_or(0, |p| p.checkpoint as usize)
        .min(PADS.len() - 1);
    PADS[index] + Vec3::new(0.0, 1.1, 0.0)
}

/// `Update`: a living player character stepping onto a pad makes it the active checkpoint. Only
/// stepping *onto* a pad counts: the pad a character stands on when it is first seen (the start
/// pad, or wherever a load or respawn put it) does not, so loading a save made at checkpoint 1
/// while standing on the start pad keeps checkpoint 1.
#[tracing::instrument(skip_all, target = "third_person", name = "checkpoints")]
pub fn touch_pads(world: &mut World, mut on_pad: Local<std::collections::BTreeMap<Entity, Option<u32>>>) {
    let dt = world.resource::<Time>().delta_secs();
    if let Some(mut toast) = world.get_resource_mut::<CheckpointToast>()
        && toast.0 > 0.0
    {
        toast.0 = (toast.0 - dt).max(0.0);
    }
    let positions: Vec<(Entity, Vec3)> = world
        .query_filtered::<(Entity, &Transform), With<Character>>()
        .iter(world)
        .map(|(e, t)| (e, t.translation))
        .collect();
    on_pad.retain(|e, _| positions.iter().any(|(p, _)| p == e));
    let course = course();
    for (character, at) in positions {
        if crate::combat::is_down(world, character) {
            continue;
        }
        let here = course
            .checkpoints
            .iter()
            .find(|gate| gate.volume.contains(at.to_array()))
            .map(|gate| gate.id);
        let Some(before) = on_pad.insert(character, here) else {
            continue; // first sight of this character: no activation
        };
        let current = world.get_resource::<CombatProgress>().map_or(0, |p| p.checkpoint);
        if let Some(i) = here
            && before != Some(i)
            && i != current
        {
            if let Some(mut progress) = world.get_resource_mut::<CombatProgress>() {
                progress.checkpoint = i;
            }
            world.insert_resource(CheckpointToast(2.0));
            tracing::info!(target: "third_person", "checkpoint {i} reached");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pads_are_a_valid_race_course() {
        let course = course();
        assert_eq!(course.validate(), Ok(()));
        for (i, pad) in PADS.iter().enumerate() {
            let hit = course
                .checkpoints
                .iter()
                .find(|gate| gate.volume.contains(pad.to_array()))
                .map(|gate| gate.id);
            assert_eq!(hit, Some(i as u32), "pad {i}");
        }
        assert_eq!(
            course.checkpoints[PADS.len() - 1].next,
            vec![0],
            "the last pad loops to the start"
        );
    }
}
