# Voice capture and loopback

Inspect and drive the voice capture stream and the voice loopback (specs/16 § 3b). All seven
methods need `regia_audio::AudioPlugin` (`regia::DefaultPlugins` installs it); without it they
return `UNSUPPORTED` (`F0013`). The method-specific codes are `capture_headless` and
`no_loopback` (below).

Regia Engine captures and plays voice back; it does not encode or transport it — that is the
game's job. The game drains captured frames to send to other players, and pushes decoded remote
frames into the loopback to hear them.

## Read the state

`audio.voice` (read-only) reports the capture stream and the loopback, if any.

```json
{ "method": "audio.voice" }
→ { "capturing": true, "device": "Built-in Microphone", "sample_rate": 48000, "channels": 2,
    "level_db": { "rms": -38.1, "peak": -12.4 }, "voice_active": true, "talking": true,
    "dropped_frames": 0,
    "loopback": { "running": true, "buffered_frames": 1240, "last_latency_ms": 34,
                  "max_latency_ms": 41, "chunks_pushed": 122, "chunks_dropped": 0 } }
```

- `level_db` is `null` when nothing is capturing; `rms` and `peak` are dBFS amplitudes.
- `voice_active` is the energy-based voice-activity detector (ITU-T P.56 style: short-term RMS
  threshold with hysteresis and a hangover).
- `talking` is the push-to-talk gate; `dropped_frames` counts frames the capture ring dropped
  because the game drained them too slowly.
- `loopback` is `null` when no loopback is running. `last_latency_ms` / `max_latency_ms` measure
  capture (or push) to first render; keep `max_latency_ms` under 60 ms for the § 3b loopback
  acceptance.

## Start and stop capture

`audio.capture_start` (`play` permission) opens the microphone; `audio.capture_stop` releases it.

```json
{ "method": "audio.capture_start", "params": { "sample_rate": 48000, "buffer_secs": 2 } }
→ { "capturing": true, "device": "Built-in Microphone", "sample_rate": 48000, "channels": 2 }
```

- `device` is a substring to prefer (default input device when omitted); `sample_rate`
  8000..=192000; `buffer_secs` 0.05..=30; `on_threshold_db` / `off_threshold_db` -90..=0
  (default -40 / -50; `off` must stay below `on`, checked against the other's default when only one
  is given); `hangover_ms` 0..=5000 (default 250); `window_ms` 1..=2000 (default 20).
- The device may open at a different rate than requested; the answer reports what opened.
- On Android the `RECORD_AUDIO` permission must already be granted by the platform layer —
  Regia Engine never prompts on its own. With no input device it answers with an error. In a
  headless run (`Audio::is_headless`) it answers `capture_headless` (`UNSUPPORTED`) and does not
  open a device. A threshold pair that breaks hysteresis is reported before that refusal.

```json
{ "method": "audio.capture_stop" }
→ { "capturing": false }
```

## Push-to-talk

`audio.set_talking` (`play` permission) replies with the requested flag. The gate is written only
when a capture exists, so with no capture `audio.voice` stays `talking: false` and a later
`audio.capture_start` opens the gate. With the gate closed, the level meter and the
voice-activity detector still update, then the samples are discarded: `Audio::tick_voice` returns
an empty list and nothing is pushed to the loopback.

```json
{ "method": "audio.set_talking", "params": { "talking": true } }
→ { "talking": true }
```

## The loopback

`audio.voice_loopback_start` (`play` permission) starts (or restarts) the loopback on the Voice
bus; `audio.voice_loopback_stop` stops it. The game pushes decoded remote frames into the running
loopback with `regia_audio::Audio::push_voice_frames` (or `Audio::tick_voice`, which drains the
capture straight into it for a local loopback test).

```json
{ "method": "audio.voice_loopback_start", "params": { "prebuffer_ms": 40 } }
→ { "loopback": { "running": true, "buffered_frames": 0, "last_latency_ms": 0,
                  "max_latency_ms": 0, "chunks_pushed": 0, "chunks_dropped": 0 } }
```

- `prebuffer_ms` is the jitter-buffer target before playback starts (0..=2000, default 40). Remote
  voice is untrusted data: non-finite samples are silenced and the rest clamped to -1..1 at the
  mixer boundary.

```json
{ "method": "audio.voice_loopback_stop" }
→ { "loopback": null }
```

## Placing the loopback (3D positioning)

`audio.voice_place` (`play` permission) places the running loopback in the stereo field
(specs/16 § 3b): distance gain, pan and an optional air-absorption low-pass, or the flat
center default. Refused with `no_loopback` (`INVALID_REQUEST`) when no loopback is running;
starting a new loopback resets the placement to center.

```json
{ "method": "audio.voice_place", "params": { "mode": "spatial", "gain": 0.8, "pan": -0.5 } }
→ { "placement": { "mode": "spatial", "gain": 0.8, "pan": -0.5, "cutoff_hz": null } }
```

- `mode`: `"center"` restores the flat default; `"spatial"` takes `gain` 0..=1 (default 1),
  `pan` -1..=1 (default 0) and `cutoff_hz` 1..=20000 (air-absorption low-pass, omitted = none).
- The gains ramp to a new placement over one audio batch and hold it exactly from the next, so
  per-frame updates from a moving anchor never click.
- A game places the stream from the world instead by putting a `VoiceStreamAnchor` component
  (distances, rolloff, air absorption) on the speaking entity: the AudioSync pass then pans and
  attenuates the stream like an emitter at that entity's position against the nearest listener.
  When the last anchor disappears (the entity despawns or is parked), the pass resets the stream
  to the flat center on that frame — an anchor that went away past `max_distance` left the
  placement at gain 0 and the loopback would otherwise stay muted. With no anchor at all, a
  placement set by `audio.voice_place` (or the game) is kept.
- `audio.voice` reports the live placement inside `loopback` as
  `{ mode: center | unresolved | spatial, gain?, pan?, cutoff_hz? }` (`mode` only when a
  stream runs).

## Recipes

- *Is the microphone live?* `audio.voice`: `capturing` true, `device` non-empty, and after a
  moment of speech `level_db.rms` rises and `voice_active` turns true.
- *Does the loopback meet the latency budget?* Start a loopback (`audio.voice_loopback_start`),
  feed a ramp, then read `audio.voice`: `loopback.max_latency_ms` must stay under 60 ms.
- *Did the game keep up?* `audio.voice` `dropped_frames` above 0 means the game drained the
  capture ring too slowly; drain every frame or raise `buffer_secs`.
