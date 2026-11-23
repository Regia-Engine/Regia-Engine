# First-person walk bob

A first-person camera can rise, fall and sway with the player's steps. The bob is **off by default**. It
applies to a `CameraMode::FirstPerson` rig when the camera carries a `regia_framework::camera::walk_bob::WalkBob`
component, or when the mode's `bob` amplitude is above zero (the default settings with that vertical amplitude).
ADR-2486.

| Method | Permission | What it does |
|---|---|---|
| `camera.walk_bob` | read | List the rigs with where their bob comes from, its settings and the gait state. |
| `camera.set_walk_bob` | edit_scene | Turn a first-person rig's bob on, change it, or remove it. |

## How it moves

- **Stride matches speed.** One stride (a left and a right step) is `2.3 · h · Fr^0.3` metres, where `h` is
  `leg_length_m` and `Fr = v² / (g · h)` is the Froude number. For a 0.9 m leg this is about 1.3 m at 1.4 m/s
  (an ordinary walk, about 125 steps a minute) and longer when running. The gait counts as running above
  `Fr = 0.5` (about 2.1 m/s for a 0.9 m leg).
- **Steps come from distance, not time.** The stride phase moves on by the ground walked divided by the stride.
  At a steady speed a footfall lands every half stride of ground, at any frame rate. A bob that runs on a
  fixed timer makes each step cover a different length of ground when the speed changes, so the steps seem
  to slide.
- **Walk and run shapes differ.** In a walk the eye is lowest at each footfall and highest over the stance
  foot. In a run it is lowest at mid-stance. The eye sways toward the stance foot once per stride. Amplitudes
  scale with speed over `reference_speed` (at most `max_gain` times), so the bob fades out when the player
  stops.
- **Gaze stabilization.** With `fixation_m` above zero, the camera pitches against its own vertical motion so
  that a point that far ahead stays in place on screen. The distant forest does not jiggle; only the near
  ground moves with the steps.
- **Slopes and jumps.** Motion that climbs up to `max_slope_deg` counts along its length. Steeper motion
  (a lift, a fall) counts only its horizontal part. A move longer than `teleport_m` in one frame restarts the
  gait.

The bob is added after the rig writes the eye pose and before shakes, so shakes stack on top. A full-body
first-person view, where the eye rides the head joint of an animated body, already bobs with the animation
and should not carry a `WalkBob`.

## `camera.walk_bob`

```sh
regia agent call camera.walk_bob '{}' --project <project>
regia agent call camera.walk_bob '{"entity": 4294967300}' --project <project>
```

Answer (list): `{ count, truncated, cameras: [row] }`. With `entity`, the answer is `{ camera: row }`. Each row:

- `entity`, `first_person` (whether the rig is in first-person mode).
- `source`: one of `component`, `mode_amplitude` or `off`.
- `settings`: `{ vertical_m, lateral_m, roll_deg, reference_speed, max_gain, leg_length_m, fixation_m, smoothing, max_slope_deg, teleport_m }`, or `null` when the bob is off.
- `state`: `{ phase, speed, stride_m, cadence_spm, steps, stance_foot, running, offset, pitch_deg, roll_deg }`, or `null` before the first bobbed frame.
  - `phase` is in `[0, 1)`: 0 is a left footfall and 0.5 a right one.
  - `steps` counts the footfalls since the gait started.
  - `offset`, `pitch_deg` and `roll_deg` are what was added this frame.

## `camera.set_walk_bob`

```sh
regia agent call camera.set_walk_bob '{"entity": 4294967300}' --project <project>                      # defaults
regia agent call camera.set_walk_bob '{"entity": 4294967300, "vertical_m": 0.02, "fixation_m": 0}' --project <project>
regia agent call camera.set_walk_bob '{"entity": 4294967300, "remove": true}' --project <project>
```

The call starts from the bob the rig has now (or the defaults) and changes the fields given. It answers
`{ removed, camera: row }`.

| Field | Default | Range | Meaning |
|---|---|---|---|
| `vertical_m` | 0.012 | 0..=0.2 | Vertical amplitude at the reference speed (half the peak-to-peak travel). |
| `lateral_m` | 0.006 | 0..=0.2 | Sideways sway amplitude. |
| `roll_deg` | 0 | 0..=10 | Roll amplitude. Off by default, for comfort. |
| `reference_speed` | 1.4 | 0.1..=20 | The speed the amplitudes are given for, in m/s. |
| `max_gain` | 1.6 | 0..=10 | Largest amplitude gain at high speed. |
| `leg_length_m` | 0.9 | 0.2..=3 | Leg length. Sets the stride and the walk-run speed. |
| `fixation_m` | 12 | 0..=1000 | Gaze stabilization distance (0: off). |
| `smoothing` | 10 | 0..=100 | Speed smoothing rate, 1/s. |
| `max_slope_deg` | 50 | 0..=90 | Steepest motion counted along its length. |
| `teleport_m` | 3 | 0.01..=1000 | A one-frame move longer than this restarts the gait. |
| `remove` | false | | Take the component off. A mode `bob` above zero still applies. |

The defaults are smaller than a real head's travel (about 4 cm peak to peak at a brisk walk). A screen does
not get the eye's own stabilization, so full-size bob is tiring to watch.

## Errors

- `UNSUPPORTED` (`F0016`): the app has no `CameraPlugin`.
- `F0003` (not found): the entity does not exist.
- `invalid_params` (`F0002`): a missing `entity` or a value of the wrong type. Categorical refusals also carry
  `data.reason`:
  - `walk_bob_not_first_person`: the entity is not a camera rig in first-person mode.
  - `walk_bob_out_of_range`: a field is outside its range. Nothing is changed.
