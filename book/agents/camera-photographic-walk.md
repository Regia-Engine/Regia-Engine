# Camera photographic walk (agents)

A walk through a forest should not smear the trunks next to the camera. `PhotographicWalk`
(`regia_framework::camera::photographic`, ADR-2449) drives a camera's `PhysicalCamera`. It is opt-in and
`CameraPlugin` installs the system.

- **Motion blur** is a short shutter (`shutter_fraction`, 0.12 of a frame, about a 43 degree shutter)
  scaled by how fast the camera moves. No blur below `blur_speed_min_m_s` (3 m/s) or
  `blur_turn_min_deg_s` (90 degrees per second), the full short shutter above `blur_speed_full_m_s`
  (8 m/s) or `blur_turn_full_deg_s` (270), a smoothstep between. Walking (1.4 m/s) and a slow turn do not
  blur; a sprint or a fast pan does.
- **Depth of field** is off, except during a focus pull and for `focus_hold_s` (0.6 s) after it. The focal
  distance eases from where it was to the target with a smoothstep, then the effect switches off.

The decision depends only on the camera's motion between frames and the pull state, so a replay gives
the same lens frame by frame. The speed is read from the camera's final pose, so a hard shake or handheld
roll counts as motion.

| Method | Permission | What it does |
|---|---|---|
| `camera.photographic_walk` | edit_scene | No params: list. `entity`: read. `enable` or settings: add or edit. `pull_focus`: start a pull. `remove`: take it off. |

Params: `entity`, `enable`, `shutter_fraction` (0..1), `blur_speed_min_m_s`, `blur_speed_full_m_s`,
`blur_turn_min_deg_s`, `blur_turn_full_deg_s`, `focus_hold_s`, `aperture_f_stops` (at least 0.7),
`pull_focus` (`{ distance_m, duration_s }`), `remove`.

```json
{ "entity": 4294967300, "enable": true, "pull_focus": { "distance_m": 3.0, "duration_s": 0.8 } }
→ { "entity": 4294967300, "settings": { "shutter_fraction": 0.12, ... },
    "lens": { "shutter_fraction": 0.0, "focus_m": null }, "focusing": false,
    "pull": { "from_m": 10.0, "to_m": 3.0, "duration_s": 0.8, "elapsed_s": 0.0 } }
```

`UNSUPPORTED` (`F0016`) without `CameraPlugin`; `F0003` for an unknown entity or a camera with no preset.
`F0002` with `data.reason`:

- `walk_bad_focus_pull`: `distance_m` not positive, or `duration_s` negative. A distance above 10 000 m is
  clamped.
- `walk_range_inverted`: a `full` threshold below its `min`.
- `walk_remove_with_fields`: `remove` together with other fields.
- `walk_not_a_camera`: the entity has no `Transform`.

`entity` is required for edits; without it the call lists up to 256 cameras. `enable` and `remove` are
booleans; numeric settings are finite non-negative numbers, `shutter_fraction` is at most 1,
`aperture_f_stops` at least 0.7, and focus pull distance is positive (clamped at 10,000 m) with
non-negative duration (below 1 ms cuts immediately). Defaults are shutter 0.12, speed thresholds
3/8 m/s, turn thresholds 90/270 degrees/s, focus hold 0.6 s, aperture f/2.8 and focal distance 10 m.
Inside `pull_focus`, `distance_m` defaults to 0 (so supply a positive value) and `duration_s` to 1.
Workflow: read the camera profile, enable the driver, apply a focus pull, step fixed frames, then
inspect the lens state.

Captured failing call (lavapipe; this editor does not install `CameraPlugin`):

```console
$ regia agent call camera.photographic_walk '{"entity":4294967295,"enable":true}' --project samples/arena
error: method call failed: no photographic walk in this app (-32004): {"code":"F0016","hint":"add regia_framework::camera::CameraPlugin"}
```
