# Camera body and the startled bird (agents)

The camera can be a physical actor: a body with a radius and a speed that the world reacts to
as it flies past. A bird placed near the path startles when the camera comes too close or too
fast. Everything is a function of the camera path and its clock, so `render.out` re-renders
give the same frames. Guide topic: `agent.guide {"topic": "camera_body"}`. Decision record:
ADR-1502.

## The wake

The body's air wake is closed form (`regia_core::math::wake`):

- **Front.** Above the speed of sound (343 m/s) it is the Mach cone behind the body, with half
  angle `asin(343 / U)`. Below it, it is the plane through the body across its travel: a point
  is reached as the body passes it.
- **Strength.** The peak gust at a point `r` metres off the path is
  `gain · ramp(U) · U · (radius_m / (radius_m + r))^falloff_exponent`. It fades to zero over the
  last 40 % of `range_m` and is capped at `max_gust_m_s`. The ramp is 0 below `min_speed_m_s`
  and 1 from `full_speed_m_s`.
- **Pulse.** In time the gust is the Friedlander pulse `(1 − τ/pulse_s) e^(−τ/pulse_s)`: a
  sharp flick, an ease back, a small rebound.
- **Direction.** It points along the front's normal: outward and forward behind a supersonic
  body, along the travel (and partly outward, `radial_share`) behind a slow one.

A real boom lasts milliseconds and would not move a blade of grass on screen. `pulse_s`
(default 0.3 s) is a deliberate stylization; the arrival times, the geometry and the distance
falloff are physical.

The defaults give about 6 m/s of gust 50 m from a 750 m/s pass, about 2 m/s 3 m from a 40 m/s
pass, and nothing beyond 150 m or below 8 m/s.

What reacts:

- **Grass** (the showcase's grass field). Blades lean with the gust and drop a little: at most
  60 % of their height, half of that at 4 m/s. The velocity pass sees the movement, so TAA and
  motion blur treat the flick as motion.
- **Dynamic rigid bodies** (`regia_physics`, when the host has a physics world) get the
  quadratic drag impulse `½ ρ C_d π a² |u|²` each fixed step. `a` is the body's reference
  radius from its collider. Kinematic and static bodies are left alone.
- **Dust and snow spray.** Each ground cell (`spray.cell_m`, 4 m) lifts one puff per pass, on
  the frame the front reaches it with a peak gust of at least `spray.lift_m_s` (4 m/s). Only
  while the body is at most `spray.max_height_m` (80 m) above the ground, at most
  `spray.max_puffs_per_frame` (24) a frame. `spray.surface` is `dust`, `snow` or `none`.
- **Birds** (below).

Not reacting yet: trees and brush meshes do not sway at all (the scatter shader has no wind),
so branches cannot flick until they do.

## Methods

- `camera_body.get {}` returns `{ spec, state, wake_active }`. `state` holds `source` (`none`,
  `sequence`, `follow`, `host`), `time_s`, `position`, `velocity`, `speed_m_s`, `mach`,
  `body_gust_m_s`, `frames`, `pushed_bodies`, `pushed_impulse`, `spray_puffs` and `spray_total`.
  F0003 when the host has no camera body.
- `camera_body.set { spec?, enabled? }` merges `spec` onto the current spec (JSON merge patch;
  values are clamped) and returns `{ spec }`. Mutating, permission `edit_scene`. Without a host
  plugin it creates the resource, but nothing drives it.
- `camera_body.wake { points, time? }` samples the gust at up to 10 000 points `[[x, y, z], ...]`.
  Without `time` it uses the body now; with `time` it uses the loaded path at that time. Each
  sample is `{ gust, gust_m_s, peak_m_s, since_front_s, lateral_m, behind_m }`;
  `since_front_s` is negative while the front has not arrived.
- `camera_body.proximity { radius_m?, from_s?, to_s?, step_s?, targets?, include_birds?,
  include_bodies?, include_scatter? }` lists what comes within `radius_m` (default 10) of the
  loaded camera path, measured surface to surface with the body's radius included. Targets are
  `[{ id, position, radius_m? }]`; the birds and dynamic rigid bodies are added by default and
  the scatter instances' bounding spheres on request. Each hit is `{ id, closest_s, gap_m,
  distance_m, body_position, speed_m_s, enter_s, exit_s, closing_at_enter_m_s }`. The closest
  approach is refined by golden-section search and the entry and exit by bisection, so the
  times do not depend on `step_s`. F0003 without a loaded path.
- `camera_body.clearance { clearance_m?, body_radius_m?, step_s?, from_s?, to_s?, ground?,
  obstacles?, propose_lift?, shot?, margin_m?, iterations? }` sweeps the loaded path:
  - against the terrain, below every sample and along every segment (a ridge between two
    samples is caught);
  - against the scatter as trees: a thin trunk from the ground to the centre of each
    instance's bounding sphere, and the sphere as the crown.

  It returns `{ clear, samples, min_ground_gap_m, min_ground_s, min_obstacle_gap_m,
  obstacles_checked, violations }`. Each violation is `{ kind: ground|trunk|crown, id, from_s,
  to_s, worst_s, position, gap_m, lift_m }`, one per run of consecutive samples. It moves
  nothing. With `propose_lift: true` it adds `lifted_keys` (the `shot`'s keys raised until it
  clears) and `lifted_clear`.
- `camera_body.lift_path { shot?, ..clearance params.., margin_m?, iterations?, dry_run? }`
  raises the shot's keys near each violation by the lift needed plus `margin_m` (default 1),
  rechecks up to `iterations` times (default 4), and loads the result into the shot player (the
  clock is kept). It returns `{ applied, shot, raised: [{ key, time, lift_m }], before, after }`.
  Mutating, `edit_scene`.

```json
{"method": "camera_body.set", "params": {"spec": {"wake": {"gain": 0.15, "range_m": 200}, "spray": {"surface": "snow"}}}}
{"method": "camera_body.wake", "params": {"points": [[120, 4, -30]], "time": 41.5}}
{"method": "camera_body.proximity", "params": {"radius_m": 25, "include_scatter": true}}
{"method": "camera_body.clearance", "params": {"clearance_m": 8, "propose_lift": true, "shot": "flight"}}
```

## The bird

A bird is a spec (`bird.json`, version 1, every field optional except `name`):

```json
{
  "name": "crow",
  "seed": 7,
  "position": [412.0, 38.5, -1210.0],
  "facing_deg": 120,
  "perch": "post",
  "species": "crow",
  "size_scale": 1.0,
  "startle": { "distance_m": 30, "closing_speed_m_s": 12, "alert_distance_m": 120, "gust_m_s": 2,
               "reaction_s": 0.06, "flinch_s": 0.15 },
  "flee": { "speed_m_s": 13, "takeoff_climb_deg": 35, "takeoff_s": 0.9, "erratic": 0.6, "jink_s": 0.45,
            "glide_after_s": 3.5, "glide_speed_m_s": 11, "glide_deg": 6, "wake_gain": 1 }
}
```

- `position` is where its feet are. `perch` is `ground`, `branch` or `post`.
- `species` is `sparrow`, `pigeon`, `crow`, `gull` or `raptor`. Each sets the span, length,
  wingbeat and colours; `size_scale` scales them, and `flap_hz`, `color` and `wing_color`
  override them.

Phases (`bird.list` reports them):

1. `perched`: idling. Seeded head turns every 1 to 2 s, a feather ruffle about every 4 s.
2. `alert`: the cue arrived. The bird freezes for `reaction_s` while its eyes widen.
3. `flinch`, for `flinch_s` (held to 0.1 to 0.2 s): it crouches, snaps its head toward the
   camera, flares its wings, and its eyes go wide (1.6×).
4. `take_off`, for `takeoff_s`: a panicked climb away from the camera's line at
   `takeoff_climb_deg`, with deep wingbeats 1.25× the species rate.
5. `flee`: erratic flapping flight away. It jinks every `jink_s` (±50° yaw and ±15° pitch,
   scaled by `erratic`) and banks into its turns. The camera's wake throws it (gust over a 0.2 s
   drag response, × `wake_gain`).
6. `glide`: after `glide_after_s` the wings hold out and it glides away at `glide_deg` below the
   horizon.

The cue is the first of:

- the camera within `startle.distance_m`;
- the camera within `startle.alert_distance_m` and closing at `startle.closing_speed_m_s` or
  faster;
- the wake's front arriving with a peak gust of `startle.gust_m_s` or more.

A camera above the speed of sound is seen but cannot be heard before its front, so with the
visual cues off the bird startles on the boom.

The trigger follows from the path: it is scanned every 1/240 s and the first cue refined by
bisection. A path longer than about 69 minutes (a timelapse) is scanned at a coarser step, at
most a million samples over its whole span, so a cue shorter than that step can be missed there.
`bird.trigger` gives it without spawning anything, so a host can place the bird by the path:

1. Pick the pass time on the path.
2. `bird.place_on_path { time_s, right_m, up_m, ahead_m }` gives the feet position. Put the bird
   there, on the ground or a post, at a height that fits the scene.
3. `bird.trigger { spec }` gives `trigger.time_s`, `flinch_s` and `takeoff_s`.

The flight runs at a fixed 1/240 s from the take-off with seeded jinks. A seek backwards
replays from the take-off, and sub-frames between steps extrapolate, so the bird is the same
in every render of the same path.

The model is procedural: 14 rigid parts (body, head, beak, eye whites and pupils, two panels
per wing hinged at the shoulder and wrist, tail, legs). It uses no assets. Each part is its own
entity, so the renderer's per-object motion vectors (the previous model matrix) carry the
wingbeat to TAA and motion blur.

- `bird.spawn { spec }` queues a bird; it is spawned next frame, replacing one of the same name.
  Returns `{ name, replaced, timing: { trigger, flinch_s, takeoff_s, path_loaded } }`. Mutating,
  `edit_scene`. F0003 when the host has no bird plugin; F0002 for a bad spec.
- `bird.list {}` returns `{ birds: [{ name, species, phase, position, velocity, trigger,
  flinch_s, takeoff_s, eye_scale, wing_raise_deg, time_s }], pending }`.
- `bird.remove { name }` removes a bird next frame (F0003 for an unknown name).
- `bird.trigger { spec | name }` gives `{ trigger: { time_s, cause: distance|closing|wake,
  camera_position, camera_velocity, distance_m, closing_m_s } | null, flinch_s, takeoff_s,
  path_loaded }` on the loaded path.
- `bird.place_on_path { time_s, right_m?, up_m?, ahead_m? }` gives `{ position, time_s }`: the
  feet position at those offsets from the camera at that time (right of its travel, up, ahead).
  F0003 without a loaded path.

```json
{"method": "bird.place_on_path", "params": {"time_s": 41.5, "right_m": 6, "up_m": -2}}
{"method": "bird.spawn", "params": {"spec": {"name": "crow", "position": [412.0, 38.5, -1210.0], "perch": "post"}}}
{"method": "bird.list", "params": {}}
```

## In the showcase

The showcase host installs both plugins. The heightfield is the spray's ground and the bird's
floor, and the wake goes to the grass. Two optional files in the scene directory:

- `camera_body.json`: a camera-body spec. Without it the body is installed but off, so older
  scenes render exactly as before. `camera_body.set {"enabled": true}` turns it on, which is
  the way to make before/after takes of one path.
- `birds.json`: `{ "version": 1, "birds": [ <bird spec>, ... ] }`. Bad entries are skipped with
  a warning.

## Errors

- F0002 is a bad parameter or spec (unknown fields are refused).
- F0003 is no camera body, no bird plugin, no bird of that name, no loaded camera path, or no
  shot of that name.
