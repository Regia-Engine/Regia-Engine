# Camera handheld (agents)

`regia_framework::camera::handheld::Handheld` gives a first-person camera head sway, breathing and a walk
bob (ADR-2449). It is opt-in: a camera without the component is untouched. `CameraPlugin` installs the
systems.

- **Sway**: smooth, correlated noise on position (about 6 mm) and rotation (about 0.35 degrees), two
  octaves. The lattice values are a pure function of `seed`, so a replay from the same seed and the same
  time steps gives the same poses.
- **Breathing**: a slow rise and a small pitch at 0.22 Hz.
- **Walk bob**: the step phase is the horizontal distance the camera travelled divided by `stride_m`.
  The head is lowest at each footfall and sways and rolls over two steps. It fades in between
  `walk_speed_min` (0.3 m/s) and `walk_speed_full` (1.5 m/s), so standing still does not bob, a faster
  walk steps faster, and a teleport (a jump above 5 m in one frame) is not walking.
- **`intensity`** scales every term (0 off, clamped to 4). **`steady`** is a stabilized mount: sway and bob
  at 20 percent and no roll.

The pose is added after the rig or the shot player moves the camera and before the shakes, and taken off
again at the start of the next frame, so it never builds up in the pose a rig eases from.
`Handheld::footfalls()` counts steps; audio can follow it.

| Method | Permission | What it does |
|---|---|---|
| `camera.handheld` | edit_scene | No params: list the cameras that have one. `entity`: read it. `entity` plus `preset` or profile fields: add or edit. `remove`: take it off. |

Params: `entity`, `preset` (`walking` or `steady`, replaces the profile), `intensity`, `steady`, `seed`,
`stride_m` (at least 0.2), `sway_position_m`, `sway_rotation_deg`, `bob_vertical_m`, `remove`.

```json
{ "entity": 4294967300, "preset": "walking", "intensity": 0.8, "seed": 7 }
→ { "entity": 4294967300, "before": null, "after": { "profile": { "intensity": 0.8, "steady": false, ... },
    "footfalls": 0, "step_phase": 0.0, "pose": { "offset_m": [0, 0, 0], "pitch_deg": 0, ... } } }
```

Answers `UNSUPPORTED` (`F0016`) without `CameraPlugin`, `F0003` for an unknown entity or a camera with no
handheld to read. `F0002` with `data.reason`:

- `handheld_unknown_preset`: `preset` is not `walking` or `steady`.
- `handheld_remove_with_fields`: `remove` together with profile fields.
- `handheld_not_a_camera`: the entity has no `Transform`.

A negative number, or a `stride_m` below 0.2, is a plain `F0002`.

`entity` is required for edits; without it the call lists up to 256 profiles. `remove` and `steady`
are booleans; `seed` is an unsigned integer; other profile fields are finite non-negative numbers,
with `stride_m` at least 0.2 and `intensity` clamped to 4. Defaults are intensity 1, steady false,
seed `0x4e414d45`, stride 0.78 m, sway 0.006 m / 0.35 degrees and bob 0.025 m; remaining breath
and walk thresholds are 0.22 Hz, 0.004 m, 0.15 degrees, 0.3 m/s and 1.5 m/s. Workflow: add the
`walking` preset with a fixed seed, step the same frame sequence, inspect `footfalls` and `pose`, then
remove the profile.

Captured failing call (lavapipe; this editor does not install `CameraPlugin`):

```console
$ regia agent call camera.handheld '{"entity":4294967295,"preset":"walking"}' --project samples/arena
error: method call failed: no handheld cameras in this app (-32004): {"code":"F0016","hint":"add regia_framework::camera::CameraPlugin"}
```
