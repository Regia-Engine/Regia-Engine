# Physics: raycasts, overlaps and closest points

Query the live physics world — find what a ray hits, who is inside a volume, what is
nearest to a point. Read-only, no gameplay driven. These need
`regia_physics::PhysicsPlugin`; `agent.hello` lists the `physics` capability when it is
installed. Without it the calls return `NOT_FOUND` with a hint.

## Raycast

`physics.raycast` casts a ray in meters, world space. `direction` need not be normalized.

```json
{ "origin": [0, 5, 0], "direction": [0, -1, 0] }
→ { "hit": { "entity": 4294967296, "point": [0, 0, 0],
             "normal": [0, 1, 0], "distance": 5.0 }, "count": 1 }
```

- `max_distance` (default 100) caps the cast; `exclude_sensors` (default `true`) skips
  trigger volumes — set it to `false` to include them.
- `all: true` returns every hit sorted by distance: `{ "hits": [...], "count": N }`.
- `hit` is `null` on a miss — a good `play.expect`-free way to assert "nothing above X".

## Overlap

`physics.overlap` lists every entity whose collider intersects a query shape:

```json
{ "shape": { "kind": "sphere", "radius": 1.0 }, "position": [0, 0.5, 0] }
{ "shape": { "kind": "box", "half_extents": [0.5, 0.5, 0.5] }, "position": [0, 0.25, 0] }
{ "shape": { "kind": "capsule", "half_height": 0.9, "radius": 0.3 }, "position": [0, 1.2, 0] }
→ { "entities": [4294967296, 8589934592], "count": 2 }
```

Shape dimensions are required: `radius` for spheres, `radius` and `half_height` for capsules,
and `half_extents` for boxes. Each dimension must remain positive and finite in `f32`; missing
or zero dimensions return `invalid_params`.

Sensors are included by default (`exclude_sensors: false`); use it to check trigger
occupancy: who is standing in the volume.

## Closest point

`physics.closest_point` projects a point onto the nearest collider surface:

```json
{ "point": [0, 2, 0] }
→ { "hit": { "entity": 4294967296, "point": [0, 0, 0], "is_inside": false,
             "normal": [0, 1, 0], "distance": 2.0 } }
```

`max_distance` bounds the search (default unlimited); `hit` is `null` when nothing is
within it. `is_inside: true` means the query point was inside the shape (or on its boundary).
`normal` is the outward unit normal of the nearest surface and `distance` the signed distance to it:
positive outside, negative inside (the penetration depth), so the nearest surface point is always
`point - normal * distance` with the query point (ADR-7233). Inside a shape, `point` is the query point
itself.

## Teleport

`physics.teleport` moves a rigid body to a pose on the next fixed step without simulating the way
there (ADR-7233): the jump makes no contact and starts no CCD sweep, a kinematic body gets no
velocity from it, and interpolation does not blend across it.

```json
{ "entity": 4294967296, "position": [0, 1, 0], "rotation": [0, 0, 0, 1], "velocity": "clear" }
→ { "entity": 4294967296, "position": [0, 1, 0], "rotation": [0, 0, 0, 1], "velocity": "clear" }
```

`position` is the entity's `Transform` translation (parent-relative when it has a parent);
`rotation` defaults to the current one. `velocity` is `keep` (default: the body carries on at the
velocity it had) or `clear` (it arrives at rest). An entity that does not exist or has no
`RigidBody` answers `F0003`; a bad `velocity` name or a zero or non-finite quaternion answers
`F0002`. Verify with `play.step` then `scene.get_entity` (the `Transform` is the target) or
`physics.stats`. Game code uses `regia_physics::teleport(world, entity, pose, TeleportVelocity::Keep)`,
`commands.entity(e).teleport(pose, TeleportVelocity::Clear)` (`TeleportCommands`) or
`ScriptCtx::teleport`; scripts also receive `ScriptEvent::ContactForce { entity, other, total_force }` on
both participants when a pair's contact force crosses a collider's `contact_force_threshold`.

## Working with the results

- Entity ids interop with everything: `scene.get_entity` for components, `play.expect`
  conditions, `scene.set_property` for edits.
- Positions are the **live stepped** collider poses (the same world the character
  controller moves against), not the interpolated render transforms.
- These three calls do not set `QueryFilter::from_layer`. With `PhysicsLayers` installed,
  a collider is hidden from them only when its collision-filter bits are empty (its layer
  collides with nothing, or its index is past the layer table). The query matrix is not
  consulted: a layer that nothing may query, but that still collides with something, stays
  visible. A query that passes `from_layer` is what applies the matrix (specs/15 § 3).
- To test dynamics (settling, stacking), pause and step first: `play.pause` then
  `play.step { "frames": 120 }`, then raycast to check where things landed.

## World stats and determinism

`physics.stats` reports what the physics world is doing (no parameters):

```json
{}
→ { "bodies": 22, "dynamic": 21, "kinematic": 0, "fixed": 1,
    "awake": 0, "sleeping": 21, "ccd_bodies": 0,
    "colliders": 22, "sensors": 0, "joints": 0, "contact_pairs": 35,
    "steps": 1200, "last_step_ms": 0.08,
    "non_finite_inputs": 0,
    "solver_iterations": 4, "ccd_substeps": 1,
    "digest": "9c1e0b7d44a2f310" }
```

- `awake` / `sleeping` count dynamic bodies only. A settled pile should be all
  `sleeping`: asleep bodies cost no solver time. A pile that never sleeps is
  jittering. Check stacking with `play.step`, then `physics.stats`.
- `non_finite_inputs` counts non-finite poses, body fields, velocities, forces and
  impulses dropped at the rapier seam (ADR-1507): rapier never sees them, so one NaN
  `Transform` cannot poison a whole pile. Zero in a healthy app; a growing count means
  a script, a deserialized scene or a networked value is writing NaN or infinity. Find
  the writer, fix it at the source — the guard keeps the world alive, it does not make
  the value meaningful.
- `last_step_ms` is the wall time of the last rapier step alone. Sync and write-back are
  not included; the frame profile has those. Use it to compare scene sizes, not as a
  budget check.
- `digest` is a 64-bit hash of every body's pose, velocities and sleep flag, bit-exact,
  in 16 hex digits. Two runs of the same build with the same inputs have the same digest
  at the same `steps`. To check that a scene replays deterministically, reset or reload
  it, step the same number of frames, and compare digests. A mismatch means something
  fed different inputs, for example wall-clock time or unordered iteration in gameplay
  code.
- `solver_iterations` and `ccd_substeps` echo `PhysicsSettings`. Raise
  `solver_iterations` (default 4) for tall stacks, long chains and large mass ratios.
  Raise `ccd_substeps` (default 1) so a fast CCD body keeps the rest of its motion after
  an impact. Both are set by the game in `PhysicsSettings`; the protocol only reads them.
  A CCD body is swept against static geometry and against bodies without `ccd` where they
  end the step, not against another CCD body: two fast projectiles fired at each other can
  pass through (ADR-1387).

Without `PhysicsPlugin` the call returns `NOT_FOUND` with a hint, like the queries.
