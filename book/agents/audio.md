# Audio: emitters, listeners, reverb, propagation

Inspect and tune entity sound without code. Both methods need `regia_audio::AudioPlugin`
(`regia::DefaultPlugins` installs it); without it they return `UNSUPPORTED` (`F0013`).

## List what is playing

`audio.list_emitters` (read-only) returns every `AudioEmitter` with its configuration and live
state, the listeners, the registered sound and cue names and the reverb-zone mix.

```json
{}
→ { "emitters": [{ "entity": 4294967301, "sound": "generator_hum", "cue": null, "autoplay": true,
      "looping": true, "volume_db": 0.0, "min_distance": 2.0, "max_distance": 40.0,
      "bus": "World", "occlusion": true, "voices": 1, "occlusion_amount": 0.62,
      "binaural": false, "hrtf_rate": null, "position": [4.0, 0.0, -10.0], … }],
    "listeners": [{ "entity": 4294967298, "position": [0, 1.7, 0], "forward": [0, 0, -1],
      "velocity": [0, 0, 0], "hrtf": false, "hrtf_rate": null }],
    "sounds": ["generator_hum", "rain"], "cues": ["footsteps", "impacts"],
    "reverb": { "weight": 1.0, "send_db": -4.0, … },
    "real_voices": 3, "virtual_voices": 0, "rejected_values": 0,
    "streams": { "playing": 1, "starved": 0, "underruns": 0, "starved_secs": 0.0 } }
```

`cue` is the name of the `SoundCue` the emitter plays (one of its variations, picked when it starts) when
no `sound` is set, else `null` (spec 16 § 3). `cues` lists the registered cue names. The buses
`audio.mix` reports are the tree a project's `config/audio.ron` defines (spec 16 § 1), falling back
to the default tree when the file is absent or invalid.

`hrtf_rate` is the sample rate of the HRIR table binaural voices convolve with: on an emitter,
its own track's table (`null` when it is not binaural); on a listener with `hrtf` on, the table
rate of the binaural voices (they share the one mixer; `null` for speakers or before a binaural
voice runs). It equals the mixer's rate (44.1, 48, 96 kHz, …): tables are built per mixer rate
on first need, at most six per sphere, so the interaural delay keeps its length in time on a
96 kHz device (ADR-7461). A value different from the device rate means the table set was full
and the nearest table is in use (the log says `HRIR tables are full` once).

`voices` 0 on an autoplaying emitter means its sound is not registered (check `sounds`) or it is
a one-shot that finished. `occlusion_amount` is the smoothed value the mix uses (0 clear, 1 fully
behind walls). `reverb` is `null` when the level has no reverb zones.

`rejected_values` counts the values the mixer refused since the process started: NaN or infinite
volumes, playback rates, pans and filter cutoffs (replaced by silence, the original speed, the
centre and an open filter), and loop regions kira cannot wrap (zero length, inverted or past the
end: the whole sound loops instead). It never goes down. If it grows across a `play.step`, a
transform, an emitter field, an occlusion tuning or a tool call is producing bad numbers: an
emitter at a NaN position is silent (its voice goes virtual) until it has a real place again. The
first refusal is also logged (`audio refused a bad …`).

`streams` watches the streaming voices and music stems: `playing` streams now, `starved` ones (held
silent because their decoder thread is behind), `underruns` since the start (one per starved
stretch of one stream) and `starved_secs` (rendered time spent starved, summed). A stream counts as
starved after it had been playing and stood still for 0.1 s of rendered audio; paused streams, a
headless run that does not render, and a non-looping stream at its end do not count. Underruns on a
device mean slow storage or a starved machine (the music plays late after each one); in an offline
render they mean the render outran the decoder: decode the sound with `SoundAsset::decode_to_memory`
for a reproducible render ([Audio](../user-guide/audio.md)).

## Change an emitter

`audio.set_emitter` (`EditScene`) sets the given fields on `entity`'s emitter; omitted fields keep
their value. An entity without an emitter gets a default one first (`created: true`). A sound
with `autoplay` starts on the next frame; changing `sound` restarts it; `""` clears it.

```json
{ "entity": 4294967301, "sound": "rain", "volume_db": -6, "max_distance": 25 }
→ { "created": false, "before": { … }, "after": { "sound": "rain", "volume_db": -6.0, … } }
```

Fields: `sound`, `cue`, `autoplay`, `looping`, `volume_db` (−100..=24), `min_distance`, `max_distance`
(≥ `min_distance`), `fade_start` (0..1, a fraction of `max_distance` where the inverse and log curves
begin a smooth fade to 0; `null` for none), `spatial_blend` (0..=1), `cone_inner_degrees`, `cone_outer_degrees`
(0..=360), `cone_outer_gain` (0..=1), `air_absorption`, `doppler_factor` (0..=10),
`occlusion`, `bus`. `cue` names a registered sound cue: the emitter picks one of its variations on
start instead of repeating one fixed sound (a looping emitter loops that variation; `looping: false`
plays it once, like a sound). A `sound` and a `cue` are mutually exclusive: setting one clears the
other, and naming both in one call returns `INVALID_PARAMS`. Unknown sounds or cues return `NOT_FOUND` listing the
registered names; out-of-range numbers and unknown buses return `INVALID_PARAMS` and change nothing.

A one-shot cue at a position with no entity is game code: `Audio::play_cue_at(cue, position, emitter)`
plays it there through the medium, and `Audio::play_cue_on(cue, entity, emitter)` attaches it to an
entity's emitter so it follows (spec 16 § 3). Both apply the cue's authored low-pass, as `play_cue` and a
cue emitter do; on an attached voice the emitter's occlusion muffling stacks on top.

## Verify by ear, offline

Pair `play.step` with `audio.list_emitters`: after a step, `voices` and `occlusion_amount` show
whether the emitter is heard and how muffled. Reverb zones and listener `hrtf` are components:
edit them with `scene.add_component` and `scene.set_property` (`AudioReverbZone`, `AudioListener.hrtf`), or use
`audio.set_zone` for tunnels and mines ([Audio zones](audio-zones.md)).

Audio that cuts out and restarts its loops is the engine's output gate. Grep the log for
`audio output paused`: it says whether every window was hidden or N slow frames in a row tripped
the throttle. The slow-frame pause follows the reflected `AudioThrottle` resource: `Auto` (the
default) arms it only on a software adapter (lavapipe), `On` always, `Off` never.
`Audio::throttle_suspend_count()` counts those pauses.

## Propagation media

Sound in Regia Engine travels through the level's **medium** (`docs/specs/16-audio.md` § 3c). Two
read-only methods let an agent check what a player would hear, and why.

### `audio.medium`

```json
{ "method": "audio.medium" }
→ { "kind": "airless", "airborne": false, "speed_of_sound": 0.0, "level_offset_db": -100.0,
    "ground": { "name": "ice", "wave_speed_mps": 3800.0, "q": 200.0, "coda_s": 4.4 },
    "listener": { "contact": "boots", "coupling_weight": 1.0, "vehicle": null, "structure": [] },
    "interiors": 1, "haptics": { "strength": 0.0, "last_event_strength": 0.31, "events_fired": 2, "pending": 0 },
    "path_voices": { "airborne": 0, "ground": 1 } }
```

- `kind`: `earth` (343 m/s), `mars` (240 m/s, −20 dB, strong high-frequency loss) or `airless`
  (no airborne path). It follows `LevelSettings.atmosphere.preset`, refined by
  `LevelSettings.audio_medium` (scene format v6).
- `listener.contact`: `boots`, `vehicle`, `structure` or `none` (jumping, flying, floating), from
  the listener's `ListenerCoupling` component. `coupling_weight` is the 50 ms fade of the ground
  path after leaving the ground.
- `haptics`: the rumble the ground path sends to the gamepads (through `regia_input`).

### `audio.propagation`

```json
{ "method": "audio.propagation", "params": { "position": [500, 0, 0], "category": "explosion" } }
→ { "paths": [ { "path": "ground", "delay_ms": 131.6, "gain_db": -31.9, "cutoff_hz": 167.7,
                 "coupling_cutoff_hz": 500.0, "coda_s": 4.4, "felt": 0.13, "audible": true } ],
    "bypass": false, "air": null, "ground_material": "ice", "coupled": true, "distance": 500.0,
    "audible": true }
```

Pass `entity` (an emitter with `AudioEmitter`; its `AudioPropagation` is used) or a hypothetical
`position` with a `category` (`impact`, `explosion`, `collapse`, `footstep`, `landing`,
`machinery`, `hull_landing`, `hull_scrape`, `hull_hover`, `planted_weapon`, `projectile`, `radio`,
`ui`, `cockpit`, `body`). Each path is:

- `airborne`: through the air (or a pressurised interior's own air, `air: "interior"`), delay
  `r / c`; through a wall it is muffled (`coupling_cutoff_hz` 400).
- `ground`: through the ground, delay `r / v`, low-passed by the material's Q (`absorption_cutoff_hz`)
  and by the listener's boots/hull/floor (`coupling_cutoff_hz`), with a ringing coda (`coda_s`).
- `direct`: the medium is bypassed (radio and UI buses, the listener's own body, the cockpit of the
  vehicle the listener sits in).

### Propagation recipes

- *Airless pilot jumping hears nothing*: set the listener's `ListenerCoupling.contact` to `None`
  (`scene.set_property`), `play.step` 4 frames, then `audio.propagation` → `paths: []`.
- *An explosion is felt before it is heard (Earth)*: the `ground` path's `delay_ms` is smaller than
  the `airborne` one, and `audio.medium.haptics.events_fired` increments when it arrives.
- *Mars is darker*: compare `cutoff_hz` and `gain_db` of the same `position` after switching the
  level's preset with `scene.set_property` on `LevelSettings.atmosphere.preset`.

## Procedural loops

A `.loopgen.ron` (`docs/formats/loopgen-ron.md`) renders a seamless loop from a noise and
filter graph. `audio.loopgen_render` renders one without playing it, so an agent can check a
file before a patch uses it. It needs the `play` permission and counts as a mutating call: with
`wav: true` it writes a file (ADR-3285), so a `read_only` client cannot reach it.

```json
{ "method": "audio.loopgen_render", "params": { "path": "audio/wind.loopgen.ron", "wav": true } }
→ { "name": "wind", "sample_rate": 48000, "channels": 1, "cached": false,
    "content_hash": "9c1f0e52a7d34b18",
    "stats": { "frames": 144000, "duration_secs": 3.0, "rms": 0.21, "peak": 0.64,
               "seam_jump": 0.0021, "max_step": 0.0187, "hash": "5be0a1f3c2d49e77" },
    "wav": "/tmp/regia_loopgen/wind-5be0a1f3c2d49e77.wav" }
```

Give exactly one of `ron` (inline text) or `path` (project-relative, ending `.loopgen.ron`, no
`..`). `wav: true` also writes a 16-bit WAV to `out_dir` (default `<temp>/regia_loopgen` on
desktop, the app cache dir's `regia_loopgen` on Android, where the system temp dir is
shell-owned; the file name carries the sample hash). `out_dir` must be relative to the project, or absolute inside the project, the
system temp directory or an `output_roots` entry of `.regia/agent.toml`, with no `..` and no symlink that leads out of them (checked on the deepest existing
directory, ADR-1816); anything else is `invalid_params` (`data.reason` `path_outside_project`). It works without the audio plugin; with it, the render
goes through the `SoundAssets` cache, and `cached: true` means the same file text had already
been rendered.

How to read the result:

- `hash` is the FNV-1a hash of the samples: the same file gives the same hash on every
  platform; a different `seed` gives a different one.
- `seam_jump` is `|first - last|` sample (the step when the loop wraps) and `max_step` the
  largest step inside the loop. A seamless loop has `seam_jump <= 1.1 x max_step + 0.0002`. A
  bigger value means `crossfade_secs` is 0 or too short for the content.
- `rms` near 0 is a silent graph (a mix with all gains 0, a `Gain` of -96 dB).

**Errors.** A bad file returns `INVALID_PARAMS` (`F0002`) whose `data.key_path` names the
offending value, e.g. `nodes[1].node.Filter.cutoff_hz` (above Nyquist), `output` (unknown id),
`nodes[0].id` (a cycle), `version` (newer than this engine) or `(file)` (RON syntax). Missing
or non-`.loopgen.ron` paths return `INVALID_PARAMS` / `NOT_FOUND` (`F0003`). No new error codes.

`audio.list_emitters` gains a `loopgens` object: `registered` (sound names made from a loop
generator, with the file's `content_hash`), `loops` (the cached loops with the same `stats`)
and `cache` (`entries`, `hits`, `misses`).

**Verify a patch layer by ear, offline.** Build the patch with `from_def_with_loopgen`, then
`play.step` a few frames and read `audio.list_emitters`; for a layer under test render the
loop with `audio.loopgen_render` and check `rms`, `peak` and `seam_jump` before listening.

## Cue points stored in a WAV file

`audio.cues` reads the cue points already stored in a WAV file: the RIFF `cue ` chunk and the
names in a `LIST` / `adtl` / `labl` list. These points are not a sound cue. A sound cue picks a
variation when it plays. This method does not play audio, does not decode PCM, does not
resample, and does not need the audio plugin.

```json
{ "path": "audio/marks.wav" }
→ { "count": 2,
    "cues": [
      { "order": 0, "id": 10, "sample_offset": 100, "name": "intro" },
      { "order": 1, "id": 11, "sample_offset": 200, "name": "loop" } ] }
```

`path` is project-relative. `..` and a symlink that leaves the project are `F0002`. A missing
file is `F0003`.

- `sample_offset` is `dwSampleOffset` as stored, in the file's own sample layout.
- `order` is the zero-based index of the point in file order across every `cue ` chunk. It is
  not `dwPosition`.
- `id` is `dwName`. Two points may share an id; both rows are returned.
- `name` is the first `labl` with that id, or null when none matches. An empty label is `""`.
  Label bytes are Latin-1: each byte is the Unicode scalar U+0000 through U+00FF, cut at the
  first NUL. The playback decoder does not read these strings.

A file with no `cue ` chunk returns `count` 0 and an empty `cues` list. That is not an error.
A cue chunk that claims more points than its bytes hold, and a label list that is cut short,
return `wav_cue_truncated` and no partial list.

**Errors.** `wav_cue_not_riff` when the file does not start with `RIFF`. `wav_cue_not_wave`
when the RIFF form is not `WAVE`. `wav_cue_truncated` when a cue or label chunk overruns.
`wav_cue_unreadable` when the path is not a regular file, the read fails, or the file is above
256 MiB. `F0003` when the file or project root cannot be resolved. `F0002` when `path` is
missing, not a string, or escapes the project.

Binding these points to a sequence is not part of this method.

