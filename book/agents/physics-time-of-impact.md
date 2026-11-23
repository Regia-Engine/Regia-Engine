# Physics: time of impact (angular CCD sweep)

`physics.time_of_impact` answers "do these two shapes touch while they move, and when?"
for shapes that translate **and spin**. Each side moves with a constant linear velocity
and a constant angular velocity about a centre, for any number of turns, over `duration`
seconds. The sweep is conservative advancement along that real path
(`regia_physics::ccd::time_of_impact_motion`, ADR-2423). It cannot tunnel, and a
fast-spinning bar is caught even when its start and end poses look identical.

Read-only. Literal shapes need no physics world. A side given as an `entity` reads the
entity's `Collider` shape, its world pose and, unless `motion` overrides it, its
`Velocity`.

Use it to check whether a projectile, a thrown prop or a spinning blade reaches a wall
before the next step, to verify a `SweptCcd` body's impact time, or to probe placement
for a moving object.

## Call

```json
{ "a": { "shape": { "kind": "box", "half_extents": [1.0, 0.02, 0.02] } },
  "b": { "shape": { "kind": "box", "half_extents": [2.0, 0.01, 2.0] }, "position": [0, 0.5, 0] },
  "motion": { "duration": 0.1, "a": { "angular": [0, 0, 188.5] } } }
→ { "status": "hit", "t": 0.0261, "time": 0.00261,
    "normal": [0.0, 1.0, 0.0], "point": [0.872, 0.490, 0.0],
    "pose_a": { "position": [0, 0, 0], "rotation": [0, 0, 0.2435, 0.9699] },
    "pose_b": { "position": [0, 0.5, 0], "rotation": [0, 0, 0, 1] } }
```

(Numbers rounded. `point` lies on the bar's leading top edge; its `z` can be anywhere
along that edge.) The 2 m bar spins three whole turns in 0.1 s. Its endpoints match, so
an endpoint interpolation would see no motion at all. The sweep finds the hit after
0.49 rad, when the bar's corner reaches the slab's lower face at y = 0.49.

- **`a`, `b`** (required): `{ shape, position?, rotation? }` or `{ entity }`.
  - `shape.kind`: `point`, `sphere { radius }`, `box { half_extents }`,
    `capsule { half_height, radius, axis? }`, `cylinder { half_height, radius, axis? }`,
    `cone { half_height, radius, axis? }` (apex at `+axis`), `segment { a, b }`,
    `hull { points: [[x, y, z], ...] }` (at most 1024 points). `axis` is `x`, `y`
    (default) or `z`.
  - `position` (default origin) and `rotation` (`[x, y, z, w]`, default identity) are the
    pose at the start.
  - `entity`: an entity id with a convex `Collider` (not a trimesh, heightfield or
    compound) and a `GlobalTransform`. Its collider offset is applied.
- **`motion`** (optional): `{ duration?, a?, b? }`. `duration` is the swept time in seconds
  (default 1). `a` and `b` are `{ linear?, angular?, center? }`: `linear` in m/s,
  `angular` in rad/s (world axis times speed), `center` the rotation centre in the
  shape's local frame (default its origin). A side without an entry is at rest, or moves
  with its entity's `Velocity`.
- **`target_gap`** (default 0): the separation, in metres, at which the sweep stops.
- **`tolerance`** (default 0.0001): the accepted band above `target_gap`, in metres.
- **`max_iterations`** (default 128, 1-100000): the conservative-advance budget.

## Result

| `status` | Meaning | Fields |
|---|---|---|
| `hit` | The shapes first reach `target_gap` (within `tolerance`) at `t` | `t` (0-1 of the interval), `time` (`t * duration`, s), `normal` (unit, from A toward B), `point` (witness point on A), `pose_a`, `pose_b` (poses at `t`) |
| `separated` | They stay further apart than `target_gap` for the whole interval | none |
| `overlapped_at_start` | They already interpenetrate at the start | `t: 0`, `time: 0` |
| `failed` | The sweep refused or could not answer | `reason` (below) |

Touching at the start, or being within `target_gap` there, is a `hit` at `t = 0`.

## Failure reasons

The query never loops and never guesses. A refusal comes back as
`{ "status": "failed", "reason": "<code>" }`:

| `reason` | Cause | Fix |
|---|---|---|
| `non_finite_input` | A number is NaN or infinite. JSON numbers past f32 range (about 3.4e38) become infinite. | Send finite values. |
| `invalid_shape` | A negative size, or an empty hull. | Fix the shape. |
| `invalid_pose` | A zero quaternion rotation. | Send a unit quaternion. |
| `invalid_parameter` | Negative `target_gap`, `tolerance` or `duration`. | Use values of 0 or more. |
| `degenerate_motion` | The closing-speed bound is not finite: velocities whose difference overflows f32, or a spin times a radius that overflows. Or it is so large that an advance no longer moves `t` at f32 resolution. | Shorten `duration` or split the interval. |
| `max_iterations` | The budget ran out, usually a grazing near-miss with a tiny `tolerance`. | Raise `max_iterations` or `tolerance`. |
| `numerical_failure` | The distance iteration did not converge. | Report it, with the call, as a bug. |

Malformed parameters (a missing side, an unknown `kind`, a field that is not a number)
are `F0002` errors. An `entity` that does not exist, or has no `Collider` or
`GlobalTransform`, is `F0003`.

## Notes

- Deterministic: the same call gives the same answer, bit for bit, on one platform.
- The motion is constant over the interval. Gravity and other forces are not applied;
  split a long interval into steps for a ballistic arc.
- `SweptCcd` bodies run the same sweep inside the physics step. See
  [Continuous collision detection](../systems/physics-ccd.md).
