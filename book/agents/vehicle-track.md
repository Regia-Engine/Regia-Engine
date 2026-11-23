# Skid-steer tracks

`physics.track_step` and `physics.track_state` drive a planar skid-steer body
(`regia_physics::vehicle_track`, ADR-4629). Two tracks share one rigid pose on the XZ
plane. The body is separate from the wheeled vehicle: a call carries its own pose and
does not read or write a physics world.

Since ADR-8551 the step is the lockstep tracked model `regia_detsim::ground_vehicle::tracked`
with these constants (`TrackedParams::DEFAULT`) on a flat floor: the same bits on every target.
Speeds and yaw are the same arithmetic as before; X and Z take the heading's sine and cosine from
`regia_detsim::math` instead of a quaternion rotation, so they can differ from an older build in
the last bits. Slopes, other numbers, wheeled vehicles and a 3D pose:
[Ground vehicles (lockstep)](det-ground-vehicles.md).

Forward is local −Z, right is +X, up is +Y. Yaw is a right-handed turn about +Y.
Positive yaw swings the nose (local −Z) toward −X, a left turn.

## The model

| Constant | Value | Role |
|---|---|---|
| `separation_m` | 2 | Distance between the track centerlines, meters |
| `max_speed_m_s` | 8 | Track speed at throttle +1, meters per second |
| `friction_accel_m_s2` | 4 | Largest speed change of a driven track, meters per second squared |
| `coast_drag_m_s2` | 1 | Coast toward zero when the throttle is 0, meters per second squared |

A throttle is a finite number. Values outside −1 to 1 are clamped. Throttle `t` requests
the speed `t * max_speed_m_s`. While the throttle is not zero, one step of length `dt`
changes that track's speed by at most `friction_accel_m_s2 * dt`.

A throttle of zero, including −0, coasts. The speed moves toward zero by
`coast_drag_m_s2 * dt`, and by no more than the friction cap. It stops at zero. Drag
alone does not reverse a track. With the shipped numbers the coast drag is the tighter cap.

The center (longitudinal) speed is the average of the two track speeds. The yaw rate, in
radians per second about +Y, divides their difference by the track separation:

```text
longitudinal_speed = (left_speed + right_speed) / 2
yaw_rate           = (right_speed - left_speed) / separation_m
```

The left track faster than the right makes `yaw_rate` negative, and the nose swings
toward +X, a right turn.

Each step updates the speeds first, then moves the pose with the average of the speeds
at the start and the end of the step. From rest, while both accelerations stay constant
and neither track has reached `max_speed_m_s`, the yaw after `T` seconds is

```text
yaw(T) = (accel_right - accel_left) * T * T / (2 * separation_m)
```

A full left throttle and a zero right throttle accelerate the left track at 4 m/s² and
leave the right track at rest, so one second of that pair is −1 rad.

The result's `digest` is 16 lowercase hex digits. The same inputs produce the same digest.

## `physics.track_step`

Read-only. Applies `steps` integrator steps and returns the pose at the end.

```json
{ "dt": 0.015625, "steps": 128, "left_throttle": 1.0, "right_throttle": 1.0 }
```

- **`dt`** (required): seconds. The parameter gate accepts `1e-6` to `1`.
- **`steps`** (default 1): how many steps this call applies, an integer from 1 to 10000.
  The step counter in the result counts only this call. It starts at 0.
- **`left_throttle`**, **`right_throttle`** (default 0): see above.
- **`left_speed`**, **`right_speed`** (default 0): track speeds at the start, meters per second.
- **`yaw`** (default 0): heading about +Y, radians.
- **`x`**, **`z`** (default 0): world position, meters. Y stays 0.

To continue a run, pass the speeds, `yaw`, `x` and `z` from the previous result, and set
`steps` to how many new steps you want. The result's `position` array is the readout;
the next call reads `x` and `z`. Sending the whole result back reuses its `steps` field
as the next count.

## `physics.track_state`

Read-only. Reports the pose it was given. It does not move the body, and `steps` in the
result is 0. Throttles are ignored. The parameters are `left_speed`, `right_speed`,
`yaw`, `x` and `z`, with the same defaults as `physics.track_step`. A `physics.track_state` call:

```json
{ "left_speed": 4.0, "right_speed": 0.0 }
```

The center speed is 2 m/s and the yaw rate is −2 rad/s.

## Result

Both methods return:

```json
{
  "left_speed": 0.0,
  "right_speed": 0.0,
  "longitudinal_speed": 0.0,
  "yaw_rate": 0.0,
  "yaw": 0.0,
  "position": [0.0, 0.0, 0.0],
  "forward": [0.0, 0.0, -1.0],
  "steps": 0,
  "digest": "0000000000000000",
  "model": {
    "separation_m": 2.0,
    "max_speed_m_s": 8.0,
    "friction_accel_m_s2": 4.0,
    "coast_drag_m_s2": 1.0
  }
}
```

`yaw_rate` is the rate at the reported speeds, `(right_speed - left_speed) / separation_m`.
`forward` is the nose after `yaw`, `(-sin(yaw), 0, -cos(yaw))` with the same lockstep sine and cosine
the step integrates (ADR-8638; before it, a quaternion rotation one ulp off on some yaws). `model`
repeats the constants above. The sample digest
is the shape only; a real call fills it from the pose.

## Errors

| Code | When |
|---|---|
| `track_bad_input` | A throttle, speed or pose is not finite, or `dt` is not positive. A JSON number that does not fit in an f32 (past about 3.4e38, so `1e308`) is not finite. The Rust API also refuses NaN. |
| `F0002` | `dt` is missing, a value has the wrong JSON type, `dt` is outside 1e-6 to 1, or `steps` is outside 1 to 10000. |

`track_bad_input` uses JSON-RPC `-32602`. `errors.explain` with `{ "code": "track_bad_input" }` returns the long form.

## Limits

The model is planar. It has no suspension, no terrain and no collision. The friction cap
is a limit on the speed change, not a mass or a contact solver. A throttle outside −1 to 1
is clamped.
