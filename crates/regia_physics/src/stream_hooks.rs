// SPDX-License-Identifier: MPL-2.0
//! Hooks of the world streaming runtime (`docs/specs/78-world-streaming.md` §§ 10.3, 10.4, 12.2).
//! Physics depends on `regia_scene`'s streaming types, never on the runtime crate.
//!
//! - While a streamed level is open, dynamic bodies and characters carry [`StreamGuarded`], so the
//!   runtime's guard can find them. Without one nothing is tagged.
//! - A body with [`StreamFrozen`] is disabled (no integration, no contacts) and keeps its velocity.
//! - The `physics` gate marks a cell once its colliders exist. Sync is total, so that is the first
//!   step after the cell spawned; `StreamGates::awaiting` is used instead of the `CellLoaded`
//!   message because a fixed step does not run every frame and messages expire.
//! - A rebase moves every body, static collider, stored pose and cloth particle.

use rapier3d::prelude::IntegrationParameters;
use regia_ecs::prelude::*;
use regia_scene::streaming::{
    OriginShifted, Readiness, StreamFrozen, StreamGates, StreamGuarded, StreamTeleported, StreamingActive,
    TERRAIN_COLLISION_GATE,
};
use tracing::instrument;

use crate::cloth::ClothSet;
use crate::components::{
    BodyKind, CharacterBody, Collider, ColliderShape, PhysicsPose, PreviousPhysicsPose, RigidBody, Teleport,
};
use crate::convert::{from_rapier_vec3, to_rapier_vec3};
use crate::world::PhysicsWorld;
use regia_core::math::Vec3;
use regia_scene::level::LevelMember;
use regia_scene::streaming::GateId;

/// The `physics` gate's id.
#[derive(Resource, Debug, Clone, Copy)]
pub(crate) struct PhysicsStreamGate(pub(crate) GateId);

/// Freeze changes since the last step. Observers record them: removal messages are cleared every
/// frame and would be lost when several frames pass without a fixed step.
#[derive(Resource, Debug, Default)]
pub(crate) struct FrozenChanges {
    frozen: Vec<Entity>,
    woken: Vec<Entity>,
}

pub(crate) fn record_frozen(on: On<Add<StreamFrozen>>, mut changes: ResMut<FrozenChanges>) {
    changes.frozen.push(on.event().entity);
}

pub(crate) fn record_woken(on: On<Remove<StreamFrozen>>, mut changes: ResMut<FrozenChanges>) {
    changes.woken.push(on.event().entity);
}

/// Registers the `physics` gate.
pub(crate) fn register_gate(world: &mut World) {
    let gate = world
        .get_resource_or_init::<StreamGates>()
        .register("physics", Readiness::SimReady);
    world.insert_resource(PhysicsStreamGate(gate));
}

/// Keeps [`StreamGuarded`] on dynamic bodies and characters, and only on them: kinematic bodies and
/// static colliders are never frozen. Only while a streamed level is open ([`StreamingActive`]): the
/// first step after one opens tags every body already there, later steps the bodies that changed.
#[instrument(skip_all, target = "regia_physics")]
pub(crate) fn mark_stream_guarded(
    active: Option<Res<StreamingActive>>,
    bodies: Query<(Entity, Ref<RigidBody>, Has<StreamGuarded>)>,
    characters: Query<Entity, (With<CharacterBody>, Without<StreamGuarded>)>,
    mut commands: Commands,
) {
    let Some(active) = active.filter(|a| a.0) else {
        return;
    };
    let sweep = active.is_changed();
    for (entity, body, guarded) in &bodies {
        if !sweep && !body.is_changed() {
            continue;
        }
        let dynamic = body.kind == BodyKind::Dynamic;
        if dynamic && !guarded {
            commands.entity(entity).insert(StreamGuarded);
        } else if !dynamic && guarded {
            commands.entity(entity).remove::<(StreamGuarded, StreamFrozen)>();
        }
    }
    for entity in &characters {
        commands.entity(entity).insert(StreamGuarded);
    }
}

/// Turns the runtime's teleport request into the physics teleport.
pub(crate) fn apply_stream_teleports(moved: Query<Entity, With<StreamTeleported>>, mut commands: Commands) {
    for entity in &moved {
        commands
            .entity(entity)
            .remove::<StreamTeleported>()
            .insert(Teleport);
    }
}

/// Disables frozen bodies and enables woken ones. Runs after the bodies synced, so a body frozen on
/// its first step never simulates.
///
/// Every entity whose `StreamFrozen` came or went since the last step ends in the state its
/// component has now, whatever the order of the changes: an add and a remove before one step
/// leave the body simulating, a remove and an add leave it frozen.
#[instrument(skip_all, target = "regia_physics")]
pub(crate) fn apply_stream_frozen(
    mut changes: ResMut<FrozenChanges>,
    mut physics: ResMut<PhysicsWorld>,
    frozen_now: Query<(), With<StreamFrozen>>,
    rebuilt: Query<Entity, (With<StreamFrozen>, Added<crate::components::RigidBodyHandle>)>,
) {
    // A body rebuilt while its entity stays frozen (its layer parked and unparked, `RigidBody`
    // removed and re-added) is created enabled, and no freeze change is recorded for it: freeze
    // it here (corr-physics audit).
    for entity in &rebuilt {
        let handle = physics.body_handles.get(&entity).copied();
        if let Some(body) = handle.and_then(|h| physics.bodies.get_mut(h)) {
            body.set_enabled(false);
        }
    }
    if changes.frozen.is_empty() && changes.woken.is_empty() {
        return;
    }
    let FrozenChanges { frozen, woken } = &mut *changes;
    for entity in woken.drain(..).chain(frozen.drain(..)) {
        let handle = physics.body_handles.get(&entity).copied();
        if let Some(body) = handle.and_then(|h| physics.bodies.get_mut(h)) {
            body.set_enabled(!frozen_now.contains(entity));
        }
    }
}

/// Marks every cell the runtime waits on: colliders of all spawned entities exist after the sync.
#[instrument(skip_all, target = "regia_physics")]
pub(crate) fn mark_physics_gate(gate: Option<Res<PhysicsStreamGate>>, gates: Option<ResMut<StreamGates>>) {
    let (Some(gate), Some(mut gates)) = (gate, gates) else {
        return;
    };
    let waiting: Vec<_> = gates.awaiting(gate.0).collect();
    for (level, cell) in waiting {
        gates.set_ready(gate.0, level, cell);
    }
}

/// A new collider that is not a tiled terrain (`terrain_tiles` answers for those).
type NewWholeHeightfield = (
    Added<Collider>,
    Without<regia_scene::streaming::TerrainTileCollision>,
);

/// The `terrain_collision` gate (spec 78 § 13.1). The terrain collider is one heightfield over the
/// whole map, so the gate answers for a whole level, open from the first step the collider is in
/// rapier. Only levels cooked with terrain claim it (`regia_world`), and it covers their cells with
/// no record too. A collider owned by no level answers for every level. A heightfield with
/// `TerrainTileCollision` answers per cell instead (`terrain_tiles`, ADR-2317).
#[instrument(skip_all, target = "regia_physics")]
pub(crate) fn mark_terrain_gate(
    colliders: Query<(&Collider, Option<&LevelMember>), NewWholeHeightfield>,
    gates: Option<ResMut<StreamGates>>,
) {
    let Some(mut gates) = gates else {
        return;
    };
    for (collider, member) in &colliders {
        if !matches!(collider.shape, ColliderShape::HeightField { .. }) {
            continue;
        }
        let gate = gates.register_for_claimed_levels(TERRAIN_COLLISION_GATE, Readiness::SimReady);
        match member {
            Some(member) => gates.open_level(gate, member.level()),
            None => gates.open_everywhere(gate),
        }
    }
}

/// Shifts everything physics holds by a rebase. Rapier's bodies and colliders have no notion of an
/// origin, so each one moves; a joint's anchors are relative to its bodies and do not change. Stored
/// poses shift too, or `interpolate_transforms` would blend across the jump. Velocities stay.
#[instrument(skip_all, target = "regia_physics")]
pub(crate) fn shift_physics_origin(
    mut shifted: MessageReader<OriginShifted>,
    mut physics: ResMut<PhysicsWorld>,
    mut poses: Query<(&mut PhysicsPose, &mut PreviousPhysicsPose)>,
    cloth: Option<ResMut<ClothSet>>,
    destruction: Option<ResMut<crate::destruction::Destruction>>,
) {
    let mut cloth = cloth;
    let mut destruction = destruction;
    for message in shifted.read() {
        let shift = Vec3::from_array(message.rebase.shift);
        let physics = &mut *physics;
        for (_, body) in physics.bodies.iter_mut() {
            let mut pose = *body.position();
            pose.translation = to_rapier_vec3(from_rapier_vec3(pose.translation) - shift);
            body.set_position(pose, false);
        }
        for (_, collider) in physics.colliders.iter_mut() {
            if collider.parent().is_none() {
                let mut pose = *collider.position();
                pose.translation = to_rapier_vec3(from_rapier_vec3(pose.translation) - shift);
                collider.set_position(pose);
            }
        }
        // Queries between here and the next step read the broad phase: bring it along now.
        physics
            .bodies
            .propagate_modified_body_positions_to_colliders(&mut physics.colliders);
        let params = IntegrationParameters::default();
        for (handle, collider) in physics.colliders.iter() {
            let aabb = collider.compute_broad_phase_aabb(&params, &physics.bodies);
            physics.broad_phase.set_aabb(&params, handle, aabb);
        }
        for (mut pose, mut previous) in &mut poses {
            pose.translation -= shift;
            previous.translation -= shift;
        }
        if let Some(cloth) = cloth.as_deref_mut() {
            cloth.shift_all(shift);
        }
        if let Some(destruction) = destruction.as_deref_mut() {
            destruction.world.shift_origin(shift.to_array());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloth::{ClothCollider, ClothConfig, ClothShape, ClothSolver};

    #[test]
    fn a_cloth_shift_moves_particles_shapes_and_keeps_contact_geometry() {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("solver");
        solver
            .add_particle(Vec3::new(10.0, 1.0, 0.0), 1.0)
            .expect("particle");
        solver
            .add_collider(ClothCollider {
                shape: ClothShape::Sphere {
                    center: Vec3::new(10.0, 0.0, 0.0),
                    radius: 0.5,
                },
                friction: 0.0,
            })
            .expect("sphere");
        solver
            .add_collider(ClothCollider {
                shape: ClothShape::Plane {
                    normal: Vec3::X,
                    offset: 2.0,
                },
                friction: 0.0,
            })
            .expect("plane");
        solver.shift_origin(Vec3::new(1024.0, 0.0, 0.0));
        assert_eq!(solver.particles()[0].position, Vec3::new(-1014.0, 1.0, 0.0));
        let ClothShape::Sphere { center, .. } = solver.colliders()[0].shape else {
            panic!("sphere");
        };
        assert_eq!(center, Vec3::new(-1014.0, 0.0, 0.0));
        let ClothShape::Plane { offset, .. } = solver.colliders()[1].shape else {
            panic!("plane");
        };
        // The particle stayed 8 m in front of the plane.
        assert_eq!(solver.particles()[0].position.x - offset, 8.0);
    }

    fn physics_app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        let mut fixed = regia_app::FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        app
    }

    fn enabled(app: &regia_app::App, entity: Entity) -> bool {
        let physics = app.world().resource::<PhysicsWorld>();
        let handle = physics.body_handles[&entity];
        physics.bodies[handle].is_enabled()
    }

    fn spawn_falling(app: &mut regia_app::App) -> Entity {
        app.world_mut()
            .spawn((
                regia_scene::Transform::from_translation(Vec3::new(0.0, 10.0, 0.0)),
                RigidBody::dynamic(),
                Collider::default(),
            ))
            .id()
    }

    /// Audit g-physics2 finding 3: `StreamFrozen` added and removed before one fixed step left
    /// the body disabled for good (the frozen list was applied after the woken one, and nothing
    /// re-enabled it once the component was gone).
    #[test]
    fn a_freeze_undone_before_the_step_leaves_the_body_simulating() {
        let mut app = physics_app();
        let body = spawn_falling(&mut app);
        for _ in 0..3 {
            app.update();
        }
        assert!(enabled(&app, body));
        app.world_mut().entity_mut(body).insert(StreamFrozen);
        app.world_mut().entity_mut(body).remove::<StreamFrozen>();
        let height = |app: &regia_app::App| {
            let physics = app.world().resource::<PhysicsWorld>();
            physics.bodies[physics.body_handles[&body]].translation().y
        };
        let before = height(&app);
        for _ in 0..5 {
            app.update();
        }
        assert!(app.world().get::<StreamFrozen>(body).is_none());
        assert!(enabled(&app, body), "a body with no StreamFrozen is enabled");
        let after = height(&app);
        assert!(after < before, "the body keeps falling: {before} -> {after}");
    }

    /// Corr-physics audit: a body rebuilt while its entity keeps `StreamFrozen` (its layer parked
    /// and unparked, or `RigidBody` removed and re-added) came back enabled and simulated, since no
    /// freeze change was recorded for the new body.
    #[test]
    fn a_body_rebuilt_while_frozen_stays_frozen() {
        for park in [false, true] {
            let mut app = physics_app();
            let body = spawn_falling(&mut app);
            app.world_mut().entity_mut(body).insert(StreamFrozen);
            for _ in 0..3 {
                app.update();
            }
            assert!(!enabled(&app, body));
            if park {
                app.world_mut()
                    .entity_mut(body)
                    .insert(regia_scene::layers::LayerParked {
                        restore: regia_scene::components::Visibility::default(),
                        inherited: false,
                    });
            } else {
                app.world_mut().entity_mut(body).remove::<RigidBody>();
            }
            for _ in 0..3 {
                app.update();
            }
            assert!(
                !app.world()
                    .resource::<PhysicsWorld>()
                    .body_handles
                    .contains_key(&body),
                "park {park}: the body was removed"
            );
            if park {
                app.world_mut()
                    .entity_mut(body)
                    .remove::<regia_scene::layers::LayerParked>();
            } else {
                app.world_mut().entity_mut(body).insert(RigidBody::dynamic());
            }
            for _ in 0..3 {
                app.update();
            }
            assert!(
                !enabled(&app, body),
                "park {park}: the rebuilt body simulates while frozen"
            );
            app.world_mut().entity_mut(body).remove::<StreamFrozen>();
            app.update();
            assert!(enabled(&app, body), "park {park}: waking still works");
        }
    }

    /// The same on the spawn frame: both events before the body's first step.
    #[test]
    fn a_freeze_undone_on_the_spawn_frame_leaves_the_body_simulating() {
        let mut app = physics_app();
        let body = spawn_falling(&mut app);
        app.world_mut().entity_mut(body).insert(StreamFrozen);
        app.world_mut().entity_mut(body).remove::<StreamFrozen>();
        for _ in 0..3 {
            app.update();
        }
        assert!(enabled(&app, body));
    }

    /// The other order still ends frozen: a remove and a later add before one step.
    #[test]
    fn a_wake_then_freeze_before_the_step_ends_frozen() {
        let mut app = physics_app();
        let body = spawn_falling(&mut app);
        app.world_mut().entity_mut(body).insert(StreamFrozen);
        for _ in 0..3 {
            app.update();
        }
        assert!(!enabled(&app, body));
        app.world_mut().entity_mut(body).remove::<StreamFrozen>();
        app.world_mut().entity_mut(body).insert(StreamFrozen);
        for _ in 0..3 {
            app.update();
        }
        assert!(
            !enabled(&app, body),
            "StreamFrozen is on: the body stays disabled"
        );
        app.world_mut().entity_mut(body).remove::<StreamFrozen>();
        app.update();
        assert!(enabled(&app, body));
    }
}
