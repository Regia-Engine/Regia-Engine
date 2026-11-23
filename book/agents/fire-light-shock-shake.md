# Fire light and shock shake

Two showcase helpers that are pure functions of an integer frame, so a sequence renders the same
every time: a flickering fire light (`regia_vfx::fire_light`) and the camera shake of a distant
blast arriving at the speed of sound (`regia_framework::shock_shake`).

## Fire light

`FireLight { base_intensity, color_temperature_k, flicker_depth, flicker_hz, seed, lifetime_frames }`
and `sample(frame)` give an `intensity` (never negative, never NaN) and a linear `color`. There is
no clock and no global random generator: the flicker is value noise on a 64-bit integer hash of
`(seed, octave, lattice index)`, two octaves (a slow swell at a quarter of `flicker_hz`, a fast
flutter at `flicker_hz`, in a 60 Hz frame clock). The color is a blackbody fit of the temperature
(1800 K is orange); it reddens slightly when the flame is dim. The light is exactly zero from
`lifetime_frames` on and fades smoothly over the last 10 percent.

`FireLights` (a resource installed by `VfxPlugin`) holds the live fires and the fire clock. Each
update its lights join `VfxFrameLights`, ranked with the effect lights under the profile's light
budget, so the renderer's local-light path and `vfx.set_look` `fire_light` / `fire_light_scale`
apply unchanged. `FireLights::spawn_crash(&CrashFires { center, count, scatter_radius_m, seed,
start_frame })` adds the burning wreck (60 m radius, 12 minutes) and `count` fires (20 to 32 m
radius, each burning 3 to 6 minutes at 60 Hz), placed and seeded from `seed` only.

### `vfx.fire_lights`

No params. Result: `{ frame, count, listed, truncated, light_budget, published_lights, fires: [{
index, position, radius, seed, start_frame, lifetime_frames, remaining_frames, lit, intensity,
color }] }` (up to 200 fires). `UNSUPPORTED` (`F0016`) without `VfxPlugin`. `frame` is the fire clock: it
advances one frame per update, and a paused app holds it, so the idle frames a host runs between render-out jobs
do not move a fire's start (ADR-2633).

### `vfx.spawn_crash_fires`

`{ position: [x, y, z], count?: 0..200 (40), scatter_radius?: 0..5000 (300), seed?: int (0),
start_frame?: int (now), replace?: bool }`. Result: `{ added, total, start_frame, seed }`.
`INVALID_PARAMS` for a bad `position`, a `count` above 200, a negative or non-integer number.

## Shock shake

`ShockShake { energy, speed_m_s, reference_distance_m, max_amplitude, duration_s }`:

- `arrival_delay_s(d)` is `d / speed` (0 for a non-positive or NaN distance); 343 m is 1 s, which is
  60 frames at 60 Hz.
- `amplitude(d)` is `energy * reference / max(d, reference)` clamped to `0..=max_amplitude`; it
  never grows with distance, and a NaN distance gives 0.
- `to_shake(d)` gives `Some((delay_s, Shake))`, or `None` when the shock is negligible.
- `arrival_frame(d, fixed_hz, start_frame)` rounds up to the next fixed step, never early.
- `ShockShake::showcase()` is the showcase blast: 0.35 m at 1 km, at most 0.2 m, 3.5 s (about 0.1 m
  at the 3.5 km of shot 4).

`ShockShakePlugin` (add it with the `CameraPlugin`) counts fixed steps in `ShockShakeSchedule` and
calls `CameraDirector::shake` on each shock's arrival frame. Send a `BlastShock { position, shock }`
message and it measures the distance to the first `CameraRig` and schedules the shake from the
current step; or call `ShockShakeSchedule::schedule(&shock, distance, blast_frame)`. The delay is
counted in fixed steps of the app's `FixedTime`, so the frame is exact at the app's fixed rate.

### `camera.shock_shake`

`{ distance_m?, blast_frame?, energy?, speed_m_s?, reference_distance_m?, max_amplitude?, duration_s? }`
(overrides apply to the showcase blast). Result: `{ frame, fixed_hz, fired, pending: [{
blast_frame, distance_m, arrival_frame, frames_left, amplitude }], blast, prediction: { distance_m,
delay_s, arrival_frame, amplitude, shake } | null }`.

### `camera.schedule_shock_shake`

`{ distance_m, blast_frame?, ...overrides }`. Result: `{ scheduled, blast_frame, arrival_frame,
delay_s, amplitude, pending }`; `scheduled` is false when the shock is negligible at that distance.
`INVALID_PARAMS` for a missing or negative `distance_m` and a non-positive override.
`UNSUPPORTED` (`F0016`) without `ShockShakePlugin`.

## Ground shake: the seismic wave and the airblast

The translation shake above is a fraction of a millimetre to a camera kilometres away, which no lens
shows. `regia_framework::ground_shock` adds the angular term (ADR-1457). `GroundShock { yield_kt (0.35),
coupling (0.05), p_speed_m_s (5500), air_speed_m_s (343), lever_m (1), mount_q (0.8), area_m2, stiffness_n_m (57600),
dynamic_factor, surface_factor, seismic_decay_s (1.4), air_decay_s (0.4), seismic_shape, air_shape, visibility }` makes two `MountShock`s per blast:

- **Seismic**, first: ground velocity from the USBM scaled-distance law, displacement at 6.5 Hz, the
  tripod turning it into a camera rotation `Q * u / L`. Arrives at `distance / 5.5 km/s`.
- **Airblast**, later: Kinney-Graham overpressure of a surface burst loading the camera against the
  tripod's stiffness. Arrives at `distance / 343 m/s`.

Each is pitch, yaw (0.8) and roll (0.3) under a rise-and-decay envelope with a sub-millimetre
translation, so the same schedule replays identically. The seismic wave rings: damped sines at seeded 4 to
9 Hz (rise 0.12 s, decay 1.4 s, about 10 s in all). The airblast is one short jolt (`ShakeShape::Impulse`):
a 30 ms rise and a 0.4 s decay with one sign per axis, down to 2 percent in 1.6 s, because its positive phase is
far shorter than the mount's period. `ShakeVisibility` bounds both: a wave under the floor (4e-5 rad,
0.25 px at 4K on a 20 degree lens) is not scheduled, and none tilts the camera over the ceiling (2
degrees). `CameraDirector::sample_pose` returns the `ShakePose` (offset, pitch, yaw, roll);
the showcase host and `CameraRig`s apply it. Numbers for 0.35 kt at 60 degrees vertical field of view and
2160 rows:

| Camera distance | Ground velocity | Seismic tilt (legacy, Q 10) | Airblast | Airblast tilt |
|---|---|---|---|---|
| 300 m | 0.31 m/s | about 4 degrees (140 px) | 59 kPa | about 4 degrees |
| 1 km | 45 mm/s | 0.6 degrees (20 px) | 8 kPa | 0.6 degrees |
| 3.3 km | 6.7 mm/s | 0.09 degrees (3 px) | 2.2 kPa | 0.16 degrees (5 px) |

Those seismic tilts are the legacy `Q` of 10 (ADR-1457). The default `Q` is 0.8 (ADR-1475, was 1.5 in ADR-1473),
0.8/10 of that tilt (0.0075 degrees, 0.8 px at 4K on a 20 degree lens at 3.3 km). The default head is
stiff (57.6 kN/m, a fluid head on a heavy tripod; legacy 12 kN/m), so the 3.3 km airblast is one jolt
of 0.033 degrees (3.5 px there, legacy 0.158 degrees, 16.8 px). Both are capped at the 2 degree ceiling. Pass `shake_profile: "legacy"` for the old strengths.

The coupling, `Q`, lever, area and stiffness are stated assumptions (`docs/provenance.md`); tune them as
`GroundShock` fields.

### Agent surface

`camera.schedule_shock_shake` schedules both waves by default: `ground` (false leaves them out) and
`yield_kt` (0.2 to 0.5 for the pod impact). Extra result fields: `ground_scheduled`,
`seismic_arrival_frame`, `air_arrival_frame`, `ground` (`ppv_mm_s`, `ground_displacement_mm`,
`overpressure_pa`, both delays, arrival frames and tilt angles in degrees), `ground_pending`.
`camera.shock_shake` takes `yield_kt`, lists `ground_pending` (`wave`, `arrival_frame`, `frames_left`,
`angle_deg`, `seed`) and `ground_fired`, and its prediction carries the same `ground` object.
`shake_profile` (`"calibrated"`, the default, or `"legacy"`), `mount_q` (positive) and `ceiling_deg`
(above 0, at most 20) set the strength; the `ground` object also reports `mount_q` and `air_shape`
(`"impulse"` or `"oscillation"`). `INVALID_PARAMS` for a non-positive `yield_kt` or `mount_q`, a
`ceiling_deg` outside (0, 20], an unknown `shake_profile` and a non-boolean `ground`.

## Check it

```sh
regia_agent call camera.shock_shake '{"distance_m": 343}'      # delay_s 1.0, arrival_frame 60
regia_agent call camera.shock_shake '{"distance_m": 3300}'     # prediction.ground: seismic frame 36, airblast 578
regia_agent call vfx.spawn_crash_fires '{"position": [0, 0, 0], "count": 40, "seed": 2026}'
regia_agent call vfx.fire_lights '{}'                          # lit, intensity, color per fire
```

`cargo test -p regia_vfx --test fire_light` renders a lit sphere under the fire at a swell frame
and a lull frame (the swell is brighter) and twice at one frame (identical bytes). Set
`REGIA_FIRE_LIGHT_DUMP=<dir>` to write the frames as PNGs.

Per-camera sensors, emits with falloff and travel, a contact emitter and the plain `camera.shake` method are on
[Camera shake sensors](camera-shake.md) (ADR-1619). The blast law above is also the `Blast` preset of a
`ShakeEmit`, with the same numbers.
