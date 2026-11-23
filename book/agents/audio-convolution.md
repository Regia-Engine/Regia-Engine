# Convolution, time stretch and ambience files (agents)

Inspect and drive the convolution reverb, the reverb zones' impulse responses, the
pitch-preserving time stretch and `.ambience.ron` files (ADR-2721, `docs/specs/16-audio.md` § 1,
§ 3d and § 3e). Every method needs `regia_audio::AudioPlugin` (`regia::DefaultPlugins` installs
it); without it they return `UNSUPPORTED` (`F0013`).

## What the engine does

- **Convolution.** `BusEffect::Convolution { ir, mix, predelay_secs }` on any bus or send bus
  convolves the bus with a registered impulse response (IR). The convolver is uniformly
  partitioned overlap-save in 256-frame partitions: the wet path is 256 frames late (5.3 ms at
  48 kHz), which a predelay of at least that much absorbs. The dry path is not delayed. IRs are
  resampled to the mixer's rate when they are prepared. An unknown IR passes only the dry part of
  the mix.
- **Tiers.** `desktop` allows IRs up to 4 s, `mobile` (the Android default) 1.5 s, `low` runs no
  convolution at all and reverb zones keep their preset reverb.
- **Zone IRs.** An `AudioReverbZone` with `ir: Some("cave")` plays that IR instead of its preset
  while it is the most specific zone around player 0. The World and Ambience sends then feed the
  hidden `ZoneIr` send bus instead of `Reverb`, crossfading over 0.25 s. Between two IR zones the
  convolver itself crossfades (equal power). An IR stays loaded after the listener leaves, so its
  tail rings out.
- **Time stretch.** WSOLA (1024-frame Hann windows, 512-frame hop, ±384-frame search) changes
  speed without changing pitch, for rates 0.5 to 2. Rate 1 passes the samples through unchanged.
  Offline: `regia_audio::time_stretch`. On a playing voice: `Audio::play_time_stretched` and
  `Audio::set_time_stretch_rate`.
- **Ambience files.** `.ambience.ron` v1 (`docs/formats/ambience-ron.md`): looping beds whose
  levels follow wind and weather curves, and random one-shots (interval range, distance range,
  height and level ranges, never the same variant twice), all drawn from the file's `seed`. The
  same seed renders the same samples.

## List the bus effects

`audio.bus_effects` (read-only). `bus` narrows it to one bus.

```json
{ "method": "audio.bus_effects", "params": { "bus": "Reverb" } }
→ { "buses": [{ "bus": "Reverb", "send": true,
                "effects": [{ "kind": "reverb", "feedback": 0.85, "damping": 0.4, "mix": 1.0 }] }],
    "impulse_responses": [{ "name": "hall", "frames": 96000, "channels": 2,
                            "sample_rate": 48000, "duration_secs": 2.0 }],
    "tier": "desktop", "max_ir_secs": 4.0 }
```

Kinds: `eq`, `low_pass`, `high_pass`, `reverb`, `delay`, `compressor`, `distortion`,
`convolution` (`ir`, `mix`, `predelay_secs`). Errors: `F0003` for an unknown bus.

## Register an IR, change a convolution, set the tier

`audio.bus_effect_set` (mutating). Any combination of:

- `ir_path` (project-relative WAV, Ogg or FLAC) registers an IR under `ir` (default: the path).
- `bus` plus `index` (default 0) selects a bus convolution. With `ir`, it crossfades to that IR
  over `fade_secs` (default 0.25) with `predelay_secs`; `"ir": null` fades the wet path out.
  `mix` sets its wet/dry mix (0 dry, 1 wet).
- `tier`: `low`, `mobile` or `desktop`.

```json
{ "method": "audio.bus_effect_set",
  "params": { "ir_path": "audio/ir/hall.wav", "ir": "hall", "bus": "Reverb", "predelay_secs": 0.02 } }
→ { "registered": "hall", "tier": "desktop", "impulse_responses": [ ... ] }
```

Errors (each in `data.code`):

| Code | When | Fix |
|---|---|---|
| `ir_empty` | The IR has no samples, or the file no bytes. | Give the recorded or designed response. |
| `ir_truncated` | The file ends before its header says, or does not decode. | Re-export it; a cut WAV is refused, not played short. |
| `ir_silent` | Every sample is zero. | Check the export. |
| `ir_non_finite` | NaN or infinite samples. | Re-export as PCM or clean float. |
| `ir_channels` | Not 1 or 2 equal channels. | Use mono or stereo; split true-stereo IRs. |
| `ir_sample_rate` | Outside 8 kHz to 192 kHz. | Resample it. |
| `ir_too_long` | Longer than the tier allows (4 s desktop, 1.5 s mobile). | Trim the tail, or give mobile a shorter IR. |
| `ir_tier_disabled` | The tier is `low`. | Zones use their preset; raise the tier to use IRs. |
| `ir_unreadable` | The file cannot be read. | Give an existing project-relative path. |
| `convolver_busy` | The previous change has not reached the mixer yet. | Wait for `crossfading: false` and send again. |
| `F0003` | No such bus convolution, or no IR of that name. | `audio.bus_effects`, `audio.convolution_stats`. |

## Read the convolvers

`audio.convolution_stats` (read-only).

```json
{ "method": "audio.convolution_stats" }
→ { "convolvers": [{ "bus": "ZoneIr", "index": 0, "ir": "cave", "predelay_secs": 0.0,
                     "blocks": 1875, "swaps": 1, "crossfades_done": 1, "crossfading": false,
                     "ir_frames": 24000, "ir_channels": 1, "partitions": 94,
                     "latency_frames": 256, "wet_delay_frames": 256, "mixer_rate": 48000,
                     "rate_mismatch": false, "mix": 1.0, "max_step": 0.031 }],
    "zone_ir": "cave", "zone_ir_switches": 1, "zone_ir_fallbacks": 0,
    "tier": "desktop", "partition_frames": 256, "impulse_responses": [ ... ] }
```

- `max_step` is the largest change between two consecutive output samples the convolver has
  produced: a click detector. Compare it before and during a crossfade.
- `rate_mismatch: true` means the mixer runs at another rate than the kernel was prepared for;
  the wet path is silent for that frame and the audio system re-prepares the kernel on the next.
- `zone_ir_fallbacks` counts zones that named an IR the tier or the registry could not give.

## Play, stop and list ambience files

`audio.ambience_play` (mutating) takes a project `path` or the RON as `text`; `seed` overrides
the file's. It replaces a playing ambience of the same name.

```json
{ "method": "audio.ambience_play", "params": { "path": "audio/glade.ambience.ron", "seed": 7 } }
→ { "name": "glade", "seed": 7, "beds": ["air", "wind", "rain"], "one_shots": ["birds", "woodpecker"] }
```

`audio.ambience_stop { name }` fades the beds out; `audio.ambience_list` reports each playing
ambience:

```json
→ { "ambiences": [{ "name": "glade", "seed": 7, "clock_secs": 31.5,
                    "beds": [{ "name": "air", "level_db": -42.0 }],
                    "one_shots": ["birds", "woodpecker"],
                    "calls_played": 9, "calls_skipped": 0, "calls_missing": 0 }] }
```

`calls_skipped` counts calls a level curve silenced (birds in the rain); `calls_missing` counts
calls whose sound is not registered in `SoundAssets`.

Errors: `ambience_parse` (not valid RON for the format; unknown fields are refused),
`ambience_version` (a newer file; this engine reads version 1), `ambience_invalid` (a range, name
or curve out of bounds; the message names it), `ambience_unreadable` (the path cannot be read),
`ambience_not_playing` (`audio.ambience_stop` for a name that is not playing).

## Probe the time stretch

`audio.time_stretch_probe` stretches a test tone (`hz`, default 440; `secs`, 0.1 to 4) at `rate`
offline and measures it. With `voice` it also sets a playing time-stretched voice to `rate`.

```json
{ "method": "audio.time_stretch_probe", "params": { "rate": 0.75 } }
→ { "rate": 0.75, "hz": 440.0, "measured_hz": 440.1, "pitch_error": 0.0002,
    "duration_ratio": 1.3333, "expected_ratio": 1.3333, "rate_one_identical": true,
    "window_frames": 1024, "hop_frames": 512, "voices": [] }
```

Errors: `stretch_rate` (outside 0.5 to 2), `stretch_voice_not_found` (no playing voice with that
id).

## Checks an agent can run

1. Register an IR and send it to a bus convolution; `audio.convolution_stats` shows `ir_frames`
   and growing `blocks`.
2. Walk the listener into a zone with `ir`; `zone_ir` names it and `zone_ir_switches` grows.
   `audio.render_check` reports `max_step` for the mix across the switch.
3. Play an ambience twice with the same `seed` through `audio.render_check`: the same samples.
4. `audio.time_stretch_probe` at 0.5, 0.75, 1.5 and 2: `pitch_error` under 0.01 and
   `duration_ratio` within 1 % of `expected_ratio`.
