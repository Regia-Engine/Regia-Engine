# Audio ambience

Ambience that matches the picture (ADR-2442): a looping **bed** whose wind, gust, rustle, rain and drip
layers follow the scene's wind and weather, **point ambiences** such as a stream or birds in a tree line,
and **surface footsteps** from animation foot events. Everything is opt-in: nothing plays until a host or
an agent creates a bed, a spot or a footstep bank. All methods need `regia_audio::AudioPlugin`
(`regia::DefaultPlugins` includes it); without it they answer `F0013`.

## How the bed follows the weather

The bed reads `regia_scene::weather::SceneWind` (the same wind the renderer bends the foliage with,
spec 88 and spec 99 § 3.3) and `SurfaceWeather`. Each layer has a **drive** and a **curve** of
`[drive value, level dB]` points:

| Drive | Value | Silent when |
|---|---|---|
| `constant` | 1 | never (the calm air tone) |
| `wind` | mean wind, m/s | wind now under `calm_m_per_s` |
| `gust` | wind now minus mean wind, m/s | no gust, or under `calm_m_per_s` |
| `rustle` | `speed / (speed + 5)`, the foliage sway stage's leaf-flutter factor | under `calm_m_per_s` |
| `rain` | rain rate, mm/h | no rain |
| `drips` | wetness 0..1, only once the rain stopped | raining, or dry |
| `snow` | snowfall, mm/h water equivalent | no snow |

A layer's level slews toward its target with `attack_secs` (rising) and `release_secs` (falling), so a gust
swells and dies away. The `forest` preset: air −42 dB; wind −54 dB at 0.3 m/s, −36 at 2, −20 at 8, −10 at
17; gust −48 dB at 0.2 m/s over the mean, −16 at 6; rustle −54 to −13 dB; rain −46 dB at 0.1 mm/h to −8 at
50; drips −50 to −30 dB. A still day (no `SceneWind`, or under 0.3 m/s) is the air tone alone.

Without a sound a layer still computes its level (`playing: false`), so the levels can be checked before
assets exist. Register a loop under a name in `SoundAssets` and set it as the layer's `sound`.

## `audio.ambience`

No params. Returns `{ bed, weather, foliage, coherence, spots }`:

- `bed` (null until `audio.set_ambience`): `calm_m_per_s`, `attack_secs`, `release_secs`, `bus`,
  `volume_db`, `wind_override_m_per_s`, `inputs` (what the last step heard), `layers[]` (`name`, `sound`, `drive`, `curve`,
  `drive_value`, `target_db`, `level_db`, `playing`, `missing_sound`), `wind_level_db` (the wind, gust and
  rustle layers summed as power; −100 is silence).
- `weather`: `wind_m_per_s`, `mean_wind_m_per_s`, `gust_m_per_s`, `rain_mm_per_h`, `snow_mm_per_h`, `wetness`.
- `foliage` (null without a `RuntimeRenderer`): the renderer's foliage wind, `speed_m_per_s` and `still`.
- `coherence` (null without a bed): `foliage_speed_m_per_s` (the renderer's foliage wind, or the scene wind
  without a renderer), `foliage_still`, `wind_silent` (`wind_level_db` at or under −60 dB) and `agree` (`false`
  when the foliage is still and the wind is heard, or the foliage sways at 2 m/s or more and the wind is silent:
  a host that sets the renderer's foliage wind without a `SceneWind` sets `wind_override_m_per_s` too).
- `spots`: `count`, `calls_spawned`, `live_calls`.

## `audio.set_ambience`

```json
{ "method": "audio.set_ambience",
  "params": { "preset": "forest",
              "layers": [{ "name": "wind", "sound": "amb/wind_loop" },
                         { "name": "creek", "drive": "constant", "curve": [[1, -30]], "sound": "amb/creek" }] } }
```

Fields: `enabled` (false stops the voices and removes the bed), `preset` (`forest`, `empty`; starts over),
`calm_m_per_s` (0..40), `attack_secs` (0..30), `release_secs` (0..60), `volume_db` (−60..12), `bus`
(default `Ambience`), `wind_override_m_per_s` (`[x, z]` m/s the bed hears while no `SceneWind` exists, for a host
that sets the renderer's foliage wind directly; a `SceneWind` wins, as in the renderer; `null` clears it), `layers` (edit by `name`: `sound` (a name or null), `drive`, `curve` (1 to 32 points),
`remove`; a new name needs `drive` and `curve`), `test_tones` (true: register generated placeholder loops,
shaped noise named `ambience_test/<layer>`, for the forest layers that have no sound; they are test tones, not
recordings). Without `preset` the call edits the current bed (or a
`forest` bed when there is none). Returns `{ before, after }`. A failed call changes nothing.

Errors: `ambience_unknown_preset` (not `forest` or `empty`), `ambience_unknown_drive` (a drive not in the
table above), `F0002` for out-of-range or malformed fields.

## Point ambiences

An `AmbientSpot` component on an entity with a transform. A **loop** spot (water) carries one looping
`AudioEmitter`; a **scatter** spot (birds) spawns a short-lived emitter entity per call at a random point
within `radius_m` and up to `height_m` above the spot, every `interval_secs` (random in the range), with a
variant that is never the previous call's. Both are heard through the emitter pipeline: distance gain by the
spot's rolloff, a distance low-pass (the emitter's air-absorption ramp, 20 kHz at `min_distance` to 2 kHz at
`max_distance`, and the medium's absorption), and occlusion when an occlusion provider is installed
(`regia::DefaultPlugins` casts the rays through physics): −15 dB and a 700 Hz cutoff fully occluded. The
weather silences a spot at or above `quiet_above_wind_m_per_s` or `quiet_above_rain_mm_per_h`: a loop fades out
at 30 dB/s, a scatter spot skips its calls. Calls are not started for a listener out of reach
(`max_distance` plus the radius).

Presets: `water` (a loop, −6 dB within 3 m, 6 dB per doubling to 60 m, occluded, never silenced) and `birds`
(a call every 2 to 9 s from within 15 m and 12 m up, −8 dB within 4 m, to 120 m, quiet over 1 mm/h of rain or
12 m/s of wind).

## `audio.ambient_spots`

No params. Returns `{ spots, count, listener }`. Each spot: `entity`, `position`, `sounds`, `kind`
(`{ kind: loop }` or `{ kind: scatter, interval_secs, radius_m, height_m }`), `volume_db`, `min_distance`,
`max_distance`, `rolloff`, `occlusion`, `quiet_above_wind_m_per_s`, `quiet_above_rain_mm_per_h`, `bus`,
`seed`, `hearing` (what player 0 hears now: `distance`, `gain`, `gain_db`, `cutoff_hz`, `occlusion`,
`quieted`, `audible`) and `schedule` (`calls`, `skipped`, `next_call_in_secs`, `last_variant`, `looping`,
`weather_db`).

## `audio.set_ambient_spot`

```json
{ "method": "audio.set_ambient_spot",
  "params": { "position": [30, 2, -12], "preset": "birds",
              "sounds": ["amb/bird_a", "amb/bird_b", "amb/bird_c"], "interval_secs": [3, 8] } }
```

With `position` and no `entity`: creates a spot entity there. With `entity`: edits its spot (a `preset`
keeps its sounds), moves it with `position`, or removes the spot and its emitter with `remove: true`.
Fields: `preset` (`water`, `birds`), `sounds`, `kind` (`loop`, `scatter`), `volume_db` (−80..12),
`min_distance`, `max_distance`, `occlusion`, `quiet_above_wind_m_per_s` and `quiet_above_rain_mm_per_h`
(null: never), `interval_secs` (`[min, max]`, 0.05 ≤ min ≤ max ≤ 3600), `radius_m`, `height_m`, `bus`, `seed`.
Returns `{ entity, created, spot }`.

Errors: `ambience_unknown_preset`, `ambience_unknown_kind`, `F0003` (no such entity), `F0002`.

## `audio.probe_ambience`

Moves nothing. `listener` and `position` are `[x, y, z]`; the spot is an `entity`'s, or a `preset` (default
`water`) with any `audio.set_ambient_spot` field applied; `occlusion` (0..1, default 0) is the amount between
them. Returns `{ distance, gain, gain_db, cutoff_hz, occlusion, quieted, audible }` (audible: above −60 dB and
not silenced by the weather).

Errors: `ambience_unknown_preset`, `ambience_unknown_kind`, `F0003`, `F0002`.

## Footsteps

A `FootstepBank` resource maps a **surface** to a set of variants. An animation notify whose name starts with
the bank's `notify_prefix` (default `Footstep`: `Footstep_L`, `Footstep_R`) on an entity with a
`regia_framework::footsteps::FootstepWalker` plays one step (`FootstepPlugin`, in `regia::DefaultPlugins`).
The surface is the walker's `surface` override, else the host's `FootstepSurfaceProbe` (a physics material, a
road mask), else the dominant terrain layer under the foot mapped through `layer_surfaces` (a layer not listed
is its own surface name), else `default_surface`. The variant is never the walker's previous one on that
surface. The speed is the walker's `speed_m_per_s`, else the distance between its last two steps over the time
between them (a gap over 2 s starts fresh at the walk speed). Level: `+speed_db_per_doubling` (4) per doubling
of speed over `walk_speed_m_per_s` (1.4) and `+weight_db_per_doubling` (3) per doubling of weight over
`reference_weight_kg` (75), together within −12..+6 dB; pitch × `(reference / weight)^0.15`; then the set's
random spread (±4 % pitch, ±1.5 dB by default). Steps play at the foot through an emitter heard to 25 m on the
`World` bus.

## `audio.footsteps`

No params. Returns `{ bank, tracker }`: `bank` (null until `audio.set_footsteps`) with `sets` (per surface:
`sounds`, `volume_db`, `pitch`, `pitch_jitter`, `volume_jitter_db`, `missing_sounds`), `layer_surfaces`,
`default_surface`, the scaling fields, `notify_prefix`, `max_distance`, `bus`, `played` and `recent` (the last
32 steps: `walker`, `surface`, `fallback`, `variant`, `sound`, `volume_db`, `pitch`, `position`, `started`);
`tracker` counts foot notifies that became `steps` and those `ignored` (no walker on the entity, no set).

## `audio.set_footsteps`

```json
{ "method": "audio.set_footsteps",
  "params": { "sets": { "grass": { "sounds": ["fs/grass_1", "fs/grass_2", "fs/grass_3", "fs/grass_4"] },
                        "gravel": { "sounds": ["fs/gravel_1", "fs/gravel_2", "fs/gravel_3"], "volume_db": -2 } },
              "layer_surfaces": { "Meadow": "grass", "Path": "gravel" },
              "default_surface": "grass" } }
```

Fields: `enabled` (false removes the bank), `sets` (`{ surface: set | null }`; a set has 1 to 64 `sounds`
and optional `volume_db` (−60..12), `pitch` (0.25..4), `pitch_jitter` (0..0.5), `volume_jitter_db` (0..12)),
`layer_surfaces` (`{ layer: surface | null }`), `default_surface`, `notify_prefix`, `walk_speed_m_per_s`,
`speed_db_per_doubling`, `reference_weight_kg`, `weight_db_per_doubling`, `weight_pitch_exponent`,
`max_distance`, `bus`, `seed` (reseeds the picks and forgets every walker's history). Returns
`{ before, after }`.

## `audio.footstep`

Plays one step as a foot notify would. `position` (required), `walker` (default 0), `surface`,
`speed_m_per_s` (default the walk speed), `weight_kg` (default the reference weight), `plan_only` (pick and
report without playing). Returns `{ walker, surface, fallback, variant, sound, volume_db, pitch, position,
started }` (`started` false when the variant's sound is not registered or out of range).

Errors: `F0003` (no bank: call `audio.set_footsteps` first), `footstep_no_set` (no set for the surface or the
default surface), `F0002`.

## Checking that the sound matches the picture

```text
render.set_scene_wind { "mode": "manual", "wind_m_per_s": [0, 0] }
audio.set_ambience { "preset": "forest" }
play.step { "frames": 600 }
audio.ambience {}            # bed.wind_level_db -100, coherence.agree true
render.set_scene_wind { "mode": "manual", "wind_m_per_s": [9, 0] }
play.step { "frames": 600 }
audio.ambience {}            # wind_level_db near -18, foliage.speed_m_per_s 9
audio.set_ambient_spot { "position": [40, 0, 0], "preset": "water", "sounds": ["amb/creek"] }
audio.probe_ambience { "listener": [0, 1.7, 0], "position": [40, 0, 0], "preset": "water" }
```

The regia_agent test `the_wind_heard_matches_the_foliage_drawn` sweeps the scene wind from 0 to 18 m/s and
checks the bed against the sway stage's CPU mirror (`FoliageSway::offset`): still foliage, silent wind; a top
moving under 2 cm, under −45 dB; over 20 cm, over −30 dB; more sway never less wind.

## Sounds the bed needs

The engine ships no ambience recordings. Until real assets exist, register any loop (a test tone, a
`.loopgen.ron` noise loop) to hear the levels move. A forest showcase needs: an air/room tone loop, a wind loop
(broadband, no tonal whistle), a gust swell loop, a leaf rustle loop, a rain-on-leaves loop and a drips loop
(each seamless, 20 to 60 s, mono or stereo); a stream or waterfall loop per water spot; 6 to 10 short bird
calls per species (mono, dry, peak-normalized, 0.3 to 3 s); and 4 to 8 footstep variants per surface (grass,
dirt, gravel, leaf litter, wood, snow), mono, dry, trimmed to the transient.

## Example calls

Example calls (`regia agent call <method> '<params json>'`):

```text
audio.ambience {}
audio.set_ambience { "preset": "forest" }
audio.ambient_spots {}
audio.footsteps {}
audio.footstep { "position": [0, 0, 0], "surface": "grass", "plan_only": true }
```
