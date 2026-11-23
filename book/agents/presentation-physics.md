# Presentation physics

`sim_bridge.presentation_stats` reads the counters of
`regia_framework::sim_bridge::presentation` (ADR-8356). The plugin places kinematic hulls, a
drawn-ground collider and debris beside an authoritative sim. It is not on the lockstep closure
and it does not feed anything back to the sim.

Add `regia_physics::PhysicsPlugin`, your `SimBridgePlugin`, and
`PresentationPhysicsPlugin { hulls, debris_lifetime_s }`. `HullShapes` maps a sim kind to any
`ColliderShape`. With the `client` feature, insert `PresentationGround::from_terrain` before
`Startup` so the ground collider and the hole cells the draw skips come from the same map.
Send a `DebrisBurst` to spawn chunks that have no sim id. `debris_lifetime_s` defaults to 30
simulated seconds.

A dynamic body on an entity with a sim id is removed before the physics step. The module warns
and increments `refused_writes`. The bridge keeps the snapshot `Transform`.

## Read the counters

```json
{"method":"sim_bridge.presentation_stats","params":{}}
```

The result is:

```json
{
  "hull_bodies": 1,
  "debris_alive": 8,
  "refused_writes": 0,
  "ground_colliders": 1,
  "bursts": 1,
  "debris_spawned": 8,
  "debris_skipped": 0
}
```

`hull_bodies` and `debris_alive` are live counts. The other fields accumulate. Permission:
`read`. The method does not change the world.

## Errors

| Code | When |
| --- | --- |
| `presentation_physics_unavailable` | `PresentationPhysicsPlugin` is not in this app. Add `regia_framework::sim_bridge::presentation::PresentationPhysicsPlugin`. |

`presentation_physics_unavailable` is lowercase ASCII with underscores. The call needs no
parameters. A project that has not added the plugin gets this code and an empty counter set is
not invented.
