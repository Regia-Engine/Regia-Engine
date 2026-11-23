# Explosions

One gameplay call makes an explosion do all of it (`regia_framework::explosion`, ADR-2430). An
`Explosion` is a center, a radius, a strength and a few options. Send it as an `ExplosionEvent`
message, call `explosion::explode(world, explosion)`, or call the agent method `gameplay.explode`.
On the next fixed step, before the physics step, `apply_explosions`:

1. **Pushes bodies.** Every dynamic rigid body whose center of mass is inside the radius gets an
   `ExternalImpulse`: `mass * peak_delta_v * falloff(distance)` along the direction away from the
   center (tilted up by `upward_bias`), plus a torque from the offset of the hit point (where the
   line from the center of mass toward the blast leaves the body's bounds) from the center of mass.
   Bodies are visited in ascending entity order and several explosions in one step add up, so the
   result is deterministic.
2. **Breaks joints through the solver.** Nothing new: the impulse reaches the jointed pair and
   the existing `break_force` check removes the joint. `joints_broken` counts the `JointBroken`
   messages of the 8 fixed steps after an explosion.
3. **Forwards the blast to destructibles.** One `DamageKind::Explosion { center, radius, impulse }`
   goes to `Destruction`. The chunk and root bodies of a destructible are skipped by step 1, so a
   chunk receives the blast once (through the damage path). Debris already in flight gets the same
   push as any other body in reach, one blast after another, and wakes if it slept.
   `chunks_hit` counts the chunks reached when the blast goes off: attached chunks whose center is
   inside the radius, plus the flying ones (`chunks_pushed`), whether or not a bond breaks.
4. **Plays the effects (feature `explosion-fx`).** `ExplosionFxPlugin` fires the `regia_vfx::crash`
   layers at `CrashScale::from_tonnes(tnt_kg / 1000)` and schedules the seismic and airblast mount
   shakes of `GroundShock { yield_kt: tnt_kg / 1e6 }` at the first camera rig. A replica plays
   them too.
5. **Stirs cloth.** When a cloth solver's centroid lies inside the radius, a radial `WindField`
   entity (strength half the peak speed change, clamped to at least 1 m/s and at most 60 m/s)
   lives for 30 fixed steps.

Only an authoritative world applies force to gameplay. With a `Destruction` resource its `Authority`
decides; without one, `ExplosionSettings::authority`. A `Replica` pushes no gameplay body and queues
no damage (`skipped_replica`), but it does push its own flying debris: debris is cosmetic and every
process simulates its own. Force reaches a replica only if the game tells it about the blast (send
your own net message, then write `ExplosionEvent` there; [`samples/destruction_lab`](destruction-net.md#a-sample-to-copy) shows it).

`ExplosionPlugin` is not in any default plugin set: add it yourself (after `PhysicsPlugin`), so
the default simulation and its determinism traces are unchanged until a game opts in.

## The law

- `impulse` is the impulse in newton-seconds that a 100 kg body receives at the center. A body of
  mass `m` receives `m / 100` times that: every body gains the same speed at the same distance
  (`peak_delta_v = impulse / 100`, at most 80 m/s). The same number is the destruction impulse.
- `Falloff::Linear` is `1 - d / radius`. `Falloff::InverseSquare { floor }` is `(floor / d)^2`
  (1 inside `floor`), shifted so it is exactly 0 at the radius. `gain` clamps a finite floor
  above 0 to at most half the radius, and replaces any other floor with 10 % of the radius.
  The result's `floor` is the authored value, not the value `gain` used.
- `Occlusion::RayCast` casts one ray from the center to the body's center of mass (the last 5 cm
  are left out). A hit on any static or kinematic collider (sensors and the body itself ignored)
  gives the body zero impulse (`bodies_occluded`). A collider on a child entity belongs to its
  body: it never shields that body, and it shields others only when that body is static or
  kinematic. `Occlusion::None` never shields.
- `from_tnt(kg)`: radius `12 m * cbrt(kg)` (at least 1.5), peak speed change `25 m/s * cbrt(kg)`.
  The radius is not clamped to 500 m. Above about 72 338 kg the radius fails `validate`
  (`bad_radius`) while `tnt_kg` is still under 1 000 000 kg.
- Presets: `grenade` (0.2 kg), `barrel` (5 kg, inverse-square, 1.5 m core, tilt 0.35), `shell`
  (15 kg, tilt 0.1), `bomb` (250 kg, inverse-square, 6 m core, tilt 0.15).

## `gameplay.explode`

`{ center: [x, y, z], preset?, tnt_kg?, radius?, impulse?, falloff?: "linear" | "inverse_square",
floor?, upward_bias?: 0..1, occlusion?: "none" | "ray", torque?, seed?, fx?, destruction?, gust? }`.
Name a `preset`, a `tnt_kg`, or both `radius` and `impulse` (then no effects are sized: `tnt_kg`
is 0). `radius` and `impulse` override the preset. Result: `{ queued: true, explosion: { center,
tnt_kg, radius, impulse, peak_delta_v, falloff, upward_bias, occlusion, torque, seed }, presets }`.
The explosion applies on the next fixed step.

## `gameplay.explosion_stats`

No params. Result: `{ explosions, skipped_replica, bodies_hit, bodies_occluded, impulse_total,
joints_broken, damage_queued, chunks_hit, chunks_pushed, gusts, fx_played, shakes }` (counts since
the plugin was added; `impulse_total` is the sum of the linear impulses' magnitudes in N*s).

## Errors

Bad parameters answer `INVALID_PARAMS` (`F0002`) with the reason in `data.reason`:

| Reason | Meaning |
|---|---|
| `bad_center` | `center` is not three finite numbers |
| `bad_radius` | `radius` not in (0, 500] |
| `bad_impulse` | `impulse` negative or not finite |
| `bad_tnt` | `tnt_kg` negative, not finite, above 1000000, or not a number |
| `bad_upward_bias` | `upward_bias` outside 0..1 |
| `bad_torque` | `torque` negative or not finite |
| `unknown_preset` | not `grenade`, `barrel`, `bomb` or `shell` |
| `missing_size` | no `preset`, no `tnt_kg`, and not both `radius` and `impulse` |
| `bad_falloff` | `falloff` is not `linear` or `inverse_square` (or `floor` is not a number) |
| `bad_occlusion` | `occlusion` is not `none` or `ray` |

A world without `ExplosionPlugin` answers `UNSUPPORTED` (`F0016`), reason `no_explosion_plugin`.

## Not built

- Per-particle cloth gust (the cloth solver samples one wind at its centroid).
- A chunk that detached but still waits for a body (the per-tick spawn cap) gets its velocity from
  the break alone: a blast only pushes chunks that have a body when it goes off.
