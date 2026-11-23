// SPDX-License-Identifier: MPL-2.0
//! Physics (rapier3d): bodies, colliders, joints, layers, events, queries and the
//! kinematic character controller.
//!
//! Specification: `docs/specs/15-physics.md` (M20: the full crate; M14 built only the
//! cosmetic-debris subset, whose API — [`DebrisSpawner`], [`BodyBudget`], [`DebrisTag`] —
//! keeps working).
//!
//! - [`PhysicsPlugin`] steps a rapier world in `FixedUpdate` (once per run, at
//!   `FixedTime::hz`) with the sync chain of § 1: ECS → rapier additions/removals and
//!   property changes, the step, rapier → ECS pose/velocity write-back, and
//!   interpolation of rendered transforms in `PostUpdate/PhysicsSync`.
//! - § 2 components: [`RigidBody`] (Dynamic/Kinematic/Static, damping, gravity scale, CCD,
//!   locked axes, dominance), [`Mass`], [`Collider`] with every [`ColliderShape`] plus
//!   offsets, sensors, materials and layers, [`Velocity`], [`ExternalForce`],
//!   [`ExternalImpulse`], [`Joint`] (with `break_force`) and [`CharacterBody`].
//! - § 3: collision layers from `config/physics.ron` ([`PhysicsLayers`]), collision and
//!   trigger events as ECS messages ([`events`]), and spatial queries through the
//!   [`PhysicsQuery`] system parameter.
//! - § 4: the character-controller backend ([`move_character`]) that the framework's
//!   `CollisionQuery` adapter wraps.
//!
//! # How the pieces fit
//!
//! A game writes [`RigidBody`]/[`Collider`]/[`Velocity`]/[`CharacterBody`] components on entities
//! (a [`regia_scene::Transform`] gives them a pose). Each `FixedUpdate`,
//! [`PhysicsPlugin`]'s sync systems ([`sync_added_bodies`], [`sync_added_colliders`],
//! [`sync_body_properties`], [`sync_velocity_and_forces`], [`sync_kinematic_poses`]) push those
//! component changes into the [`PhysicsWorld`], [`step_physics`] advances it once,
//! [`flush_physics_events`] turns contacts into [`CollisionStarted`]/[`TriggerEnter`] messages, and
//! the pose/velocity write-back plus [`interpolate_transforms`] render the result. Spatial
//! [`queries`] ([`PhysicsQuery`] inside a system, [`WorldQueries`] for tooling that holds a
//! `&World`) and the character controller ([`move_character`]) read the stepped world through
//! [`PhysicsWorld::query_pipeline`].
//!
//! # Determinism and performance (spec 15 § 5)
//!
//! Same build + same platform + same inputs → identical results: rapier is deterministic
//! per platform, and the step runs exactly once per `FixedUpdate` (a fixed dt from
//! `FixedTime`), so the simulation never depends on the render frame rate. For
//! cross-platform lockstep determinism enable the `physics-cross-platform-determinism`
//! cargo feature (rapier's `enhanced-determinism`; costs speed, off by default). Rapier's
//! `parallel` solver feature stays off — its rayon pool would contend with the engine's
//! job pools; revisit per scene size (ADR-worthy when large scenes need it).
//!
//! The step itself does not depend on the thread count (rapier runs single-threaded and the
//! sync chain is ordered), and every rapier object is created in entity order. The tests in
//! `tests/determinism.rs` hold this: two runs, both executors, and 1/2/4/8 compute threads
//! give bit-identical results, compared through [`PhysicsWorld::state_digest`] (ADR-870).
//!
//! Budgets (desktop reference): 2,000 active dynamic boxes ≤ 4 ms per step; mobile:
//! 300 ≤ 4 ms. The ignored test `perf_step_time_2000_boxes` measures the desktop budget, and
//! the ignored `perf_step_time_20000_bodies` (`tests/perf_20k.rs`) prints the cost of a
//! 20,000-body pile while active, asleep and while despawning. [`PhysicsWorld::stats`]
//! (agent `physics.stats`) reports awake/sleeping counts and the last step's cost.
//!
//! # Robustness (ADR-870)
//!
//! - Removals are recorded by `Remove` observers when they happen and applied before the
//!   next step, so a despawn is never lost when frames pass without a fixed step.
//! - A joint comes back when a body it lost returns (or when its connected entity gets a
//!   body later); a joint broken by `break_force` stays broken until its `Joint` changes.
//! - Removing a `RigidBody` strips its colliders' handles, so they are rebuilt with the body.
//! - A pair whose collider was removed still reports `CollisionEnded` / `TriggerExit`.
//! - `ExternalForce` is pushed only when it changes and is cleared when removed, so a
//!   steady force lets its body sleep.
//! - [`PhysicsSettings::solver_iterations`] and [`PhysicsSettings::ccd_substeps`] tune the
//!   solver for tall stacks, long chains and fast CCD bodies.
//!
//! # 2D (`specs/22` § T2.10)
//!
//! - [`platformer2d`]: kinematic pixel platformer (actors, solids that push and carry, one-way
//!   tiles), always built; [`platformer2d::PlatformerPlugin`].
//! - `physics2d` (cargo feature `physics2d`): rapier2d bodies, colliders, queries and contact
//!   messages mirroring the 3D wrapper; `physics2d::Physics2dPlugin`.
//!
//! See `book/user-guide/physics.md` (bodies, colliders, queries, characters) and
//! `book/user-guide/2d.md`.

#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub mod ccd;
pub mod character;
pub mod components;
pub mod convert;
mod debris;
pub mod debug_draw;
pub mod events;
pub mod joints;
pub mod layers;
pub mod material;
pub mod parent_warning;
#[cfg(feature = "physics2d")]
pub mod physics2d;
pub mod platformer2d;
mod plugin;
pub mod step;
mod stream_hooks;
mod terrain_tiles;
pub use terrain_tiles::TerrainTileStats;
pub use terrain_tiles::{TerrainColliderPiece, TerrainLayerColliders};
mod sync;
pub mod world;

pub use ccd::{
    ConvexDistance, ConvexInput, ToiFailure, ToiResult, convex_distance, sphere_sweep,
    swept_box_vs_thin_wall, time_of_impact, time_of_impact_angular,
};
pub use character::{CharacterMove, capsule_has_headroom, move_character};
pub use components::{
    ActiveEvents, BodyKind, BodyMarker, CharacterBody, CharacterGround, Collider, ColliderHandle,
    ColliderShape, CollisionLayer, CombineRule, DebrisTag, ExternalForce, ExternalImpulse, Joint,
    JointBreakTorque, JointHandle, JointKind, JointMotorConfig, JointNoContacts, LockedAxes, Mass,
    NoInterpolation, PhysicsPose, PreviousPhysicsPose, RigidBody, RigidBodyHandle, ShapeAxis, Teleport,
    Velocity,
};
pub use debris::{ChunkDesc, DebrisSpawner, SpawnConfig, SpawnResult};
pub use events::{CollisionEnded, CollisionStarted, ContactForce, JointBroken, TriggerEnter, TriggerExit};
pub use joints::{check_joint_breaks, sync_joints};
pub use layers::{LAYER_CONFIG_PATH, LayerConfigError, LayerFile, MAX_LAYERS, PhysicsLayers};
pub use material::{PhysicsMaterial, PhysicsMaterialError, PhysicsMaterialRef, PhysicsMaterials};
pub use parent_warning::{ParentedBodyWarning, ParentedBodyWarnings, warn_parented_bodies};
pub use plugin::{PhysicsPlugin, PhysicsSpawnOrderError, PhysicsStep, track_physics_spawn_order};
pub use queries::{
    ClosestPointResult, PhysicsQuery, QueryFilter, RaycastHit, ShapeCastHitInfo, WorldQueries,
};
pub use step::{
    flush_physics_events, shape_axis_rotation, shape_to_shared_shape, step_physics, sync_added_bodies,
    sync_added_colliders, sync_body_properties, sync_kinematic_poses, sync_parked, sync_removed,
    sync_velocity_and_forces,
};
pub use sync::{
    despawn_expired_debris, enforce_body_budget, init_debris_spawn_times, interpolate_transforms,
};
pub use world::{BodyBudget, Gravity, PhysicsSettings, PhysicsStats, PhysicsWorld};

pub mod cloth;
pub mod convex_decomposition;
pub mod hover;
pub mod hull_set;
pub mod queries;
pub mod vehicle;
pub mod vehicle_asset;
pub mod vehicle_drivetrain;
pub mod vehicle_snapshot;
pub mod vehicle_tire;

// Destruction level 2: pre-fractured chunks, bonds, support graph (ADR-1597).
pub mod destruction;
// Colliders from cooked `collision_*` glTF proxies (ADR-2341).
pub mod authored_collision;
pub use authored_collision::{AuthoredCollision, AuthoredCollisionStatus};
// Opt-in swept CCD in the step, rotation included, and its motion query (ADR-2423).
pub mod ccd_step;
pub use ccd::{RigidMotion, ToiOptions, time_of_impact_motion};
pub use ccd_step::SweptCcd;
// Joint-space stable-PD helpers for active ragdolls (ADR-2498).
pub mod pd;
// Trailers: articulated vehicles on a tow ball or a fifth wheel, `.trailer.ron` (ADR-3281).
pub mod vehicle_trailer;
pub mod vehicle_trailer_asset;
// Fixed-wing aircraft: aerodynamic surfaces, thrust, landing gear, trim and `.aircraft.ron` (ADR-3374).
pub mod aero;

// Two-wheel lean balance, separate from the car solver (ADR-4017).
pub mod vehicle_bike;
// Read-only contact geometry for contact sounds (ADR-3270).
pub mod contact_query;
// Gameplay force fields: directional, radial, vortex, gravity, drag, impulse pads (ADR-3348).
pub mod fields;
pub use fields::{
    BodySample, Falloff, FieldEffect, FieldKind, FieldMode, FieldPose, FieldProbe, FieldScratch, FieldVolume,
    ForceField, ancestor_world_affine, apply_force_fields, localize_under_parent, probe_fields,
    world_field_pose,
};
// The explicit teleport: velocity kept or cleared, no swept contacts (ADR-7233).
pub mod teleport;
pub use teleport::{TeleportCommands, TeleportVelocity, teleport};

#[cfg(test)]
mod alloc_count;
// Skid-steer tracks: two track speeds, yaw from their difference (ADR-4629).
pub mod vehicle_track;
// The vehicle tuning proving ground: scripted accel and skid-pad runs (ADR-9760).
pub mod vehicle_proving;
