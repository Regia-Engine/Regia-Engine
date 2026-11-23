# Physics

`regia_physics` is a thin Rust layer over the rapier3d solver, driven by ECS components and synced
both ways. Add `regia::physics::PhysicsPlugin` yourself (`DefaultPlugins` does not install it; the dedicated server does) and give entities rigid bodies and colliders.

## Bodies and colliders

```rust
use regia::prelude::*;
use regia_scene::prelude::*;
use regia::physics::{Collider, ColliderShape, RigidBody};
use regia::core::math::Vec3;

fn spawn_crate(mut commands: Commands) {
    commands.spawn((
        Transform::from_translation(Vec3::new(0.0, 4.0, 0.0)),
        RigidBody::dynamic(),
        Collider {
            shape: ColliderShape::Box { half_extents: Vec3::new(0.5, 0.5, 0.5) },
            ..Collider::default()
        },
    ));
}
```

`RigidBody` is static (`RigidBody::static_body()`), dynamic (`RigidBody::dynamic()`) or kinematic
(`RigidBody::kinematic()`); a `Collider` (friction, restitution, density, layer, `active_events`) whose
`shape` is a `ColliderShape` gives it a `Box`, `Sphere`, `Capsule`, `Cylinder`,
`Cone`, `ConvexHull`, `TriMesh`, `HeightField` or `Compound` shape. The engine syncs transforms
ECS→physics for kinematically driven bodies and physics→ECS for dynamic ones.

## Queries

Query the live world without driving it — raycasts, overlaps, closest points:

```rust
use regia::prelude::*;
use regia::physics::{PhysicsQuery, QueryFilter};
use regia::core::math::Vec3;

fn has_clear_shot(queries: PhysicsQuery) -> bool {
    queries.raycast(Vec3::ZERO, Vec3::Y, 100.0, QueryFilter::new().exclude_sensors()).is_none()
}
```

`PhysicsQuery` is the `SystemParam` behind the protocol's
[`physics.raycast` / `overlap` / `closest_point`](../agents/physics.md) — the same API the
character controller builds on. `overlap(shape, pose, filter)` lists entities inside a shape;
`closest_point(point, max_distance, filter)` finds the nearest collider surface.

## Collision events

Collisions and sensor triggers surface as messages, for colliders whose
`active_events` has `collision: true` (the default is off; `ActiveEvents::ALL` turns both kinds on):

```rust
use regia::prelude::*;
use regia::physics::CollisionStarted;

fn on_collision(mut reader: MessageReader<CollisionStarted>) {
    for e in reader.read() {
        // e.entity_a, e.entity_b — the two entities that started touching.
    }
}
```

When an entity is despawned (or loses its `Collider`) while it touches something, the pair
still gets its `CollisionEnded`, or `TriggerExit` for a sensor, on the next fixed step. The entity
in that message may already be despawned, so look it up with `get`, not `[]`.

## Stacking, joints and sleeping

- **Solver settings.** `PhysicsSettings::solver_iterations` (default 4) trades solver time for
  stiffer stacks, chains and large mass ratios. `ccd_substeps` (default 1) lets a fast CCD body
  keep moving after its first hit in a step. Insert `PhysicsSettings` before or after
  `PhysicsPlugin`: the plugin keeps yours.

  ```rust,wrap
  # use regia::prelude::*;
  # use regia_physics::PhysicsSettings;
  # let app: &mut App = todo!();
  app.insert_resource(PhysicsSettings { solver_iterations: 8, ..Default::default() });
  ```
- **Fast bodies.** Set `RigidBody { ccd: true, .. }` on bullets and other bodies that move more
  than their own size in a step (at 60 Hz, 6 m/s for a 10 cm ball).
- **Joints come back with their bodies.** If a jointed body loses its `RigidBody` and gets it back,
  or the `connected` entity gets a body only later, the joint is created then. A joint broken by
  `break_force` stays broken until you edit its `Joint` component (`JointBroken` reports it).
- **Breaking by torque.** Put a `JointBreakTorque(n)` component (newton-metres) beside the `Joint` to break it when
  its torque goes over `n`, in addition to `break_force`. It counts a load hanging to the side of the joint.
- **Materials.** A `.physmat.ron` (`docs/formats/physmat.md`) names friction, restitution, density and the combine
  rules; register it in `PhysicsMaterials` and put `PhysicsMaterialRef("name")` beside a `Collider` to give the
  collider those numbers. The default material is the collider default, so unreferenced colliders are unchanged.
- **Debug draw.** The agent's `physics.debug_draw` shows contact points and normals, joints and centers of mass in the
  `physics` gizmo group (see [Physics: debug draw, materials](../agents/physics-spec15.md)). It costs nothing off.
- **Sleeping.** Bodies at rest go to sleep after half a second and cost no solver time.
  Impulses, `Velocity` writes, `ExternalForce` edits, teleports and a removed floor wake them.
  A steady `ExternalForce` does not keep a body awake. Removing the component stops the force.
  `RigidBody { can_sleep: false, .. }` keeps a body awake.
- **Counting.** `PhysicsWorld::stats()` (agent `physics.stats`) reports awake and sleeping
  bodies, contact pairs and the last step's cost. Its `non_finite_inputs` counts non-finite
  poses, body fields, velocities, forces and impulses that were dropped before reaching the
  simulation (ADR-1507): zero in a healthy app, and a growing count points at a script, scene
  or networked value writing NaN — one bad value cannot poison the contacts of everything a
  body touches, but the guard does not make the value meaningful either; fix the writer.

## Characters

A **character** is a capsule driven through the physics world with movement modes (walk, crouch,
jump, fly) — the foundation for the player and AI controllers. See [Gameplay](gameplay.md) and
`docs/specs/15-physics.md`.

Crouching shrinks the capsule from the top (the entity's transform stays at the standing centre)
and stands up only when there is headroom; a ceiling ends a jump at once; jumping off a moving
platform keeps its velocity; the reported velocity is the motion achieved. Ragdolls get joint
limits from the bind pose (`RagdollProfile::humanoid_limited`, `.ragdoll.ron`), report when they
have settled (face up or down) and stay within a per-tier budget (`RagdollSettings`). See
`docs/specs/93-character-physics.md` and [character physics (agents)](../agents/character-physics.md).

## Determinism

On one platform and build, the same inputs give the same simulation, bit for bit, whatever the
thread count. `PhysicsWorld::state_digest()` hashes every body's pose, velocities and sleep flag.
Compare it between two runs at the same step to check a replay (the agent's `physics.stats`
returns it too).

`regia_physics`'s `physics-cross-platform-determinism` feature enables rapier's
`enhanced-determinism`, which makes a measured subset of rapier bit-identical across the CPU
architectures measured so far. It costs speed, so it is off by default. It is not a promise for
lockstep games: Regia does not promise rigid bodies on a lockstep simulation, and a lockstep
simulation never reads a rapier pose, contact or event (ADR-8841). Lockstep games move their units
with the `regia_detsim` models; see [Movement models](movement-models.md).

Collision layers come from `config/physics.ron` when the project installs `PhysicsLayers`; a
collider on a layer nothing may query is invisible to queries that set `QueryFilter::from_layer`
(a query without `from_layer` is not filtered by the query matrix and still hits it).
