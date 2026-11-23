// SPDX-License-Identifier: MPL-2.0
//! Presentation physics beside an authoritative sim (ADR-8356).
//!
//! The sim owns every unit. This module only decorates the presentation world: kinematic hulls
//! taken from the bridge's snapshot, a static ground collider that matches the drawn ground, and
//! debris that has no sim id. It is not on the lockstep closure and it does not feed bodies,
//! contacts or debris back into the sim.
//!
//! - **Units are kinematic hulls.** Every bridged entity whose `SimKind` is listed in
//!   [`HullShapes`] gets a kinematic body of that shape. The bridge writes the snapshot pose into
//!   `Transform` in `PreUpdate`; physics pushes a kinematic body's `Transform` as its next pose
//!   each fixed step, so the hull moves where the sim says and resting debris rides along through
//!   friction. Physics does not write a kinematic body's `Transform` back.
//! - **A dynamic body with a sim id is refused.** Before the physics step the module drops that
//!   body and counts it on [`PresentationPhysicsStats::refused_writes`]. The bridge keeps the
//!   snapshot `Transform`.
//! - **The ground matches the draw** when the `client` feature links `regia_terrain`.
//!   `PresentationGround::from_terrain` builds the heightfield collider and the hole cells the
//!   draw should skip. Insert the resource before `Startup`.
//! - **Debris has no sim id.** [`DebrisBurst`] messages spawn chunks through `DebrisSpawner`.
//!
//! Add `regia_physics::PhysicsPlugin` and `SimBridgePlugin` yourself. This plugin does not step
//! the sim and does not write the sim's inputs.
//!
//! ```
//! use regia_core::math::Vec3;
//! use regia_framework::sim_bridge::presentation::{HullShapes, PresentationPhysicsPlugin};
//! use regia_physics::ColliderShape;
//!
//! let plugin = PresentationPhysicsPlugin::new(HullShapes::single(
//!     "disc",
//!     ColliderShape::Box {
//!         half_extents: Vec3::new(0.5, 0.05, 0.5),
//!     },
//! ));
//! assert_eq!(plugin.debris_lifetime_s, 30.0);
//! ```

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::{
    BodyBudget, BodyKind, ChunkDesc, Collider, ColliderHandle, ColliderShape, DebrisSpawner, DebrisTag,
    PhysicsPose, PhysicsSettings, PhysicsWorld, PreviousPhysicsPose, RigidBody, RigidBodyHandle, SpawnConfig,
};
#[cfg(feature = "client")]
use regia_scene::Transform;

use super::{SimId, SimKind};

// The `client` feature links `regia_terrain`. There is no separate terrain feature flag.
#[cfg(feature = "client")]
use regia_terrain::{HolesMask, TerrainMap};

/// Which sim kinds get a kinematic hull, and the collider shape of that hull.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct HullShapes {
    /// Kind key (as the bridge's kind registry names it) to any collider shape.
    pub by_kind: HashMap<String, ColliderShape>,
}

impl HullShapes {
    /// One kind with one shape.
    #[must_use]
    pub fn single(kind: &str, shape: ColliderShape) -> Self {
        Self::default().with(kind, shape)
    }

    /// Adds (or replaces) a kind's shape.
    #[must_use]
    pub fn with(mut self, kind: &str, shape: ColliderShape) -> Self {
        self.by_kind.insert(kind.to_string(), shape);
        self
    }
}

/// Spawns a burst of debris chunks (no sim id, dynamic) through `DebrisSpawner`.
#[derive(Message, Debug, Clone)]
pub struct DebrisBurst {
    /// World-space fracture origin.
    pub origin: Vec3,
    /// The chunks, relative to `origin`.
    pub chunks: Vec<ChunkDesc>,
    /// Outward speed, m/s (`SpawnConfig::impulse_strength`; 0 drops the chunks at rest).
    pub speed: f32,
}

/// What presentation physics last reported. Hull and debris counts are live; the rest accumulate.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PresentationPhysicsStats {
    /// Kinematic `RigidBody` entities that also carry a sim id, counted after `Update`.
    pub hull_bodies: u64,
    /// Debris chunks with no sim id, counted after `Update`.
    pub debris_alive: u64,
    /// Dynamic bodies on a sim id that were removed before the physics step.
    pub refused_writes: u64,
    /// Ground colliders spawned from `PresentationGround` (the `client` feature).
    pub ground_colliders: u64,
    /// [`DebrisBurst`] messages handled.
    pub bursts: u64,
    /// Debris chunks spawned.
    pub debris_spawned: u64,
    /// Debris chunks the spawner skipped (over the debris budget, invalid shape or pose).
    pub debris_skipped: u64,
}

/// Marks the static ground entity [`PresentationPhysicsPlugin`] spawned from `PresentationGround`.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct PresentationGroundCollider;

/// The drawn ground as a static collider, plus the cells the draw should skip.
///
/// Available when the `client` feature is on (that feature links `regia_terrain`). Insert this
/// resource before the plugin's `Startup` system runs; the plugin spawns one static collider at
/// the origin. A later insert does not spawn a second collider.
#[cfg(feature = "client")]
#[derive(Resource, Debug, Clone)]
pub struct PresentationGround {
    /// Heightfield collider. Place the body at the world origin (the map is centered there).
    pub collider: Collider,
    /// Cells the collider removes, `(row, col)` with row along Z and col along X. A cell is here
    /// when any of its four heightfield corners is a hole. The draw skips the same cells.
    pub hole_cells: Vec<(u32, u32)>,
}

#[cfg(feature = "client")]
impl PresentationGround {
    /// Collider and hole cells from `map` and `holes`, via `TerrainMap::collision_heightfield`.
    #[must_use]
    pub fn from_terrain(map: &TerrainMap, holes: Option<&HolesMask>) -> Self {
        let field = map.collision_heightfield(holes);
        let rows = field.nx;
        let cols = field.nz;
        let mut hole_cells = Vec::new();
        let cut = |row: usize, col: usize| {
            field
                .heights
                .get(row + col * rows)
                .copied()
                .is_some_and(f32::is_nan)
        };
        for row in 0..rows.saturating_sub(1) {
            for col in 0..cols.saturating_sub(1) {
                if cut(row, col) || cut(row + 1, col) || cut(row, col + 1) || cut(row + 1, col + 1) {
                    hole_cells.push((row as u32, col as u32));
                }
            }
        }
        let collider = Collider {
            shape: ColliderShape::HeightField {
                nx: field.nx,
                nz: field.nz,
                heights: field.heights,
                scale: Vec3::from(field.scale),
            },
            ..Collider::default()
        };
        Self { collider, hole_cells }
    }
}

/// How long this plugin's debris chunks live, in simulated seconds.
#[derive(Resource, Debug, Clone, Copy)]
struct DebrisLifetimeSecs(f64);

/// Presentation physics next to the sim bridge.
///
/// `hulls` selects which sim kinds receive a kinematic collider. `debris_lifetime_s` is the
/// simulated lifetime of chunks spawned from [`DebrisBurst`] (30 seconds unless
/// [`Self::debris_lifetime`] changes it).
#[derive(Debug, Clone)]
pub struct PresentationPhysicsPlugin {
    /// Kind to collider shape for kinematic hulls.
    pub hulls: HullShapes,
    /// Debris lifetime, simulated seconds.
    pub debris_lifetime_s: f64,
}

impl PresentationPhysicsPlugin {
    /// Hulls for the given kinds; debris lives 30 simulated seconds.
    #[must_use]
    pub fn new(hulls: HullShapes) -> Self {
        Self {
            hulls,
            debris_lifetime_s: 30.0,
        }
    }

    /// Sets how long debris chunks live, simulated seconds.
    #[must_use]
    pub fn debris_lifetime(mut self, secs: f64) -> Self {
        self.debris_lifetime_s = secs;
        self
    }
}

impl Plugin for PresentationPhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.hulls.clone())
            .insert_resource(DebrisLifetimeSecs(self.debris_lifetime_s))
            .init_resource::<PresentationPhysicsStats>()
            .add_message::<DebrisBurst>()
            .add_systems(
                FixedUpdate,
                refuse_dynamic_sim_bodies.before(regia_physics::PhysicsStep),
            )
            .add_systems(Update, (attach_hull_bodies, spawn_debris_bursts).chain())
            .add_systems(PostUpdate, refresh_presentation_counts);
        #[cfg(feature = "client")]
        app.add_systems(Startup, spawn_ground_collider);
    }
}

/// `Startup`: one static ground collider from [`PresentationGround`], at the origin.
#[cfg(feature = "client")]
fn spawn_ground_collider(
    ground: Option<Res<PresentationGround>>,
    existing: Query<(), With<PresentationGroundCollider>>,
    mut commands: Commands,
    mut stats: ResMut<PresentationPhysicsStats>,
) {
    let Some(ground) = ground else {
        return;
    };
    if !existing.is_empty() {
        return;
    }
    commands.spawn((
        regia_scene::components::Name::new("presentation_ground_collider"),
        PresentationGroundCollider,
        Transform::IDENTITY,
        RigidBody::static_body(),
        ground.collider.clone(),
    ));
    stats.ground_colliders = stats.ground_colliders.saturating_add(1);
}

/// `FixedUpdate`, before the physics step: a dynamic body on a sim id is removed.
///
/// Both forms count: a `RigidBody` whose kind is dynamic, and a legacy `BodyKind::Dynamic`
/// with no `RigidBody` (the debris spawner writes that second form). Removing the handle
/// records the rapier body so the step drops it. A hull kind is attached again as kinematic
/// in `Update`.
fn refuse_dynamic_sim_bodies(world: &mut World) {
    let rigid = {
        let mut query = world.query_filtered::<(Entity, &RigidBody), With<SimId>>();
        let world_ref: &World = world;
        query
            .iter(world_ref)
            .filter(|(_, body)| body.kind == BodyKind::Dynamic)
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>()
    };
    let legacy = {
        let mut query = world.query_filtered::<(Entity, &BodyKind), (With<SimId>, Without<RigidBody>)>();
        let world_ref: &World = world;
        query
            .iter(world_ref)
            .filter(|(_, kind)| **kind == BodyKind::Dynamic)
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>()
    };
    let count = rigid.len() + legacy.len();
    if count == 0 {
        return;
    }
    let _span = tracing::debug_span!(
        target: "regia_framework",
        "refuse_dynamic_sim_bodies",
        count
    )
    .entered();
    tracing::warn!(
        target: "regia_framework",
        count,
        "presentation physics refused a dynamic body on a sim entity; the sim owns that Transform"
    );
    if let Some(mut stats) = world.get_resource_mut::<PresentationPhysicsStats>() {
        stats.refused_writes = stats.refused_writes.saturating_add(count as u64);
    }
    for entity in rigid.into_iter().chain(legacy) {
        if let Ok(mut entity) = world.get_entity_mut(entity) {
            entity.remove::<(
                RigidBody,
                BodyKind,
                PhysicsPose,
                PreviousPhysicsPose,
                RigidBodyHandle,
                Collider,
                ColliderHandle,
            )>();
        }
    }
}

/// Bridged sim entities that have no physics body yet.
type UnitsWithoutBody = (With<SimId>, Without<RigidBody>);

/// `Update`: a kinematic collider for every bridged entity whose kind is in [`HullShapes`].
fn attach_hull_bodies(
    hulls: Res<HullShapes>,
    new_units: Query<(Entity, &SimKind), UnitsWithoutBody>,
    mut commands: Commands,
) {
    for (entity, kind) in &new_units {
        let Some(shape) = hulls.by_kind.get(&kind.0) else {
            continue;
        };
        commands.entity(entity).insert((
            RigidBody::kinematic(),
            Collider {
                shape: shape.clone(),
                ..Collider::default()
            },
        ));
    }
}

/// `Update`: spawns each [`DebrisBurst`] through `DebrisSpawner`.
#[tracing::instrument(level = "debug", skip_all, target = "regia_framework")]
fn spawn_debris_bursts(
    mut bursts: MessageReader<DebrisBurst>,
    mut commands: Commands,
    mut physics: ResMut<PhysicsWorld>,
    mut budget: ResMut<BodyBudget>,
    settings: Res<PhysicsSettings>,
    lifetime: Res<DebrisLifetimeSecs>,
    mut stats: ResMut<PresentationPhysicsStats>,
) {
    let spawner = DebrisSpawner::new(lifetime.0);
    for burst in bursts.read() {
        let result = spawner.spawn(
            &mut commands,
            &mut physics,
            &mut budget,
            &settings,
            &SpawnConfig {
                chunks: &burst.chunks,
                origin: burst.origin,
                impulse_strength: burst.speed,
            },
        );
        stats.bursts = stats.bursts.saturating_add(1);
        stats.debris_spawned = stats.debris_spawned.saturating_add(u64::from(result.spawned));
        stats.debris_skipped = stats.debris_skipped.saturating_add(u64::from(result.skipped));
    }
}

/// `PostUpdate`: live hull and debris counts. Cumulative counters are left alone.
fn refresh_presentation_counts(
    mut stats: ResMut<PresentationPhysicsStats>,
    bodies: Query<&RigidBody, With<SimId>>,
    debris: Query<(), (With<DebrisTag>, Without<SimId>)>,
) {
    stats.hull_bodies = bodies
        .iter()
        .filter(|body| body.kind == BodyKind::Kinematic)
        .count() as u64;
    stats.debris_alive = debris.iter().count() as u64;
}

/// One cube chunk of half size `half` at `local` (relative to the burst origin), `mass` kg.
#[must_use]
pub fn cube_chunk(local: Vec3, half: f32, mass: f32) -> ChunkDesc {
    ChunkDesc {
        local_translation: local,
        local_rotation: Quat::IDENTITY,
        shape: ColliderShape::Box {
            half_extents: Vec3::splat(half),
        },
        mass,
    }
}
