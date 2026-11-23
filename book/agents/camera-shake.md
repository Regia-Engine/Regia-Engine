# Camera shake sensors

`regia_framework::camera::sensor` separates shake sources from the cameras that feel them
(ADR-1619). A `ShakeSensor` on a camera decides which shakes reach it and how they look; sources are
`ShakeEmit` messages, the `CameraDirector` and `ContactShake` physics bodies. `CameraPlugin`
installs everything; nothing else is needed.

| Method | Permission | What it does |
|---|---|---|
| `camera.shake` | edit_scene | Send a shake (a noise preset, the blast law or the ground shock) to the listening cameras; reports who will feel what and when. |
| `camera.shake_sensor` | read | Every camera's sensor, pending and running shakes and the pose added this frame (or one camera with `entity`). |
| `camera.set_shake_sensor` | edit_scene | Give a camera its own sensor, change it, or remove it. |

All three answer `UNSUPPORTED` (`F0016`) in an app without `CameraPlugin`.

## Who shakes

A camera feels shakes through its sensor:

- its own `ShakeSensor`, when it has one;
- otherwise a `CameraRig` camera, and the camera the `ShotPlayer` drives, feel `ShakeSensor::default()`
  (channel 1, gain 1, `add`, `world`, `gameplay`), so projects from before sensors shake as they did;
- otherwise it does not shake. A plain `Camera` (a UI camera, a picture-in-picture view, a drone feed)
  never shakes until it is given a sensor.

`ShakeSensor::off()` (channels 0) keeps a rig or the shot camera steady.

| Field | Meaning |
|---|---|
| `channels` | Bitmask. The camera feels an emit when `channels & emit.channels != 0`. The director's shakes are on channel 1. |
| `gain` | Multiplies every offset and angle; clamped into 0..=100, NaN reads as 0. |
| `combine` | `add`: all shakes sum (the director's old behavior). `max`: only the strongest plays, by its envelope now (metres of amplitude, or the larger of radians and metres for a mount oscillation; ties go to the shake that started first). |
| `axes` | `world`: the offset is in world axes. `camera`: x right, y up, z back in the camera's own axes. Rotations are always about the camera's axes. |
| `clock` | `gameplay`: pause and time scale apply. `unscaled`: the shake keeps playing while the game is paused (a pause-menu or photo-mode camera). Both are the engine's per-frame deltas, never a wall-clock read. |

## Emits

`ShakeEmit { origin, channels, preset, falloff_radius_m, travel_speed_m_s }`:

- `preset`: `Noise(Shake)`, `Blast(ShockShake)` (amplitude by distance, arrival at the shock speed:
  the same law and numbers as `camera.schedule_shock_shake`) or `Ground(GroundShock)` (the seismic
  wave and the airblast as two mount oscillations, each on its own arrival).
- `origin`: without one, every listening camera feels it at once and in full.
- `falloff_radius_m`: the strength falls as `(1 - d / r)^2` and is zero at the radius and beyond.
  Infinite (the default) means no falloff; the blast and ground laws still fall with distance.
- `travel_speed_m_s`: the shake arrives `distance / speed` seconds later. It replaces the blast law's
  343 m/s; the ground preset keeps its own wave speeds.

Each listening camera measures its own distance from its unshaken pose. The delay is counted in fixed
steps (`ShakeField`), rounded up so a shake never arrives early: 686 m at 343 m/s is 120 steps at
60 Hz and 60 steps at 30 Hz. Travel waits while the game is paused (fixed steps stop).

## Order in a frame

In `PostUpdate`: `ShakeRestoreSet` puts back last frame's unshaken pose (only if nothing moved the
camera since), the rigs (`CameraSet::Update`) and the shot player (`ShotDriveSet`) move the camera,
then `CameraSet::Director` receives emits, starts the shakes that arrived and adds the result. So a
blast during a showcase shot shakes the shot camera, and the shake never builds up in the pose a rig
eases from. A camera teleported between frames keeps the new pose; the shake is added to it.

## Contact emitter

`ContactShake` on a physics body emits a noise shake from the body when one of its contacts reports
`regia_physics::ContactForce` at or above `min_force_n`: amplitude times `force / reference_force_n`,
at most `max_scale` times, on `channels`, with `falloff_radius_m`. `layers` is a bitmask over the
other body's `CollisionLayer::index` (`u32::MAX`: any). A resting contact reports every step, so
`cooldown_steps` (default 6) keeps one emit per impact. The collider must raise the events:
`active_events.contact_force` on and `contact_force_threshold` at or below `min_force_n`.

## `camera.shake`

```json
{ "preset": "impact", "origin": [0.0, 0.0, -40.0], "falloff_radius_m": 120.0, "travel_speed_m_s": 343.0 }
→ { "emitted": true, "preset": "impact", "kind": "noise", "listening": 1,
    "listeners": [{ "entity": 4294967300, "distance_m": 40.0, "gain": 1.0,
                    "shakes": [{ "kind": "noise", "peak": 0.053, "duration_s": 0.45,
                                 "delay_s": 0.1166, "delay_steps": 7 }] }], ... }
```

Params: `preset` (`bump`, `impact`, `rumble`, `explosion` (default), `blast`, `ground`);
`amplitude`, `frequency_hz`, `duration_s`, `falloff` override the noise presets; `origin`;
`channels` (default 1); `falloff_radius_m`; `travel_speed_m_s`; `yield_kt` for `ground`. The emit
is read in the next frame; `listeners` is computed now with the same plan. `delay_steps` counts
fixed steps from when the emit is read.

With `player` (a local player index) a noise preset goes to that player's camera director instead of
the sensors (spec 14 § B5, ADR-7263): only the cameras marked `PlayerCamera(player)` feel it, through
their sensors. It answers `{ emitted, preset, kind, shake, player, shakes }` (`shakes`: the player
director's running shakes); `camera.get` lists every player's under `players`.

```json
{ "preset": "impact", "player": 1 }
→ { "emitted": true, "preset": "impact", "kind": "noise", "player": 1, "shakes": 1, "shake": { ... } }
```

## `camera.shake_sensor`

No params lists every rig, render camera and entity with a sensor (sorted by id, at most 256):
`{ field: { frame, fixed_hz, emitted, delivered, contact_emitted }, count, truncated, cameras: [{
entity, rig, render_camera, shot_driven, explicit, sensor, receiver }] }`. `sensor` is the one the
camera feels with (`null`: it does not shake); `receiver` holds `pending` (with `arrival_frame` and
`frames_left`), `active` (with `elapsed_s`), `received`, `fired` and `pose` (the offset and angles
added this frame, after gain). `{ "entity": id }` answers `{ field, camera }`; an unknown entity is
`F0003`.

## `camera.set_shake_sensor`

```json
{ "entity": 4294967300, "channels": 2, "gain": 0.5, "combine": "max", "axes": "camera" }
→ { "entity": 4294967300, "removed": false, "explicit": true,
    "before": { "channels": 1, "gain": 1.0, ... }, "after": { "channels": 2, "gain": 0.5, ... } }
```

Fields not given keep the camera's current sensor (its own or its default). `"remove": true` removes
the camera's own sensor (no other field allowed): a rig goes back to the default, a plain camera stops
shaking.

## Error codes

Errors are `F0002` with `data.reason`:

- `shake_unknown_preset`: `preset` is not one of the six names.
- `shake_override_not_noise`: `amplitude`, `frequency_hz`, `duration_s` or `falloff` with `blast` or
  `ground`.
- `shake_no_channels`: `channels` is 0, so no camera could hear it.
- `shake_unknown_combine`, `shake_unknown_axes`, `shake_unknown_clock`: a sensor field outside its
  names (`add` / `max`, `world` / `camera`, `gameplay` / `unscaled`).
- `shake_not_a_camera`: `camera.set_shake_sensor` on an entity without a `Transform`.
- `shake_player_noise_only`: `player` with `blast` or `ground` (a player's director plays noise shakes).
- `player_not_local`: `player` names no local player (no `PlayerManager`, or no director at that index).

Other mistakes (a negative amplitude, a zero duration, `origin` not three numbers, `channels` above
32 bits, `gain` outside 0..=100, a missing `entity` on `camera.set_shake_sensor`) are plain `F0002`.
An `entity` that does not exist is `F0003` on `camera.shake_sensor` and `camera.set_shake_sensor`.

## In Rust

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let drone_camera: Entity = todo!();
# let hit_point = Vec3::ZERO;
# fn default<T: Default>() -> T { T::default() }
# let mut emits: MessageWriter<regia_framework::camera::sensor::ShakeEmit> = todo!();
use regia_framework::camera::sensor::{ShakeEmit, ShakePreset, ShakeSensor, named_shake};

// The drone feed listens only to channel 2, at half strength.
commands.entity(drone_camera).insert(ShakeSensor { channels: 1 << 2, gain: 0.5, ..default() });
// A truck hits a wall: everything within 80 m on the default channel shakes, later farther away.
emits.write(
    ShakeEmit::at(hit_point, ShakePreset::Noise(named_shake("impact").unwrap_or_default()))
        .with_falloff(80.0)
        .with_travel(343.0),
);
```

The older paths keep working unchanged: `CameraDirector::shake` (channel 1, felt by every listening
camera alike), `BlastShock` and `camera.schedule_shock_shake` (measured to the first rig). See
[Fire light and shock shake](fire-light-shock-shake.md).
