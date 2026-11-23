# Physics: force fields and wind on rigid bodies

Jump pads, wind tunnels, gravity wells, slow zones and leaves blown across a table need no host
code: put a `ForceField` on an entity and the dynamic bodies inside its volume feel it
(`regia_physics::fields`, ADR-3348). Quadratic wind drag from the scene wind is a second
component, `WindDrag`, in `regia_framework::physics_wind`.

## What a field is

A `ForceField` is one effect (`kind`) in one region (`volume`), placed by the entity's
`Transform` (volume sizes ignore entity scale). A field on a child entity is placed by its
composed world pose (its own `Transform` relative to its ancestors), which is current within the
fixed step.

| `kind` | Parameters | Effect |
|---|---|---|
| `directional` | `vector` (field-local axes) | A constant push; rotate the entity to aim a wind tunnel. |
| `radial` | `strength`, `falloff` | Toward the center for positive strength, away for negative. |
| `vortex` | `tangential`, `inward`, `axis`, `max_speed`, `falloff` | A swirl around `axis` through the center. The tangential push fades to zero at `max_speed` and reverses above it, so the swirl speed settles there. |
| `gravity` | `gravity` (field-local) | Replaces world gravity inside the volume; the highest `priority` wins, then the latest spawned field. Leaving the volume restores normal gravity (and `gravity_scale` still applies). |
| `linear_drag` | `coefficient` | `F = -k v`. A body falling through it settles on `m g / k`. |
| `angular_drag` | `coefficient` | `torque = -k ω`. |
| `impulse_pad` | `impulse` (field-local) | One impulse each time a body **enters** the volume; the velocity changes by exactly `J / m`. Staying inside adds nothing; leaving and re-entering fires again. |

`volume`: `global` (everywhere; radial and vortex use the entity position as center), `sphere
{radius}`, `box {half_extents}` or `capsule {half_height, radius}` (along local Y). A body is
inside when its center is.

`falloff` (radial and vortex): `none`, `linear {range}` (`1 - d/range`), `inverse_square
{min_distance}` (`1/max(d, min)^2`, so `strength` is the force at one meter) or `curve {points:
[[distance, factor], ...]}` (up to 64 points sorted by distance, linear between, clamped at both
ends).

`mode`: `force` (default; newtons, divided by the body's mass; drag in N s/m; impulse in kg m/s)
or `acceleration` (m/s², 1/s, velocity change: the same for every body, which makes a jump pad
that throws a feather and an anvil alike).

`mask`: bit `i` set means bodies whose collider is on collision layer `i` are affected (default
all bits). Every collider attached to a body counts, including colliders on child entities (compound
bodies): a body is affected when any of its colliders sits on a selected layer. A body without a collider uses a bare `CollisionLayer`, else layer 0; a body whose colliders are all on layers past 31 is on no field layer. `mask` must fit in
an unsigned 32-bit integer and `priority` in a signed 32-bit integer; a value outside that range is
`invalid_value` and the field is left unchanged.

`enabled: false` switches a field off; an impulse pad then forgets who was inside. Removing a
field forgets only that field's occupants: the other pads keep theirs and do not fire again.

## How it runs

`apply_force_fields` runs in `FixedUpdate` inside `PhysicsStep`, after the velocity sync and
before `step_physics`, only while some `ForceField` exists. Bodies are visited in entity order and
each body folds the fields latest spawned first, so a run is a function of the fixed steps and inputs.
A disabled body (frozen by streaming, a parked vehicle) is skipped: fields add no velocity to it
while it waits, and it keeps the impulse pads it was inside.
Per step: `v' = v (1 - drag dt) + a dt + dv_pad`, then rapier adds gravity. Drag is clamped so a
huge coefficient stops a body rather than reversing it. Fields write the body's velocity, not
`ExternalForce`, so they never fight gameplay forces; a body inside a non-zero field is woken
each step.

Scenes without a `ForceField` run exactly as before (the system is behind a run condition).

## Wind

`WindDrag { area, cd }` on a dynamic body gives it `F = -½ ρ cd A |u| u`, with `u` the body's
velocity relative to the air. Add `regia_framework::physics_wind::PhysicsWindPlugin` after
`PhysicsPlugin`. The air moves with `SceneWind::velocity3()` (the wind **with gusts**, applied at
every height); with no `SceneWind`, or in `SceneWindMode::Off`, the air is still. `AirDensity`
(resource, default 1.225 kg/m³) sets ρ. A body falling in still air settles on
`sqrt(2 m g / (ρ cd A))`.

## Agent methods

### `physics.fields`

`{ entity? }` → `{ count, pad_entries, fields: [{ entity, kind, volume, mode, mask, enabled,
priority, position, rotation }] }`. `pad_entries` counts impulse-pad entries delivered so far.
`position` and `rotation` are the world pose the next fixed step uses: for a field on a child
entity, the composed `Transform` chain, not the `GlobalTransform` that propagates later in the
frame (`physics.field_probe` places fields the same way).

### `physics.field_set`

Creates (no `entity`, or an entity with no field; `kind` required), edits (only the keys given
change) or removes (`remove: true`) a field. `position`/`rotation` are world space for any
entity: for a child entity the local `Transform` is written relative to its parent's composed
world pose, and the reported `position`/`rotation` are the composed world pose.

```json
{ "kind": { "type": "directional", "vector": [0, 30, 0] },
  "volume": { "type": "box", "half_extents": [2, 5, 2] }, "position": [0, 5, 0] }
→ { "entity": 4294967310, "created": true, "kind": {...}, "volume": {...}, "mode": "force", ... }
```

### `physics.field_probe`

`{ point, mass?, velocity?, angular_velocity?, layer? }` → `{ force, torque, acceleration,
gravity_override_force, entry_impulse, fields: [{ entity, force }] }`: what a mass would feel at
a point, field by field, without moving anything. Drag uses the velocity you pass.

## Error codes

Malformed JSON is `F0002`; a missing `entity` is `F0003`. A value a field cannot take answers
`{ "status": "failed", "reason": ... }` with one of:

| `reason` | Meaning |
|---|---|
| `unknown_kind` | `kind.type` is not one of the kinds above. |
| `unknown_volume` | `volume.type` is not global, sphere, box or capsule. |
| `unknown_falloff` | `falloff.type` is not none, linear, inverse_square or curve. |
| `unknown_mode` | `mode` is not force or acceleration. |
| `missing_kind` | Creating a field without `kind`. Nothing is created. |
| `no_field` | `remove` on an entity that has no field. |
| `invalid_value` | A negative volume size, an unsorted or oversized curve, a zero quaternion, a non-positive probe mass, a `mask` or `priority` outside its 32-bit range, a probe `layer` past 31. |
| `non_finite_input` | A NaN or infinite number. |

## Notes

- Fields move bodies by changing their velocity before the step, so a body held by a joint or
  resting on a wall still gets the push and the solver resolves it.
- The pinned determinism digest of 500 bodies in four fields is in
  `crates/regia_physics/tests/force_fields.rs` (Linux x86_64).
