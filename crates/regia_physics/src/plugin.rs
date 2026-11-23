// SPDX-License-Identifier: MPL-2.0
//! `PhysicsPlugin` — registers resources, components, messages, systems and the rapier
//! world (specs/15 § 1).
//!
//! | Schedule | System | Purpose |
//! |---|---|---|
//! | `FixedUpdate` | `sync_parked` → `sync_removed` → … → `flush_physics_events` | The sync chain of `specs/15` § 1 (see [`crate::step`]) |
//! | `PostUpdate` | `init_debris_spawn_times` → `despawn_expired_debris` → `enforce_body_budget` | M14 debris lifecycle |
//! | `PostUpdate/PhysicsSync` | `interpolate_transforms` | Fixed-step pose interpolation |
//! | `PreUpdate` | `attach_authored_colliders` | Colliders from cooked `collision_*` proxies ([`crate::authored_collision`]) |
//!
//! Collision layers come from `config/physics.ron` when the project inserts a
//! [`PhysicsLayers`](crate::layers::PhysicsLayers) resource loaded with
//! [`LayerFile::load`](crate::layers::LayerFile::load); the default is a single
//! self-colliding "default" layer.

use regia_app::prelude::*;
use regia_ecs::spawn_order::{TrackSpawnOrderError, try_track_spawn_order};

use crate::components::{
    ActiveEvents, BodyKind, CharacterBody, CharacterGround, Collider, ColliderShape, CollisionLayer,
    CombineRule, DebrisTag, ExternalForce, ExternalImpulse, Joint, JointKind, JointMotorConfig,
    JointNoContacts, LockedAxes, Mass, NoInterpolation, RigidBody, ShapeAxis, Teleport, Velocity,
};
use crate::events::{CollisionEnded, CollisionStarted, ContactForce, JointBroken, TriggerEnter, TriggerExit};
use crate::joints::{check_joint_breaks, sync_joints};
use crate::step::{
    flush_physics_events, record_body_handle_removal, record_body_removal, record_collider_handle_removal,
    record_collider_removal, record_force_removal, record_joint_no_contacts_removal, record_joint_removal,
    step_physics, sync_added_bodies, sync_added_colliders, sync_body_properties, sync_collider_changes,
    sync_kinematic_poses, sync_parked, sync_removed, sync_velocity_and_forces,
};
use crate::sync::{
    despawn_expired_debris, enforce_body_budget, init_debris_spawn_times, interpolate_transforms,
};
use crate::world::{BodyBudget, Gravity, PhysicsSettings, PhysicsWorld};
use regia_scene::streaming::StreamSet;

/// The fixed-step physics chain (specs/15 § 1): sync → step → event flush, in `FixedUpdate`.
///
/// The chain runs after [`regia_app::FixedUpdateSet::Animation`]: property tracks have already
/// advanced and applied their kinematic `Transform`s for this tick when `sync_kinematic_poses`
/// reads them (ADR-4653).
///
/// Gameplay that moves against the stepped world — kinematic platforms driving their
/// `Transform`, the character controller pushing bodies, scripts reading velocities —
/// should order itself `.after(PhysicsStep)` so it sees the freshly stepped state and its
/// impulses survive until the next step. The same goes for systems that read
/// [`PhysicsQuery`](crate::queries::PhysicsQuery) or the collision messages
/// (`flush_physics_events` is the last system in the set): without the edge they can run
/// between the body and collider sync and see a half-synced world, or one substep late.
/// `Physics2dStep` and `PlatformerStep` are ordered after this set so one entity carrying
/// several body kinds always gets the same `Transform` writer last. The set exists even when the chain is empty
/// (plugin absent): ordering against it is then a no-op.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicsStep;

/// The physics plugin (specs/15). The step runs in `FixedUpdate` at `FixedTime::hz`; add
/// this plugin after the scene plugin so `LevelSettings` gravity is available.
pub struct PhysicsPlugin;

/// Spawn-order tracking is a hard requirement for every component whose arrival order sorts a
/// simulation feed (ADR-1347): the untracked fallback sorts by entity ids that parallel
/// `Commands::spawn` calls reserve in thread-timing order, which diverges between runs and
/// machines. Fail at plugin-build time instead of stepping a nondeterministic world.
pub(crate) fn must_track_spawn_order<C: Component>(world: &mut World) {
    if let Err(error) = try_track_spawn_order::<C>(world) {
        panic!(
            "spawn-order tracking of `{}` failed ({error:?}): insertion order would follow \
             thread-timing entity ids and break deterministic replays. Add the physics plugin \
             before anything that inserts `{}` or hooks its on_add/on_remove.",
            std::any::type_name::<C>(),
            std::any::type_name::<C>()
        );
    }
}

/// The component whose spawn-order tracking [`track_physics_spawn_order`] could not start, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicsSpawnOrderError {
    /// The component's type name.
    pub component: &'static str,
    /// Why its hooks could not be installed.
    pub error: TrackSpawnOrderError,
}

impl std::fmt::Display for PhysicsSpawnOrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "spawn-order tracking of `{}` failed ({:?})",
            self.component, self.error
        )
    }
}

impl std::error::Error for PhysicsSpawnOrderError {}

/// Starts the spawn-order tracking [`PhysicsPlugin`] requires (ADR-1347) without the rest of
/// the plugin: rigid bodies, colliders, joints, authored collision and force fields have their
/// arrivals numbered from now on. Idempotent; the plugin calls it itself.
///
/// The hooks can only be installed before the first entity holds the component. A host that
/// puts level content into a world *before* a game's `register` may add the plugin (play in
/// editor spawns the edit level first) calls this before that spawn: the level's bodies are
/// then numbered in spawn order, and the game's `PhysicsPlugin` finds tracking already on
/// instead of failing (ADR-9180).
///
/// # Errors
///
/// The first component whose tracking cannot start: an entity already holds it, or its
/// `on_add`/`on_remove` hooks belong to someone else.
pub fn track_physics_spawn_order(world: &mut World) -> Result<(), PhysicsSpawnOrderError> {
    fn one<C: Component>(world: &mut World) -> Result<(), PhysicsSpawnOrderError> {
        try_track_spawn_order::<C>(world).map_err(|error| PhysicsSpawnOrderError {
            component: std::any::type_name::<C>(),
            error,
        })
    }
    one::<RigidBody>(world)?;
    one::<BodyKind>(world)?;
    one::<Collider>(world)?;
    one::<ColliderShape>(world)?;
    one::<Joint>(world)?;
    one::<crate::authored_collision::AuthoredCollision>(world)?;
    // The force-field fold order (sums and the gravity tie-break) follows spawn order.
    one::<crate::fields::ForceField>(world)
}

impl Plugin for PhysicsPlugin {
    fn build(&self, app: &mut App) {
        // `init_resource`: settings a project inserted before adding the plugin are kept.
        app.init_resource::<PhysicsSettings>()
            .init_resource::<Gravity>()
            .init_resource::<PhysicsWorld>()
            .init_resource::<crate::parent_warning::ParentedBodyWarnings>()
            .init_resource::<BodyBudget>()
            .init_resource::<crate::layers::PhysicsLayers>()
            .init_resource::<crate::material::PhysicsMaterials>()
            .init_resource::<crate::debug_draw::PhysicsDebugDraw>()
            .register_type::<crate::material::PhysicsMaterialRef>()
            // Saved in levels even where the game adds no DestructionPlugin (ADR-9750).
            .register_type::<crate::destruction::DestructibleSource>()
            .init_resource::<crate::stream_hooks::FrozenChanges>()
            .init_resource::<crate::terrain_tiles::TerrainTiles>()
            .init_resource::<crate::terrain_tiles::TerrainTileStats>()
            .add_message::<regia_scene::streaming::OriginShifted>()
            .add_message::<CollisionStarted>()
            .add_message::<CollisionEnded>()
            .add_message::<TriggerEnter>()
            .add_message::<TriggerExit>()
            .add_message::<ContactForce>()
            .add_message::<JointBroken>()
            .register_type::<RigidBody>()
            .register_type::<BodyKind>()
            .register_type::<LockedAxes>()
            .register_type::<Mass>()
            .register_type::<Velocity>()
            .register_type::<ExternalForce>()
            .register_type::<ExternalImpulse>()
            .register_type::<Teleport>()
            .register_type::<NoInterpolation>()
            .register_type::<Collider>()
            .register_type::<ColliderShape>()
            .register_type::<ShapeAxis>()
            .register_type::<CombineRule>()
            .register_type::<CollisionLayer>()
            .register_type::<ActiveEvents>()
            .register_type::<Joint>()
            .register_type::<JointNoContacts>()
            .register_type::<crate::components::JointBreakTorque>()
            .register_type::<JointKind>()
            .register_type::<JointMotorConfig>()
            .register_type::<CharacterBody>()
            .register_type::<CharacterGround>()
            .register_type::<DebrisTag>()
            .register_type::<crate::authored_collision::AuthoredCollision>();

        // Number the arrivals of the components rapier inserts, so insertion order does not
        // follow entity ids that parallel `Commands::spawn` calls reserve in thread-timing
        // order (ADR-1347). Tracking is a hard requirement: the id fallback is not
        // deterministic, so a component that cannot be tracked fails the build instead. A host
        // that started it earlier (`track_physics_spawn_order`, ADR-9180) is found on here.
        if let Err(failed) = track_physics_spawn_order(app.world_mut()) {
            panic!(
                "{failed}: insertion order would follow thread-timing entity ids and break \
                 deterministic replays. Add the physics plugin (or call \
                 `regia_physics::track_physics_spawn_order`) before anything that inserts `{}` \
                 or hooks its on_add/on_remove.",
                failed.component
            );
        }

        // Removals are recorded the moment they happen and applied by `sync_removed` before
        // the next step: removal messages are cleared every frame and would be lost when
        // several frames pass without a fixed step.
        crate::stream_hooks::register_gate(app.world_mut());
        app.add_observer(crate::stream_hooks::record_frozen)
            .add_observer(crate::stream_hooks::record_woken);
        app.add_observer(record_body_handle_removal)
            .add_observer(record_collider_handle_removal)
            .add_observer(record_body_removal)
            .add_observer(record_collider_removal)
            .add_observer(record_joint_removal)
            .add_observer(record_joint_no_contacts_removal)
            .add_observer(record_force_removal);

        // The fixed-step sync chain (spec 15 § 1): removals first so re-created components
        // on the same frame behave, then additions, joints, inputs, the step, breaks and
        // the event flush.
        app.add_systems(
            FixedUpdate,
            (
                sync_parked,
                sync_removed,
                sync_added_bodies,
                sync_added_colliders,
                crate::stream_hooks::apply_stream_frozen,
                crate::stream_hooks::mark_physics_gate,
                crate::stream_hooks::mark_terrain_gate,
                crate::terrain_tiles::step_terrain_tiles,
                sync_collider_changes,
                sync_joints,
                sync_body_properties,
                sync_kinematic_poses,
                sync_velocity_and_forces,
                step_physics,
                check_joint_breaks,
                flush_physics_events,
            )
                .chain()
                .in_set(PhysicsStep),
        );

        // World streaming (spec 78 § 4.3): the guard runs before the step, the sources after it, and
        // a rebase reaches rapier before anything plans on the new positions. Animation's fixed
        // set (property tracks → kinematic Transforms) runs before the step consumes them
        // (ADR-4653).
        app.configure_sets(
            FixedUpdate,
            PhysicsStep
                .after(regia_app::FixedUpdateSet::Animation)
                .after(StreamSet::Guard)
                .before(StreamSet::Sources),
        );
        app.add_systems(
            FixedUpdate,
            (
                crate::stream_hooks::mark_stream_guarded.before(StreamSet::Guard),
                crate::stream_hooks::apply_stream_teleports.before(StreamSet::Guard),
                crate::stream_hooks::shift_physics_origin
                    .after(StreamSet::Rebase)
                    .before(StreamSet::Plan),
            ),
        );

        // M14 debris lifecycle: init → despawn → budget (spawn time before despawn checks).
        app.add_systems(
            PostUpdate,
            (
                init_debris_spawn_times,
                despawn_expired_debris,
                enforce_body_budget,
            )
                .chain(),
        );

        // Authored collision proxies (ADR-2341): opt-in per entity, outside the fixed chain so the
        // fixed-step schedule (and its determinism traces) is unchanged.
        app.add_systems(PreUpdate, crate::authored_collision::attach_authored_colliders);
        // spec 15 § 1: a body under a non-physics parent fights the world-pose writeback;
        // count it for the editor warning and the agent counter. Play worlds only: the
        // edit world has no PhysicsPlugin.
        app.add_systems(PostUpdate, crate::parent_warning::warn_parented_bodies);
        // Named physics materials (ADR-2424): only colliders carrying a `PhysicsMaterialRef` are touched.
        app.add_systems(PreUpdate, crate::material::apply_physics_materials);

        // Debug draw lines (ADR-2424): outside the fixed chain, read-only on the rapier world, and the
        // system does not run while the draw is off.
        app.add_systems(
            PostUpdate,
            crate::debug_draw::collect_physics_debug.run_if(crate::debug_draw::physics_debug_on),
        );

        // Force fields (ADR-3348): after the velocity sync, before the step; the condition keeps the
        // system out of scenes without a `ForceField`, so their schedule work is unchanged.
        app.init_resource::<crate::fields::FieldScratch>()
            .add_observer(crate::fields::clear_pad_occupancy)
            .register_type::<crate::fields::ForceField>()
            .register_type::<crate::fields::FieldKind>()
            .register_type::<crate::fields::FieldVolume>()
            .register_type::<crate::fields::FieldMode>()
            .register_type::<crate::fields::Falloff>()
            .add_systems(
                FixedUpdate,
                crate::fields::apply_force_fields
                    .run_if(crate::fields::any_force_field)
                    .after(sync_velocity_and_forces)
                    .before(step_physics)
                    .in_set(PhysicsStep),
            );

        // Interpolation of dynamic bodies (spec 15 § 1) in the PhysicsSync set.
        app.add_systems(
            PostUpdate,
            interpolate_transforms.in_set(PostUpdateSet::PhysicsSync),
        );
    }

    fn finish(&self, _app: &mut App) {
        tracing::info!(target: "regia_physics", "PhysicsPlugin finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_registers_resources() {
        let mut app = App::new();
        app.add_plugins(PhysicsPlugin);
        app.update();

        let world = app.world();
        assert!(world.contains_resource::<PhysicsSettings>());
        assert!(world.contains_resource::<Gravity>());
        assert!(world.contains_resource::<PhysicsWorld>());
        assert!(world.contains_resource::<BodyBudget>());
        assert!(world.contains_resource::<crate::layers::PhysicsLayers>());
    }

    #[test]
    fn plugin_registers_messages() {
        use regia_ecs::ecs::message::Messages;
        let mut app = App::new();
        app.add_plugins(PhysicsPlugin);
        assert!(app.world().contains_resource::<Messages<CollisionStarted>>());
        assert!(app.world().contains_resource::<Messages<TriggerEnter>>());
        assert!(app.world().contains_resource::<Messages<JointBroken>>());
    }

    /// Finding 1 of the ecs determinism audit: a `RigidBody` in the world before the plugin
    /// builds used to silently turn spawn-order tracking off, leaving rapier insertion order to
    /// thread-timing entity ids. That now fails loudly at build time.
    #[test]
    #[should_panic(expected = "spawn-order tracking of")]
    fn plugin_build_fails_when_a_tracked_component_is_already_in_use() {
        let mut app = App::new();
        app.world_mut().spawn(RigidBody::default());
        app.add_plugins(PhysicsPlugin);
    }

    /// The other way tracking used to fail open: another owner holding a component's `on_add`
    /// hook slot.
    #[test]
    #[should_panic(expected = "spawn-order tracking of")]
    fn plugin_build_fails_when_a_tracked_components_hooks_are_taken() {
        let mut app = App::new();
        app.world_mut()
            .register_component_hooks::<RigidBody>()
            .on_add(|_world, _context| {});
        app.add_plugins(PhysicsPlugin);
    }

    /// ADR-9180: a host that spawns level content before the game's `register` (play in editor)
    /// starts tracking first; the plugin then builds over the existing bodies, which keep the
    /// numbers they arrived with.
    #[test]
    fn tracking_started_before_the_level_lets_the_plugin_build_over_it() {
        use regia_ecs::spawn_order::SpawnOrder;
        let mut app = App::new();
        track_physics_spawn_order(app.world_mut()).expect("a fresh world tracks");
        let floor = app
            .world_mut()
            .spawn((RigidBody::default(), Collider::default()))
            .id();
        let crate_body = app
            .world_mut()
            .spawn((RigidBody::default(), Collider::default()))
            .id();
        app.add_plugins(PhysicsPlugin);
        // Idempotent: a second call with tracking on is not an error.
        assert_eq!(track_physics_spawn_order(app.world_mut()), Ok(()));
        let order = app.world().resource::<SpawnOrder<Collider>>();
        assert!(order.is_tracking());
        assert_eq!(order.get(floor), Some(0));
        assert_eq!(order.get(crate_body), Some(1));
        app.update();
        assert!(app.world().contains_resource::<PhysicsWorld>());
    }

    /// The helper reports which component failed instead of panicking; the plugin panics.
    #[test]
    fn tracking_reports_the_component_already_in_use() {
        let mut app = App::new();
        app.world_mut().spawn(Collider::default());
        let failed = track_physics_spawn_order(app.world_mut()).expect_err("in use");
        assert_eq!(failed.component, std::any::type_name::<Collider>());
        assert_eq!(failed.error, TrackSpawnOrderError::ComponentInUse);
    }
}
