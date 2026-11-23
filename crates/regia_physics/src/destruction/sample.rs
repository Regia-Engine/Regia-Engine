// SPDX-License-Identifier: MPL-2.0
//! The destruction sample scene: a brick wall and a plank bridge, and a fixed hit script that
//! breaks both (a shot through the wall, a blast against it, a blast that cuts the bridge deck in
//! two, and a hit next to the left abutment that drops the left half).
//!
//! [`run_sample_headless`] runs the break model alone (no rapier, no GPU) and returns the hashes
//! the determinism tests pin; `examples/destruction_scene.rs` and the agent's
//! `physics.destruction_define { sample: "wall_bridge" }` use the same scene.

use std::sync::Arc;

use super::asset::{BoxGrid, DestructibleAsset, DestructibleError, DestructionMaterial, GridSupport};
use super::sim::{
    Authority, BreakEvent, Damage, DamageKind, DestructibleId, DestructiblePose, DestructionError,
    DestructionWorld,
};

/// Fixed tick rate of the sample script.
pub const SAMPLE_HZ: f32 = 60.0;

/// Ticks the sample script spans (the last hit lands on tick 90).
pub const SAMPLE_TICKS: u64 = 120;

/// Seeds of the two sample instances.
pub const SAMPLE_SEEDS: [u64; 2] = [0x5741_4C4C, 0x4252_4944];

/// An 8 x 8 brick wall, 4 m wide and 2 m tall, standing on the ground (bottom row supported).
///
/// # Errors
/// Never in practice (the parameters are valid); the grid builder's error otherwise.
pub fn wall_asset() -> Result<DestructibleAsset, DestructibleError> {
    DestructibleAsset::box_grid(&BoxGrid {
        name: "sample_wall".into(),
        counts: [8, 8, 1],
        cell: [0.5, 0.25, 0.25],
        density: 2000.0,
        strength_per_area: 1000.0,
        support: GridSupport::Bottom,
        material: DestructionMaterial {
            fx_tag: "brick".into(),
            ..DestructionMaterial::default()
        },
    })
}

/// A 12 m bridge deck of 12 x 2 planks resting on two abutments (first and last columns
/// supported).
///
/// # Errors
/// Never in practice (the parameters are valid); the grid builder's error otherwise.
pub fn bridge_asset() -> Result<DestructibleAsset, DestructibleError> {
    DestructibleAsset::box_grid(&BoxGrid {
        name: "sample_bridge".into(),
        counts: [12, 1, 2],
        cell: [1.0, 0.3, 1.0],
        density: 600.0,
        strength_per_area: 1000.0,
        support: GridSupport::Ends,
        material: DestructionMaterial {
            fx_tag: "wood".into(),
            ..DestructionMaterial::default()
        },
    })
}

/// Where the sample places the wall.
pub const WALL_POSE: [f32; 3] = [0.0, 0.0, -5.0];

/// Where the sample places the bridge deck.
pub const BRIDGE_POSE: [f32; 3] = [0.0, 3.0, 5.0];

/// The two sample instances in a world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleIds {
    /// The wall.
    pub wall: DestructibleId,
    /// The bridge.
    pub bridge: DestructibleId,
}

/// Defines the wall and the bridge (in that order) with the sample seeds.
///
/// # Errors
/// See [`DestructionWorld::define`].
pub fn define_sample(world: &mut DestructionWorld) -> Result<SampleIds, DestructionError> {
    let (wall, bridge) = sample_assets()?;
    let wall = world.define(wall, DestructiblePose::at(WALL_POSE), SAMPLE_SEEDS[0])?;
    let bridge = world.define(bridge, DestructiblePose::at(BRIDGE_POSE), SAMPLE_SEEDS[1])?;
    Ok(SampleIds { wall, bridge })
}

/// The two sample assets, shared.
///
/// # Errors
/// The grid builder's error (never with these parameters).
pub fn sample_assets() -> Result<(Arc<DestructibleAsset>, Arc<DestructibleAsset>), DestructionError> {
    Ok((Arc::new(wall_asset()?), Arc::new(bridge_asset()?)))
}

/// The hits that land on `tick` (1-based: the first step is tick 1).
#[must_use]
pub fn sample_hits(tick: u64, ids: SampleIds) -> Vec<Damage> {
    match tick {
        // A heavy round through the wall at (0.1, 1.1): knocks one brick out.
        10 => vec![Damage {
            target: None,
            kind: DamageKind::Projectile {
                origin: [0.1, 1.1, 5.0],
                direction: [0.0, 0.0, -1.0],
                impulse: 1500.0,
                radius: 0.3,
                max_distance: 50.0,
            },
        }],
        // A charge 0.4 m in front of the wall's middle.
        30 => vec![Damage {
            target: Some(ids.wall),
            kind: DamageKind::Explosion {
                center: [0.0, 1.0, -4.6],
                radius: 1.2,
                impulse: 1500.0,
            },
        }],
        // A blast on the middle of the deck cuts it in two; each half still rests on its abutment.
        50 => vec![Damage {
            target: Some(ids.bridge),
            kind: DamageKind::Explosion {
                center: [0.0, 3.15, 5.0],
                radius: 1.6,
                impulse: 3000.0,
            },
        }],
        // Downward hits on both planks next to the left abutment: they break loose and the left
        // half of the deck, now held by nothing, collapses.
        70 => [1u32, 13]
            .into_iter()
            .map(|chunk| Damage {
                target: Some(ids.bridge),
                kind: DamageKind::Impulse {
                    chunk,
                    impulse: [0.0, -1500.0, 0.0],
                },
            })
            .collect(),
        // A charge at the wall's lower left.
        90 => vec![Damage {
            target: Some(ids.wall),
            kind: DamageKind::Explosion {
                center: [-1.5, 0.2, -4.7],
                radius: 1.0,
                impulse: 1250.0,
            },
        }],
        _ => Vec::new(),
    }
}

/// The result of [`run_sample_headless`].
#[derive(Clone, Debug, PartialEq)]
pub struct SampleRun {
    /// [`DestructionWorld::structure_hash`] at the end.
    pub structure_hash: u64,
    /// [`DestructionWorld::state_hash`] at the end.
    pub state_hash: u64,
    /// Every break event, in order.
    pub events: Vec<BreakEvent>,
    /// Events per tick (index = tick - 1), for replaying them tick by tick.
    pub events_per_tick: Vec<usize>,
    /// Attached chunks left: wall, bridge.
    pub attached: [usize; 2],
    /// The instances.
    pub ids: SampleIds,
}

/// Runs the sample script for `ticks` fixed ticks on the break model alone.
///
/// # Errors
/// A define or damage error (never with the sample's parameters).
pub fn run_sample_headless(ticks: u64) -> Result<SampleRun, DestructionError> {
    let mut world = DestructionWorld::new(Authority::Server);
    let ids = define_sample(&mut world)?;
    let mut events = Vec::new();
    let mut events_per_tick = Vec::with_capacity(ticks as usize);
    for tick in 1..=ticks {
        for hit in sample_hits(tick, ids) {
            world.damage(hit)?;
        }
        let report = world.step(1.0 / SAMPLE_HZ);
        events_per_tick.push(report.events.len());
        events.extend(report.events);
        // No rapier here: drop the spawn requests so the queue stays bounded.
        world.take_spawns(0);
    }
    let attached = |id| world.instance(id).map_or(0, |i| i.attached_count());
    Ok(SampleRun {
        structure_hash: world.structure_hash(),
        state_hash: world.state_hash(),
        attached: [attached(ids.wall), attached(ids.bridge)],
        events,
        events_per_tick,
        ids,
    })
}
