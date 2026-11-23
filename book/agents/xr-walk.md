# XR walk mode

Walk mode moves a headset user through a scene, seated or standing: smooth or teleport locomotion, snap or
smooth turning, the motion vignette, a floor that follows the ground, height calibration and a render
budget for the headset's refresh rate (ADR-2457). It sits on the [XR plugin](xr.md) (`regia_xr`, spec 58)
and is opt-in: add `regia_xr::XrWalkPlugin` after `XrPlugin`, or run the showcase with `--xr`.

The head pose always comes from the headset. The walk moves the rig (the tracking-space origin) and never
adds a bob, a sway or any camera noise; nothing that moves a desktop camera reaches the eyes.

| Method | Permission | What it does |
|---|---|---|
| `xr.walk_start` | play | Apply a comfort preset and the render budget, put the head over a ground point facing a heading, and turn locomotion on. On the simulator it also takes the session to `Focused`. |
| `xr.walk_set` | play | Change locomotion, speed, teleport range, ground follow or the preset; turn the walk on or off; calibrate the height. |
| `xr.get` | read | Gains a `walk` object (below) when the walk plugin is installed. |

Without the XR plugin both methods answer `{ "refused": "xr_not_installed", "installed": false }`; with XR
but without the walk plugin, `{ "refused": "xr_walk_not_installed", "installed": true }`.

## Starting a walk

```json
{ "position": [60, -170], "yaw": 0.7, "preset": "comfortable" }
→ { "active": true, "locomotion": "teleport", "preset": "comfortable", "app_state": "Focused",
    "budget": { "refresh_hz": 90, "frame_ms": 11.11, "render_scale": 0.8, "foveation": "high" },
    "head_world": [60, 47.7, -170], ... }
```

| Parameter | Default | Meaning |
|---|---|---|
| `position` | the rig stays | `[x, y, z]` or `[x, z]`: the head stands over this point. The height comes from the ground (`y` is used only where there is none). |
| `yaw` | the rig's | Heading in radians: 0 looks down −Z, a positive value turns left. |
| `preset` | none | `comfortable`, `moderate` or `free` (below). |
| `budget` | `true` | Apply the walk budget for the session's refresh rate (render scale, foveation). |
| `focus` | `true` | On the simulator, drive the session to `Focused` first. A real runtime focuses when the user puts the headset on. |

## Locomotion and comfort

| Mode | The `move` stick (left hand; the right with a left-dominant hand) |
|---|---|
| `smooth` | Glides along the head's heading at `move_speed_mps` (1.4 m/s, a walking pace) at full deflection; a 0.15 dead zone, rescaled above it. |
| `teleport` | Push forward past 0.7 to aim: an arc leaves the moving hand (the head when the hand is not tracked) and lands within `teleport_range_m` (8 m on flat ground). Release below 0.3 to jump: the head lands over the arc's end. An arc that strikes a wall face, or finds no ground, gives no target. While a target is aimed, a flat 0.5 m marker lies on the ground there (a world-locked panel, id 4294967294, reserved: do not use it with `xr.set_quad`; it needs `XrRenderPlugin`, and `xr.layers` lists it). A jump never raises the vignette. |
| `off` | Nothing: room-scale walking only. |

Turning stays on the `turn` stick ([XR plugin](xr.md): `snap_turn_deg`, 0 for smooth). The vignette rises
with artificial motion only, never with the user's own head.

| Preset | Locomotion | Turning | Vignette |
|---|---|---|---|
| `comfortable` | teleport | 30-degree snaps | 0.7 |
| `moderate` | smooth | 30-degree snaps | 0.5 |
| `free` | smooth | smooth | off |

## Ground and height

With an `XrGround` height function (the showcase installs its heightfield) the rig's floor follows the ground
under the head every fixed step, so walking up a slope keeps the eye at the same height above the terrain.
Off the ground (outside the map) the rig keeps its height and `ground_misses` counts the steps.

`xr.walk_set { "calibrate_height": true }` makes the head's current height the comfort eye height
(`eye_height_m`, 1.65 m by default); a number (0.5 to 2.5 m) names another. A seated user then walks at
standing height. The lift (−1 to 1.5 m) is reported as `lift_m`. It needs a running session
(`xr_not_running`).

## Changing the walk: `xr.walk_set`

```json
{ "locomotion": "smooth", "move_speed_mps": 2.0, "calibrate_height": 1.7 }
→ { "locomotion": "smooth", "move_speed_mps": 2.0, "lift_m": 0.45, ... }
```

| Key | Value |
|---|---|
| `active` | bool: locomotion on or off |
| `locomotion` | `smooth`, `teleport`, `off` |
| `move_speed_mps` | 0.1 to 6 |
| `teleport_range_m` | 1 to 30 |
| `ground_follow` | bool |
| `preset` | `comfortable`, `moderate`, `free` (sets locomotion, snap turn and vignette) |
| `calibrate_height` | `true` or an eye height in meters |

Everything is checked first: a bad value is `F0002` with `data.reason` `xr_invalid_value` naming the key,
and nothing changes. `locomotion` given with `preset` wins over the preset's.

## `xr.get.walk`

| Field | Meaning |
|---|---|
| `active`, `locomotion`, `move_speed_mps`, `teleport_range_m`, `ground_follow`, `preset` | the settings |
| `ground`, `ground_m`, `ground_misses` | whether a ground is installed, its height under the head at the last step, steps off it |
| `lift_m` | the height calibration |
| `budget` | `{ refresh_hz, frame_ms, render_scale, foveation }` applied last, or null |
| `aiming`, `aim_target` | a teleport being aimed and where its arc lands (null when it finds no ground) |
| `teleports`, `jumps`, `distance_m`, `steps` | teleports made; instant rig moves (teleports, the first ground placement, each `xr.walk_start`: velocity-from-motion consumers such as the showcase's listener treat them as cuts); horizontal meters moved by locomotion; fixed steps run while active |
| `head_world` | the head's world position (rig, offset and head pose) |
| `app_state` | the XR app state |

Poll `xr.get` and compare these fields; in a test app, step a fixed number of updates and assert on them.

## The render budget

`walk_budget(refresh_hz)` picks the eye-image scale and foveation: 0.7 and `high` at 110 Hz and up, 0.8 and
`high` at 85 Hz and up (90 Hz headsets), 0.9 and `medium` below. On top of it the XR quality profile already
turns off screen-space GI, probe updates, volumetric fog and clouds, reflections and the other effects the
eyes could disagree on ([XR plugin](xr.md)). The showcase adds a coarser scatter mesh and LOD switching at
the eye's size. Foveation is asked of the runtime and reported; the simulator applies none, and a runtime
without it ignores it.

## A typical check (no headset)

Run the showcase on the simulator (with `--xr` the headless clock runs, so the walk moves on every update),
start the walk, push the stick, read the walk:

```sh
cargo run -p showcase -- samples/showcase/example_scene --xr simulator --project-dir /tmp/walk
```

```json
xr.walk_start { "preset": "moderate" }
xr.drive { "action": "pose", "head": { "position": [0, 1.7, 0], "yaw": 0 } }
xr.drive { "action": "input", "name": "move", "sub": "left", "value": [0, 1] }
xr.get (repeat until walk.distance_m > 2) → walk.head_world[1] - walk.ground_m = 1.7
xr.capture_eye { "eye": "both" }
```

`xr.capture_eye` shows both eyes (the walk's render scale sets their size); in a window (`--window --xr
simulator`) the desktop mirror shows the left eye.

## In Rust

<!-- ignore-reason: needs the `xr` feature, an XR config and the game's own terrain (`my_terrain`) -->
```rust,ignore
app.add_plugins(XrPlugin::new(config))
    .add_plugins(XrWalkPlugin::default())
    .insert_resource(XrGround(Arc::new(|x, z| Some(my_terrain.height(x, z)))));
// later, with an XrRig entity in the world:
regia_xr::walk_start(world, WalkStart { position: Some(start), yaw: Some(0.0),
    preset: Some(ComfortPreset::Moderate), budget: true })?;
```

Systems that stream or cull around the primary camera (the streamed scatter, mesh LODs, the terrain) do not see the
eyes: keep the desktop camera on the head while walking with a field of view that holds both eyes (the showcase's
`camera_follows_head` and `covering_fov_y` do this).

Put an `AudioListener` on the `XrHead` entity (a child of the `XrRig`), or on a child of it, so spatial audio is
heard from the head pose. The mixer derives the listener's Doppler velocity from its motion, so a teleport would
sound like a pass at hundreds of meters per second: the showcase keeps the listener on an `XrEars` child of the
head and replaces that entity whenever `XrWalk::jumps` changes (a new entity starts at rest), and removes any
other listener while the walk runs.

## Error codes

| Reason | Meaning |
|---|---|
| `xr_invalid_value` | A value of the wrong type or out of range (the message names the key), or no single `XrRig` entity (`rig`). |
| `xr_not_running` | `calibrate_height` without a running session. |

On a real runtime `xr.walk_start` never drives the session (no `xr_simulator_only`): the walk starts and moves once the runtime focuses.
