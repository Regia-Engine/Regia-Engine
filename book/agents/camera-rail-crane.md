# Camera rail and crane

A camera asset (`.camera.ron`, format v2) can ride a spline like a dolly on a track and hang from a crane arm
(ADR-3231, spec 35 § 17). Both are node kinds in a rig; `CameraPlugin` installs everything.

| Method | Permission | What it does |
|---|---|---|
| `camera.rail_state` | read | What each `Rail` node did in the last evaluation, per camera driver (or one with `entity`). |
| `camera.crane_state` | read | What each `Crane` node did: base, requested and clamped axes, which axes were clamped, the arm tip. |
| `camera.set_rail` | edit_scene | Sets the `rail_position` variable (meters). |
| `camera.set_crane` | edit_scene | Sets `crane_yaw`, `crane_pitch`, `crane_length`, `crane_mount_yaw`, `crane_mount_pitch` (any subset). |

All four answer `UNSUPPORTED` (`F0016`) in an app without `CameraPlugin`.

A driver named explicitly with `entity` whose asset is not bound answers `{"entity": …, "bound": false}` on
`camera.rail_state` and `camera.crane_state` (like `camera.inspect`), not an empty success; a list call skips
unbound drivers.

## The Rail node

```ron
Rail(spline: "track", drive: Value(Var("rail_position")), orient: Frame, offset: (0.0, 0.5, 0.0))
```

`spline` is the `Name` of an entity with a `Spline` component. The camera sits at a distance in meters along the curve.
The distance is arc length, so advancing it by `v · dt` each step moves at `v` m/s on any curve.

| Drive | Meaning |
|---|---|
| `Value(v)` | The distance is the value (literal, `Param` or `Var`). |
| `Time(speed: …, start: …)` | `start + speed · elapsed` (meters per second; negative rides backwards; `start` defaults to 0). |
| `Follow(target: …, lead: …, damping: …)` | The closest point of the target on the rail, plus `lead` meters ahead, eased with rate `damping` (1/s; 0 snaps). Never passes the goal or an end. |

An open spline clamps at its ends; a closed spline loops. `orient` is `Frame` (along the rail), `LookAt(target)` or
`Free` (keep the earlier rotation). A rail shorter than 1 mm, or a missing spline entity, skips the node.

## The Crane node

```ron
Crane(base: Rail(spline: "track", position: Var("rail_position"), heading: true),
      yaw: Var("crane_yaw"), pitch: Var("crane_pitch"), length: Var("crane_length"),
      mount_yaw: Literal(180.0),
      limits: (yaw_deg: Some((-90.0, 90.0)), length: Some((1.0, 8.0))))
```

The arm leaves the base (`Ground((x, y, z))` or a point on a rail) along yaw (0 faces −Z, positive turns toward −X)
and pitch (positive up) for `length` meters; the camera is at the tip. The mount adds `mount_yaw` and `mount_pitch`
(degrees) to the arm angles. `limits` clamp each axis; pitch is always within ±89.9° and the length is never negative.
**The arm does not collide with anything.**

## Keying from a sequence

The properties `camera.rail.position` (meters) and `camera.crane.yaw`, `.pitch`, `.length`, `.mount_yaw`,
`.mount_pitch` write the Float driver variables `rail_position`, `crane_yaw`, `crane_pitch`, `crane_length`,
`crane_mount_yaw`, `crane_mount_pitch`. Declare the ones you key under `variables` in the asset:

```ron
variables: [(name: "rail_position", default: Float(0.0))]
```

Bind the track's target to the camera entity (the one with the `CameraDriver`). An undeclared variable makes the
track report an apply error.

## Error reasons

`data.reason` on `camera.set_rail` and `camera.set_crane`:

| Reason | Meaning |
|---|---|
| `rail_var_not_declared` | The asset has no Float variable of that name; nothing was written. |
| `camera_no_driver` | The entity has no `CameraDriver`. |
| `camera_asset_invalid` | The driver's asset is not loaded or has errors. |

A missing entity is `F0003`; a bad number is `F0002`.

## Limits

The spline's world transform is the last propagated `GlobalTransform` (one frame behind a moving spline). The curve
is rebuilt every frame for each driver that has a rail node.
