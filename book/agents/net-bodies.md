# Networked rigid bodies (agents)

Topic `net-bodies` (ADR-2902). Crates, barrels and thrown objects in a networked game: the server
simulates them with rapier, every client shows them. Characters and vehicles have their own paths
(`net-character`, vehicles); this page is about everything else that falls, rolls and gets pushed.

## Setting it up

Add `regia_framework::net_body::NetBodyPlugin` on every process, after `NetPlugin` and
`PhysicsPlugin` (the `net` feature of `regia_framework`). Then give each body:

- `Replicated { transform: Movement, profile: Some("body".into()), .. }`;
- `ReplicatedBody` (replicated, so client copies know they are bodies). Its `shape` gives copies a
  collider when they have none (`Cuboid`, `Ball`, `Capsule`, `Cylinder`); `FromEntity` (the default)
  means a prefab brings `RigidBody` and `Collider` on every process;
- on the server, a dynamic `RigidBody` (added for you when `shape` is not `FromEntity`).

Only root bodies are supported: the stepped pose is a world pose.

## What happens

On the server, after each physics step:

- a body that moved marks its `Transform` changed (the step writes it without change detection), so
  the transform fast path sends it on the profile's 1 mm grid with a smallest-three rotation;
- its linear velocity goes to `NetVelocity`, rounded to `velocity_step` (0.01 m/s) and zero under
  `rest_speed` (0.02 m/s), so a resting copy holds instead of drifting;
- when rapier puts the body to sleep it gets `NetAsleep`. Replication keeps sending until every
  client acknowledged the final pose, then sends nothing: **a sleeping body costs zero bytes**.
- when rapier wakes it (an impact, an impulse, a character push) the marker goes and the move is sent
  in the same tick;
- `NetPriorityScale = 1 + speed × speed_priority` (capped at `max_speed_priority`, 4): a tumbling crate
  wins the bandwidth budget over a slow one. Distance comes from the `body` profile (spatial, 150 m).

On a client, a copy is a **kinematic proxy**: spec 74 interpolation writes its `Transform` (linear
blend, extrapolation capped at 100 ms, a decaying recovery offset after a late state, snaps over 5 m
or on a teleport) and rapier follows it. Characters stand on and walk into what the player sees.
Physics render interpolation is off for copies (`NoInterpolation`).

Pushed bodies are **not predicted** on the pushing client (ADR-2902): a client that walks into a crate
is blocked where the crate is shown and corrected when the server's push arrives, a few small
corrections per second of pushing with no pop.

`net_body_teleport(world, entity, pose)` moves a server body at once: rapier teleports it and clients
snap (no in-between frames).

## The `body` profile

| Field | Value | Why |
|---|---|---|
| `priority` | 1.0 | between props (0.5) and players (3.0); speed scales it |
| `relevance` | spatial, 150 m, hysteresis 0.15 | bodies far away do not matter |
| `max_interval_ticks` | 15 | a moving body is sent at least four times a second |
| `position_step_m` | 0.001 | sub-millimeter jitter makes no new state |
| interpolation | linear, snap 5 m, extrapolate 100 ms | spec 74 defaults |

A settled copy is within `body_position_tolerance(profile)` (half a grid step per axis, about 0.97 mm
in all) and `BODY_ROTATION_TOLERANCE_RAD` (0.005 rad) of the server.

## `net.bodies`

| Param | Type | Meaning |
|---|---|---|
| `limit` | integer, optional | rows to return, 0..=10000 (default 64); counts always cover every body |

Returns `{ side, bodies, asleep, awake, proxies, bytes_per_second, fell_asleep, woke, moved_steps,
proxies_followed, teleports, rows }`. Each row: `{ entity, net_id, role, kind, asleep, position,
velocity, bytes_per_second }`.

- `asleep`: rapier sleeping on the server; on a client, a copy held at zero velocity.
- `bytes_per_second`: state bytes for bodies over the last 5 s, summed over connections. Only on the
  server with the relevance pipeline (`install_net_interest`); `null` elsewhere.
- `fell_asleep`, `woke`, `moved_steps`: server counters; `proxies_followed`: copies made kinematic.

How to read it: on a quiet scene `bytes_per_second` should be 0 with `asleep == bodies`. A body that
never sleeps (a jittering stack, `can_sleep: false`) shows in `awake` and costs bytes every send.

## Errors

`F0002` with `data.reason`:

| Reason | When |
|---|---|
| `net_bodies_not_installed` | no `NetBodyPlugin` on this process |
| `limit_out_of_range` | `limit` above 10000 (a `limit` that is not a non-negative integer is `F0002` with `data.param`) |
