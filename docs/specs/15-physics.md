# Spec 15 — Physics (`regia_physics`, rapier3d)

**Milestones:** M14 builds only what cosmetic debris needs (§ 1; dynamic and static bodies; box,
convex-hull and heightfield colliders — no events, joints, queries or character controller). Everything
else, and the acceptance in § 6, is M20.

## 1. Integration model
- `PhysicsWorld` resource owns rapier's sets (`RigidBodySet`, `ColliderSet`, `ImpulseJointSet`,
  `MultibodyJointSet`), pipeline, broad/narrow phase, island manager, CCD solver, query pipeline.
  Rapier handles are stored in `RigidBodyHandle`/`ColliderHandle` components (internal, not saved).
- Steps run in `FixedUpdate` (default 60 Hz; project setting). Gravity from `LevelSettings`.
- Sync each fixed step:
  1. ECS → rapier: create/remove bodies and colliders for added/removed components; push changed
     `Transform` for `Kinematic` bodies (as next kinematic position) and for teleported bodies (the `Teleport`
     marker, or the explicit `regia_physics::teleport(world, entity, pose, TeleportVelocity::Keep | Clear)`,
     `EntityCommands::teleport`, `ScriptCtx::teleport` and agent `physics.teleport`, ADR-7233: the body's current and
     next poses both become the target, so the jump sweeps nothing, makes no contact, gives a kinematic body no
     velocity and is not interpolated; `Keep` keeps the velocity, `Clear` zeroes it); push changed properties (mass, damping, collider shape...).
  2. `step()`.
  3. rapier → ECS: write positions of `Dynamic` bodies into `Transform` (world → local if parented to a
     non-physics parent; recommended: physics bodies are hierarchy roots — a `Dynamic` body under a
     non-physics parent is warned about once per play session: a `tracing::warn!` and the
     `physics.parented_bodies` counter; `Static`/`Kinematic` bodies under a group node are the normal,
     supported case and are not warned, ADR-5613).
- **Interpolation:** `PostUpdate/PhysicsSync` interpolates rendered transforms of dynamic bodies between
  previous and current fixed-step poses using `FixedTime::overstep_fraction()` (per-body opt-out).
- Conversion between glam and rapier's `glamx`/nalgebra types centralised in `convert.rs`.

## 2. Components (all `Reflect`, editable)
| Component | Fields |
|---|---|
| `RigidBody` | `kind: Dynamic|Kinematic|Static`, linear/angular damping, gravity scale, `ccd`, `can_sleep`, `locked_axes` (translation/rotation per axis), dominance group |
| `Mass` | `Auto` (from density) or explicit mass + center of mass + principal inertia |
| `Collider` | `shape`, `offset: Transform`, `sensor: bool`, inline friction/restitution/density + combine rules; a `PhysicsMaterialRef(name)` beside it takes the values of a `.physmat.ron` `PhysicsMaterial` from the `PhysicsMaterials` registry (ADR-2424, `docs/formats/physmat.md`), `layer: CollisionLayer`, `active_events` |
| `ColliderShape` | `Box{half_extents}`, `Sphere{radius}`, `Capsule{half_height, radius, axis}`, `Cylinder{half_height, radius, axis}`, `Cone{half_height, radius, axis}`, `ConvexHull{points}`, `TriMesh{vertices, indices}` (mesh-asset forms not built; ADR-2250) (static/kinematic only), `HeightField` (terrain, M13/M14), `Compound{shapes: Vec<(Transform, Shape)>}` |
| `Velocity` | linear, angular (read/write) |
| `ExternalForce`, `ExternalImpulse` | applied next step then cleared (impulse) |
| `Joint` | `Fixed`, `Revolute{axis, limits, motor}`, `Prismatic{axis, limits, motor}`, `Spherical{cone limits}`, `Distance{min,max}` (a rope is `min == 0`), `SwingTwist` (ragdoll limbs, spec 93); `connected: Entity`, anchors, `break_force` (Tier 1.5), and a `JointBreakTorque(n)` component beside it for a torque limit (ADR-2424) |
| `CharacterBody` | used by `regia_framework::Character` (see § 4) |

A child entity with a `Collider` and no `RigidBody` attaches to the nearest ancestor body (compound).

## 3. Layers, events, queries
- **Collision layers**: up to 32 named layers defined in `config/physics.ron` with a symmetric
  collision matrix and a separate query matrix; mapped to rapier interaction groups. The editor's layer matrix
  (Project Settings, `editor.physics_layers_edit`, ADR-7233) writes the file in canonical RON; a game loads it with
  `LayerFile::load` + `PhysicsLayers::from_file`.
- Events (ECS events + routed to scripts as `ScriptEvent`): `CollisionStarted{entity_a,entity_b}`, `CollisionEnded`,
  `TriggerEnter{trigger, other}`, `TriggerExit`, `ContactForce{entity_a,entity_b,total_force}` (threshold per
  collider; routed to both participants' scripts as `ScriptEvent::ContactForce{entity, other, total_force}`, ADR-7233).
- Queries (`PhysicsQuery` system param + `ScriptCtx::physics()`): `raycast`, `raycast_all`,
  `shapecast(shape, from, dir, max)`, `overlap(shape, pose)`, `closest_point`, with `QueryFilter
  { layers, exclude_entities, exclude_sensors, predicate }` (set through builder methods). Results return
  `Entity`, point, normal, distance/toi; `closest_point` returns the point, whether the query point is inside, the
  outward surface normal and the signed distance (negative inside; the nearest surface point is
  `query - normal * distance`, ADR-7233).
- Debug draw: colliders (`physics` gizmo group), contacts, joints and center of mass (`regia_physics::debug_draw`, agent `physics.debug_draw {on, kinds}`; free while off, ADR-2424); AABBs are not drawn.

## 4. Character controller
- Wraps rapier's `KinematicCharacterController`: `move_shape(desired_translation)` with
  `max_slope_climb_angle`, `min_slope_slide_angle`, autostep (height, min width, include dynamic
  bodies), snap-to-ground, `offset` (skin width), slide.
- Regia Engine adds: ground detection with ground normal + ground entity, moving-platform support (inherit
  ground body's velocity and yaw), pushing dynamic bodies (apply impulse proportional to character mass),
  ceiling hit detection, landing (`CharacterState::landed_this_frame`, a flag rather than a message). A moving
  platform's yaw rate is reported in `CharacterGround` and carries the character's position around the spin
  axis; turning the character's facing with it is not built (ADR-5613). Over kinematic ground that moves, the
  move has two parts (ADR-5972): the character first moves with the ground, stopped only by what does not move
  with it, then makes its own move over the ground with no time step given to rapier's controller, whose own
  carry for kinematic ground would otherwise add the ground's motion a second time after a contact.
- `samples/physics_lab` (ADR-2424): a headless run of a stack, torque-limited levers, a chain, ice and default boxes on a ramp, a ragdoll drop and the debug draw, with an agent dogfood test, plus a traversal course (swim, vault, mantle, ladder; ADR-2971). The character-controller scenarios are tests, not lab scenes (ADR-5613): stairs, walkable and steep slopes, moving and rotating platforms (jitter under 1 cm) and pushing boxes in `regia_framework/tests/character_physics.rs`, and stairs, ceilings and pushing in `regia_physics/tests/character_controller.rs`. One-way-ish triggers are not built.

## 5. Determinism and performance
- Same build + same platform + same inputs → identical results (rapier is deterministic per platform);
  feature `enhanced-determinism` behind Regia Engine feature `physics-cross-platform-determinism` (off
  by default; costs speed) makes a measured subset of rapier bit-identical across the targets measured
  (ADR-8511). It is not a lockstep promise (ADR-8841).
- Lockstep games (ADR-8841): a lockstep simulation reads only the steps and queries of the lockstep closure
  (`docs/ci/lockstep-closure.toml`: `regia_detsim`, `regia_world_geom`, `regia_nav::det`, `regia_rules`,
  `regia_net::lockstep`). Rapier output never feeds back: the simulation reads no rapier pose, velocity, contact,
  query hit or event. Hover, ground vehicles, projectiles, body contacts and world gravity have lockstep forms in
  `regia_detsim`; rigid bodies (stacking, joints, ragdolls, cloth, destruction) are presentation-only in a lockstep
  game and not promised there until the owner rules otherwise. The table per model and genre is in the book page
  "Movement models".
- Rapier `parallel` feature off by default (avoids rayon pool contending with the engine pools); ADR to
  revisit if large scenes need it.
- Budgets (desktop reference): 2,000 active dynamic boxes ≤ 4 ms per step; mobile reference: 300 ≤ 4 ms.
- The result does not depend on the thread count or the executor: rapier objects are created in the order their
  components arrived (`regia_ecs::spawn_order`, ADR-1347), removed or rebuilt in rapier handle order (which is that
  insertion order; ADR-2396), and no simulation order comes from a hash map (ADR-870). `PhysicsWorld::state_digest`
  (agent `physics.stats`) hashes every body's pose, velocities and sleep flag so runs can be compared.
- `PhysicsSettings::solver_iterations` (default 4) and `ccd_substeps` (default 1) tune the solver (ADR-870).
- Removals are recorded by `Remove` observers and applied before the next step, so they are never lost to frames that
  run no fixed step (ADR-870).
- Non-finite numbers never reach rapier (ADR-1507, finishing ADR-383's open half): a NaN or infinite `Transform`
  parks a new body at the origin and is skipped as a kinematic target or `Teleport`; a NaN velocity, force, impulse,
  damping or gravity scale is not pushed, and neither is an explicit mass whose mass or principal
  moments are not finite and strictly positive; a non-finite local transform in a compound-collider chain is
  skipped; a simulated pose that still goes non-finite is never written back to the scene. Every drop is counted in
  `PhysicsStats::non_finite_inputs` (agent `physics.stats`), so a NaN writing bug is visible instead of a pile
  silently falling apart.

## 6. Acceptance (M20)
- Unit tests: create/remove/change shape round trips; layer matrix filtering; events delivered exactly
  once; raycast correctness vs analytic shapes.
- Robustness (ADR-870): a 21-box pyramid and a 16-box tower hold; a 100:1 mass ratio stack does not sink; CCD stops
  400 m/s balls at a 5 cm wall and a CCD ball head-on into a slow ball (a CCD body is swept against the others'
  end-of-step pose, so two fast bodies closing head-on can pass through each other, ADR-1387); ball-joint chains stay within 10 cm, revolute and prismatic limits hold, a
  motor reaches its target velocity; joints come back with their bodies and stay broken after `break_force`;
  resting piles sleep and wake when their floor goes, on impulses, velocity and force edits; removed colliders
  still report their end events; bit-identical runs across processes, executors and 1/2/4/8 compute threads;
  seeded property tests (finite state, nothing under the floor, no energy gain, chains hold).
- Stacking test: 10-box tower stable for 60 s; joint chain sample; the character controller passes the
  scripted checks (reaches top of stairs, does not climb a 60° slope, rides a platform without jitter > 1 cm)
  as the tests named in § 4.

## 7. Destruction level 2 (ADR-1597)

`regia_physics::destruction`, beside the level 1 cosmetic debris (`DebrisSpawner`, unchanged):

- **Asset**: `.destructible.ron` v2, v1 migrates (`docs/formats/destructible.md`): chunks (mass, convex hull, bounds, support
  flag) and bonds (area, anchor, normal, break impulse in N s), cooked from a `.fracture.ron` by
  `regia_asset_pipeline::destructible_cook` or `regia fracture`, or built as a box grid.
- **Break model** (`DestructionWorld`): hits (explosion, projectile, impulse) are applied once per fixed tick in
  queue order; a chunk's impulse is shared by its intact bonds and accumulates; bonds at threshold break in
  (instance, bond) order under `max_breaks_per_tick`; chunks with no intact path to a support chunk collapse.
  Decisions use only IEEE-exact `f32` operations on plain arrays, so they are bit-identical on every platform and
  independent of rapier and the renderer; `BreakEvent`s (seq, tick, ids, seed) are what a server sends and an
  `Authority::Replica` replays to the same `structure_hash`.
- **Rapier side** (`DestructionPhysics`): one fixed body per structure with a hull collider per chunk; a detached
  chunk's collider leaves and a pooled dynamic body takes its place (`max_spawns_per_tick`, `max_active_chunks`,
  sleep / lifetime / kill-height despawn). Debris motion is cosmetic.
- **Hooks**: `DestructionBroke`, `DestructionFxMessage`, `NavmeshInvalidated` messages; agent methods
  `physics.destruction_*` (`book/agents/destruction.md`).
- **Network** (`regia_framework::destruction_net`, feature `net`, ADR-1967): the server decides; a per-peer
  reliable stream carries instance changes, breaks in a short form the replica rebuilds bit for bit, and hash
  checks, after a snapshot for a late joiner, itself paced over ticks with the stream waiting behind it. A replica
  that loses step asks for a new snapshot. ADR-2305: the stream waits on the server per peer and catches up after
  the snapshot; big instances go in pieces; a structure with a relevance anchor goes only to the peers that hold
  the anchor (spec 72), its breaks skipped for the others with the sequence kept; `DestructionDrawPlugin` draws the
  cooked chunk meshes on break; the navmesh source uses the chunks' convex hulls.
- **Acceptance**: the wall-and-bridge sample breaks the same way on every run and platform (pinned hashes in
  `crates/regia_physics/tests/destruction.rs`); a replica replaying the events matches every tick; caps hold.
- **Editor and levels** (ADR-9750, as built): the Fracture panel marks support chunks (clicks, a box, or the
  `.meta`'s `support_below_y` plane, the only rule a re-cook keeps), runs a test break on the saved cook's
  `DestructionWorld` (same `structure_hash` as a game fed the same inputs), draws bonds by break impulse, and
  **Place** spawns a `DestructibleSource { asset, seed }` entity that `DestructionPlugin` defines at play
  (`PreUpdate`, outside the fixed tick). Site sampling (ADR-9870): besides the seeded best-candidate kind and
  explicit/grid sites, `FractureSites::Impact { at, bias }` biases the sites toward a hit (fine debris there, larger
  chunks away; density per unit volume `1/r^bias`, flat within 1 % of the diagonal of the hit, ADR-7550) and `FractureSites::Clusters { clusters, spread }` cuts clumps of fine chunks; the panel exposes both as
  preview choices the `.meta` does not store. Not built: a box support rule in the `.meta` (a format bump), `.meta`
  storage for any site choice.
- **Limits** (level 2): intact structures do not move as a whole; collapsing sections break into chunks; bonds
  carry hits, not gravity load. Level 3 (runtime fracture, carving) is a v1.2 candidate.
- **As built (ADR-7431, v0.6 follow-ups):** impact cooldowns are capped at `MAX_IMPACT_COOLDOWNS` (the soonest-ending
  are dropped first) and looked up by binary search; wheel settings are capped at `MAX_VEHICLE_SETTING` (1e9); a static
  collider resized in place forces one rebuild of a vehicle's private world; `vehicle_track::step_for` refuses more than
  `MAX_TRACK_STEPS` steps.
