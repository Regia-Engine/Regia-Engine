// SPDX-License-Identifier: MPL-2.0
//! What a process is playing, in a form two processes can compare (ADR-2573).
//!
//! The server decides breaks and clients replay them, so the broken bond set has to match
//! everywhere. Debris motion is each process's own: it matches only while the processes run the same
//! steps from the same stream, which is what a lockstep test wants to know.

use regia_physics::convert::from_rapier_pose;
use regia_physics::destruction::{ChunkState, DestructibleId, Destruction};
use regia_physics::world::PhysicsWorld;
use serde::{Serialize, Serializer};

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

const FNV_START: u64 = 0xCBF2_9CE4_8422_2325;

fn as_hex<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{value:#018x}"))
}

/// One instance in a [`DestructionPlayback`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstancePlayback {
    /// The instance.
    pub id: u32,
    /// Asset name.
    pub asset: String,
    /// Chunks still part of the structure.
    pub attached: u32,
    /// Chunks that left it, whether they still have a body or not.
    pub detached: u32,
    /// Chunks with a debris body right now.
    pub debris: u32,
    /// Broken bonds, ascending.
    pub broken_bonds: Vec<u32>,
}

/// What this process plays. Equal on two processes means they show the same chunks in the same
/// places, bit for bit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DestructionPlayback {
    /// The break model's tick here. Compare two clients at the same tick. A replica takes its tick
    /// from the snapshot it joined with, so it can be a tick off the server's.
    pub tick: u64,
    /// Every instance, in id order.
    pub instances: Vec<InstancePlayback>,
    /// Broken bonds over all instances.
    pub broken_bond_count: u64,
    /// FNV-1a 64 of (instance, broken bond) in order. Hex in JSON.
    #[serde(serialize_with = "as_hex")]
    pub broken_bond_digest: u64,
    /// Chunks with a debris body over all instances.
    pub debris_count: u32,
    /// FNV-1a 64 of each instance's pose and chunk states, then each debris body's chunk and exact
    /// translation and rotation, in (instance, chunk) order. An attached chunk's pose follows from
    /// its instance's, so this covers every chunk. Hex in JSON.
    #[serde(serialize_with = "as_hex")]
    pub chunk_pose_digest: u64,
    /// The break model's `structure_hash`. Hex in JSON.
    #[serde(serialize_with = "as_hex")]
    pub structure_hash: u64,
}

/// Reads what `destruction` plays. Without a `physics` world no debris body is read: every debris
/// chunk counts as gone (a replica may run without rapier).
#[must_use]
pub fn destruction_playback(
    destruction: &Destruction,
    physics: Option<&PhysicsWorld>,
) -> DestructionPlayback {
    let mut debris: Vec<_> = destruction.physics.active().copied().collect();
    debris.sort_by_key(|a| (a.destructible, a.chunk));
    let mut bonds_digest = FNV_START;
    let mut pose_digest = FNV_START;
    let mut instances = Vec::new();
    let mut broken_bond_count = 0;
    let mut debris_count = 0;
    for (id, inst) in destruction.world.instances() {
        let broken_bonds: Vec<u32> = (0u32..)
            .zip(inst.bond_broken())
            .filter(|(_, broken)| **broken)
            .map(|(bond, _)| bond)
            .collect();
        for bond in &broken_bonds {
            fnv(&mut bonds_digest, &id.0.to_le_bytes());
            fnv(&mut bonds_digest, &bond.to_le_bytes());
        }
        broken_bond_count += broken_bonds.len() as u64;
        let pose = inst.pose();
        fnv(&mut pose_digest, &id.0.to_le_bytes());
        for c in pose.translation.into_iter().chain(pose.rotation) {
            fnv(&mut pose_digest, &c.to_bits().to_le_bytes());
        }
        let mut detached = 0;
        for state in inst.chunk_states() {
            let attached = *state == ChunkState::Attached;
            detached += u32::from(!attached);
            fnv(&mut pose_digest, &[u8::from(attached)]);
        }
        let moving = debris_of(&debris, id, physics, &mut pose_digest);
        debris_count += moving;
        instances.push(InstancePlayback {
            id: id.0,
            asset: inst.asset().name.clone(),
            attached: inst.chunk_states().len() as u32 - detached,
            detached,
            debris: moving,
            broken_bonds,
        });
    }
    DestructionPlayback {
        tick: destruction.world.tick(),
        instances,
        broken_bond_count,
        broken_bond_digest: bonds_digest,
        debris_count,
        chunk_pose_digest: pose_digest,
        structure_hash: destruction.world.structure_hash(),
    }
}

/// Folds the debris bodies of one instance into `digest` and counts them.
fn debris_of(
    debris: &[regia_physics::destruction::ActiveChunk],
    id: DestructibleId,
    physics: Option<&PhysicsWorld>,
    digest: &mut u64,
) -> u32 {
    let Some(physics) = physics else {
        return 0;
    };
    let mut count = 0;
    for a in debris.iter().filter(|a| a.destructible == id) {
        let Some(rb) = physics.bodies.get(a.body) else {
            continue;
        };
        let (translation, rotation) = from_rapier_pose(rb.position());
        fnv(digest, &a.chunk.to_le_bytes());
        for c in translation.to_array().into_iter().chain(rotation.to_array()) {
            fnv(digest, &c.to_bits().to_le_bytes());
        }
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_physics::destruction::{
        Authority, BoxGrid, Damage, DamageKind, DestructibleAsset, DestructiblePose, DestructionMaterial,
        GridSupport,
    };

    use super::*;

    fn wall() -> Arc<DestructibleAsset> {
        Arc::new(
            DestructibleAsset::box_grid(&BoxGrid {
                name: "wall".into(),
                counts: [3, 2, 1],
                cell: [0.5, 0.5, 0.5],
                density: 2000.0,
                strength_per_area: 200.0,
                support: GridSupport::Bottom,
                material: DestructionMaterial::default(),
            })
            .expect("grid"),
        )
    }

    fn scene() -> (Destruction, PhysicsWorld) {
        let mut d = Destruction::new(Authority::Server);
        let mut physics = PhysicsWorld::default();
        d.define(&mut physics, wall(), DestructiblePose::at([0.0, 0.0, 0.0]), 7)
            .expect("define");
        (d, physics)
    }

    fn blast(d: &mut Destruction, physics: &mut PhysicsWorld) {
        d.world
            .damage(Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: [0.75, 1.5, 0.75],
                    radius: 8.0,
                    impulse: 1000.0,
                },
            })
            .expect("damage");
        d.world.step(1.0 / 60.0);
        d.physics.sync(physics, &mut d.world, 1.0 / 60.0);
    }

    #[test]
    fn a_whole_structure_reports_no_broken_bond_and_no_debris() {
        let (d, physics) = scene();
        let p = destruction_playback(&d, Some(&physics));
        assert_eq!(p.broken_bond_count, 0);
        assert_eq!(p.debris_count, 0);
        assert_eq!(p.instances.len(), 1);
        assert_eq!(p.instances[0].attached, 6);
        assert_eq!(p.instances[0].detached, 0);
        assert_eq!(p.instances[0].asset, "wall");
    }

    #[test]
    fn the_report_lists_what_broke_and_what_flies() {
        let (mut d, mut physics) = scene();
        let before = destruction_playback(&d, Some(&physics));
        blast(&mut d, &mut physics);
        let after = destruction_playback(&d, Some(&physics));
        assert!(after.broken_bond_count > 0);
        assert!(after.debris_count > 0);
        assert_eq!(
            after.instances[0].broken_bonds.len() as u64,
            after.broken_bond_count
        );
        assert_eq!(
            after.instances[0].attached + after.instances[0].detached,
            6,
            "{after:?}"
        );
        assert_ne!(before.broken_bond_digest, after.broken_bond_digest);
        assert_ne!(before.chunk_pose_digest, after.chunk_pose_digest);
    }

    #[test]
    fn the_same_inputs_give_the_same_digests_and_a_nudge_changes_only_the_pose_digest() {
        let run = || {
            let (mut d, mut physics) = scene();
            blast(&mut d, &mut physics);
            (d, physics)
        };
        let (a, pa) = run();
        let (b, mut pb) = run();
        let first = destruction_playback(&a, Some(&pa));
        assert_eq!(first, destruction_playback(&b, Some(&pb)));
        // One debris body a hair off: the set of bonds is the same, the pose digest is not.
        let chunk = b.physics.active().next().copied().expect("debris");
        let rb = pb.bodies.get_mut(chunk.body).expect("body");
        let mut at = *rb.position();
        at.translation.x += 1e-4;
        rb.set_position(at, true);
        let nudged = destruction_playback(&b, Some(&pb));
        assert_eq!(first.broken_bond_digest, nudged.broken_bond_digest);
        assert_eq!(first.structure_hash, nudged.structure_hash);
        assert_ne!(first.chunk_pose_digest, nudged.chunk_pose_digest);
    }

    #[test]
    fn digests_are_hex_strings_in_json_so_no_tool_rounds_them() {
        let (d, physics) = scene();
        let json = serde_json::to_value(destruction_playback(&d, Some(&physics))).expect("json");
        for field in ["broken_bond_digest", "chunk_pose_digest", "structure_hash"] {
            let text = json[field].as_str().expect(field);
            assert!(text.starts_with("0x") && text.len() == 18, "{field}: {text}");
        }
    }

    #[test]
    fn without_a_physics_world_no_debris_is_read() {
        let (mut d, mut physics) = scene();
        blast(&mut d, &mut physics);
        let p = destruction_playback(&d, None);
        assert!(p.broken_bond_count > 0);
        assert_eq!(p.debris_count, 0);
    }
}
