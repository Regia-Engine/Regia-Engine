# Music and the listener (agents)

Read the music system, move between tracks, fire stingers and find out where the ears are
(ADR-2517, `docs/specs/16-audio.md` § 3 and § 3a). All four methods need
`regia_audio::AudioPlugin` (`regia::DefaultPlugins` installs it); without it they return
`UNSUPPORTED` (`F0013`).

Musical time runs on a clock inside the mixer, not on the frame clock. A stinger or a transition
starts on an exact sample whatever the frame rate, and gameplay gets a `MusicBeat` message for every
beat.

## Read the music state

`audio.music_state` (read-only).

```json
{ "method": "audio.music_state" }
→ { "track": "combat", "bpm": 120.0, "beats_per_bar": 4, "segment_beats": 32,
    "clock_secs": 3.25, "clock_frame": 156000, "sample_rate": 48000,
    "beat_index": 6, "bar": 1, "beat": 2, "intensity": 0.5,
    "stems": [{ "name": "drums", "target_db": 0.0 }],
    "pending": { "to": "calm", "sync": "next_bar", "switch_secs": 4.0, "start_secs": 4.0,
                 "crossfade_secs": 2.0, "segment": false },
    "stingers_playing": [{ "name": "victory", "at_secs": 4.0 }],
    "crossfading": false, "stingers": ["victory"], "tracks": ["calm", "combat"],
    "late_starts": 0 }
```

- `clock_secs` and `clock_frame` are the music clock (`null` when none runs). This thread reads it
  up to one output callback behind the mixer. Frame 0 is where the track that started the clock
  began.
- `bar`, `beat` and `beat_index` count from 0, on the playing track's grid, from that track's start.
- `tracks` lists what `audio.music_transition` can reach. A host registers tracks with
  `Audio::register_music`.
- `late_starts` counts scheduled sounds that reached the mixer after their start time and started
  late. Anything but 0 means this output's callback is longer than the schedule lead
  (`MUSIC_SCHEDULE_LEAD_SECS`, 50 ms).

## Move to another track

`audio.music_transition` (`play` permission) follows the playing track's rule for `to` (its own,
else its `*` rule, else next bar with no crossfade). `sync` and `crossfade_secs` override the rule
for this call.

```json
{ "method": "audio.music_transition",
  "params": { "to": "calm", "sync": "next_bar", "crossfade_secs": 2.0 } }
→ { "name": "calm", "sync": "next_bar", "at_secs": 4.0, "at_frame": 192000, "bar": 2, "beat": 0,
    "noop": false }
```

- `sync` is `immediate` (after the 50 ms lead), `next_beat`, `next_bar` or `end_of_segment`. A
  boundary closer than the lead goes to the next one.
- The crossfade is equal power and starts at the switch. With no crossfade, the old track still
  fades over 10 ms so it does not click, and the new one starts on the boundary sample.
- A rule with a segment plays that sound at the switch and starts the next track, at full level,
  when it ends.
- Asking for what is already playing, or already on its way, returns `noop: true` and changes
  nothing. Asking for the playing track while another transition waits takes that transition back.
  A request after the switch waits for the next track and is applied once it plays.
- With no track playing the track simply starts.

Errors: `music_unknown_track` (`NOT_FOUND`, the hint lists the tracks), `music_start_failed`
(`INTERNAL_ERROR`, the mixer refused a sound: see `audio.mix` and `logs.read`).

## Fire a stinger

`audio.music_stinger` (`play` permission) plays a one-shot of the playing track on its own sync, or
on `sync`.

```json
{ "method": "audio.music_stinger", "params": { "name": "victory", "sync": "next_beat" } }
→ { "name": "victory", "sync": "next_beat", "at_secs": 3.5, "at_frame": 168000, "bar": 1, "beat": 3,
    "noop": false }
```

Errors: `music_not_playing` (`INVALID_PARAMS`), `music_unknown_stinger` (`NOT_FOUND`, the hint lists
the track's stingers), `music_start_failed`.

## Where are the ears?

`audio.listener_info` (read-only).

```json
{ "method": "audio.listener_info" }
→ { "source": "camera", "follow": "active_camera",
    "camera": { "entity": 4294967298, "position": [0.0, 1.7, 0.0] },
    "listeners": [{ "entity": 4294967298, "position": [0.0, 1.7, 0.0], "forward": [0.0, 0.0, -1.0],
                    "right": [1.0, 0.0, 0.0], "up": [0.0, 1.0, 0.0], "velocity": [0.0, 0.0, 0.0],
                    "hrtf": false }] }
```

`source` is `component` (an entity has `AudioListener`, which always wins), `camera` (the ears
follow the active camera) or `origin` (no listener and no camera, or `AudioListenerFollow::Off`:
the origin, facing −Z). The active camera is the lowest `order` drawing to the window. A second
listener is a split-screen player; each emitter is heard by the nearest.

## How to check it

1. `audio.music_state` before and after `audio.music_transition`: `pending` appears, then `track`
   changes and `pending` clears once the clock passes `start_secs`.
2. `audio.render_check` on a headless app hears the result in the offline mixer.
3. `late_starts` stays 0.
