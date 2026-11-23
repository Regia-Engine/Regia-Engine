# Vehicle audio

Cars that sound like cars (ADR-3271). A `regia_physics` vehicle with a `VehicleAudio` component plays engine
loops that follow its engine speed and load, tire squeal from its wheels' slip, rolling noise per ground
surface, suspension bumps, wind by speed and a one-shot per gear change. The voices play on emitter entities
at the engine, the tailpipe and every wheel, so distance, Doppler, occlusion and the medium apply as for any
emitter, and a listener seated in the car (`ListenerCoupling::vehicle`) hears it directly through the cockpit
path. Add `regia_framework::vehicle_audio::VehicleAudioPlugin` (with `AudioPlugin` and `VehiclePlugin`).

## The model

Each fixed step (`FixedUpdate`, after the physics step) the plugin reads the vehicle and steps a pure model
(`regia_audio::VehicleAudioModel`):

- **Engine layers.** Loops recorded at known engine speeds, an on-load set (throttle open) and an off-load
  set (overrun). The two layers of a set nearest the smoothed rpm crossfade with **equal power**: weights
  `cos(t·π/2)` and `sin(t·π/2)` where `t` is the rpm's position between the two recorded speeds, so
  `w0² + w1² = 1` and at a recorded rpm the weights are exactly `(1, 0)`. Each layer plays at
  `rpm / recorded rpm`, clamped to `min_pitch..max_pitch` (default 0.5..2).
- **Load.** The target is `throttle_weight × throttle + (1 − throttle_weight) × clamp(torque / peak_torque_nm, 0, 1)`
  from the drivetrain's `applied_throttle()` and `engine_torque()`; the on-load set gets `sin(load·π/2)` and
  the off-load set `cos(load·π/2)` (plus `off_load_db`), again equal power.
- **Rev limiter.** Within `margin_rpm` of `redline_rpm` with at least `min_throttle`, the engine drops to
  `cut_db` in a fixed `rate_hz` rhythm counted in steps (never a clock), through a 4 ms smoother.
- **Shifts.** Each increase of `upshifts()` fires one shift one-shot (downshifts too with `on_downshift`)
  and dips the engine to `dip_db` for `dip_secs`.
- **Whine.** An optional loop whose level is a curve of rpm, at `rpm / recorded_rpm`.
- **Tires.** Squeal from the combined slip `x = √((|α|/α_peak)² + (|κ|/κ_peak)²)` of each grounded wheel
  (slip angle α and slip ratio κ as `regia_physics` reports them in `WheelState`): exactly 0 at or below
  `threshold`, rising linearly to full at `full`. Rolling noise per wheel picks the loop of the ground's
  `VehicleAudioSurface` (else the `default` surface) at a level by contact speed. A bump one-shot fires
  when a wheel's suspension compresses faster than `rate_threshold_mps`, at most once per `cooldown_secs`.
- **Wind.** A loop whose level is a curve of chassis speed.

Every parameter passes through a fixed-step one-pole smoother, `value += c × (target − value)` with
`c = 1 − e^(−dt/τ)`: the change per step is at most `c × |target − value|`, so a parameter never jumps
(no zipper noise). Defaults: rpm 30 ms, load 80 ms, levels 50 ms, tires 60 ms. The first step snaps rpm and
load to their targets. A vehicle with the `Simple` drivetrain has no engine state: its rpm is
`900 + 160 × |forward speed|` (to the redline) and its load comes from the throttle input.

Steady-state steps allocate nothing (`crates/regia_audio/tests/vehicle_audio_alloc.rs`), and the same inputs
give the same `parameter_digest`. `Vehicle::wheel_states` returns a new `Vec` per call; that readout belongs
to `regia_physics`.

## The voice budget

`VehicleAudioBudget { full_vehicles: 3, reduced_vehicles: 12, max_distance: 250 }`. Every frame the vehicles
are ranked: the listener's own vehicle first, then by distance to the nearest listener (ties by entity). The
first `full_vehicles` play every audible layer; the next `reduced_vehicles` play their loudest engine layer
only (it changes only when another is 25 % louder); the rest, and any vehicle beyond `max_distance`, are
silent. A voice starts when its level rises above −60 dB and stops (60 ms fade) below −80 dB.

## Placeholder sounds

`preset: placeholder` (`VehicleAudioDef::placeholder()`) names only `placeholder:...` sounds, which
`regia_audio::placeholder_sound` **generates in code**; they are labeled test signals, not recordings:

| Sound | What it is |
|---|---|
| `engine_on_<rpm>` | A harmonic stack at the firing frequency of a four-cylinder four-stroke (`rpm / 60 × 2`): saw fundamental, 2nd and 3rd harmonics, a half-order rumble, an amplitude-modulated noise pulse, low-passed at 3 kHz; rendered by loopgen over whole cycles so it loops seamlessly. |
| `engine_off_<rpm>` | The same with a sine fundamental, weaker harmonics, low-passed at 1 kHz. |
| `whine_<rpm>` | Two sines at 12× and 24× the crank rate. |
| `squeal` | Narrow band-passed white noise at 1.4 kHz with a slow flutter. |
| `roll_asphalt`, `roll_gravel` | Low-passed brown noise (600 Hz), low-passed pink noise (3.5 kHz). |
| `wind` | Band-passed pink noise with a slow swell. |
| `shift`, `bump` | Decaying noise bursts (90 ms, 180 ms). |

Real cars replace them with recordings registered in `SoundAssets` under the names the definition uses.

## `audio.vehicle_audio`

No params. Returns `{ plugin, budget, counts: { full, reduced, silent }, digest, vehicles, rejected }`.
Each vehicle: `entity`, `name`, `mode` (`full`, `reduced`, `silent`), `distance`, `params` (`rpm`, `load`,
`limiter`, `limiter_cutting`, `dip`), `inputs` (the drivetrain readout of the last step), `steps`,
`shifts_fired`, `bumps_fired`, `one_shots_played`, `one_shots_dropped`, `live_voices`, `voices_started`,
`voices_stopped`, `missing_sounds`, `problem`, `digest` and `layers[]` (`index`, `role` such as `engine_on_1`,
`squeal_2`, `rolling_0`, `slot` such as `exhaust` or `wheel_2`, `sound`, `weight`, `amplitude`, `level_db`,
`pitch`, `voice`: whether a voice plays it). `rejected` lists definitions that did not build, with the
error text. Without the plugin: `plugin: false` and empty lists.

## `audio.vehicle_audio_set`

`{ entity?, preset?, ron?, remove?, budget? }`. With `entity` (a vehicle: it must carry a
`VehicleController`), `ron` (a `.vehicleaudio.ron`) or `preset: placeholder` attaches or replaces its sound
and `remove: true` removes it. `budget: { full_vehicles?, reduced_vehicles?, max_distance? }` sets the
budget. Returns `{ entity, changed, budget }`.

```json
{"method": "audio.vehicle_audio_set", "params": {"entity": 4294967297, "preset": "placeholder"}}
```

## `audio.vehicle_audio_probe`

Offline (no world, no voices): what a definition plays with every smoother settled.
`{ rpm?, load?, throttle?, engine_torque?, slip_angle_deg?, slip_ratio?, speed?, surface?, preset?, ron? }`
(rpm 3000 and full load by default; `load` sets throttle and torque together). Returns `{ name, params,
squeal, active_engine_layers: [{ role, weight, pitch }], layers, digest }`.

```json
{"method": "audio.vehicle_audio_probe", "params": {"rpm": 3500, "load": 1.0, "slip_angle_deg": 9}}
```

At a recorded rpm one on-load layer is active with weight 1 and pitch 1; between two, both are active and
their squared weights sum to 1.

## Error codes

| Code | When |
|---|---|
| `vehicle_audio_parse` | The `ron` text does not parse as a `.vehicleaudio.ron`. |
| `vehicle_audio_version` | `format_version` is not 1. |
| `vehicle_audio_value` | A value is out of range or not finite, or a list is empty, unordered or too long; the message names the field (`engine.on_load[1].rpm`). |
| `vehicle_audio_unknown_preset` | `preset` is not `placeholder`. |
| `vehicle_audio_not_vehicle` | The entity has no `VehicleController` (`audio.vehicle_audio_set`). |
| `vehicle_audio_no_plugin` | `VehicleAudioPlugin` is not running (`audio.vehicle_audio_set`). |
| `F0003` | No such entity. |

## Format

`.vehicleaudio.ron` v1: [docs/formats/vehicle-audio.md](../../docs/formats/vehicle-audio.md).

## Tests

- `regia_audio` unit tests (`vehicle_audio::tests`): the equal-power sweep (800 to 7,000 rpm, exact `(1, 0)` on
  recorded rpms), continuous pitch on a linear ramp, the smoothed load switch, squeal threshold and
  monotonicity, shift one-shots and the dip, the limiter rhythm, bumps, rolling surfaces, digests, the format.
- `crates/regia_audio/tests/vehicle_audio.rs`: a 3 s rpm ramp through the real mixer is finite, under 0 dBTP,
  and its spectral centroid rises; a layer pitch of 2 reaches the mixer through the emitter pipeline; voice
  modes bound the live voices.
- `crates/regia_audio/tests/vehicle_audio_alloc.rs`: no allocation per step.
- `regia_framework` (`vehicle_audio::tests`): a geared car accelerating from rest raises the rpm and plays
  exactly `upshifts()` shift one-shots; 50 cars keep at most `full_vehicles` full; the listener's own car comes
  first; equal drives give equal digests; despawn releases voices and emitters.
