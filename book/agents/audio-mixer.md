# Audio mixer dock and cue editor

The editor's audio tools (`docs/specs/16-audio.md` § 5, ADR-2530): a **mixer dock** with a bus tree, faders,
mute, solo and live meters, a **sound cue editor** for `.cue.ron`, and the runtime read `audio.bus_meters`.
The `.cue.ron` stays version 3; the mixer's bus layout `config/audio.ron` is the `audio-buses` format (v1 adds
snapshots and effect bypass, ADR-8354).

## Meters

Every regular bus carries a meter tap (`regia_audio::bus_meter`), the last effect of its chain. On the audio thread
it reads each block once, with no allocation and no lock, and publishes three figures every 50 ms:

- `peak_db`: the sample peak of the last 50 ms (dBFS),
- `rms_db`: the RMS of both channels over the same 50 ms (dBFS),
- `short_term_lufs`: the K-weighted loudness over the last 3 s (a ring of 60 hops; the same filters and scale as
  `regia_audio::LoudnessMeter`, which also keeps every gating block for the integrated value and so grows without
  bound; the tap does not).

The figures are **post-fader**: the tap hears the bus before kira applies the track volume, so the meter adds the
fader level (the bus blend, ducking and mute included). The fader mirror jumps to the target at once, so during a
fade the reading leads the audible level. A muted bus reads the floor. Silence reads `-inf` internally and
**`-100` dB** (`floor_db`) in every answer.

`audio.bus_meters` (read-only; needs `regia_audio::AudioPlugin`, else `UNSUPPORTED` / `F0013`):

```json
{ "method": "audio.bus_meters" }
→ { "buses": [ { "bus": "Master", "peak_db": -6.02, "rms_db": -9.03, "short_term_lufs": -12.4 },
               { "bus": "Music", "peak_db": -100, "rms_db": -100, "short_term_lufs": -100 } ],
    "floor_db": -100, "hop_ms": 50, "headless": true }
```

`floor: false` makes silence read `null`. Meters need the mixer to run: a headless app reads them after
`Audio::render_offline`.

## Mixer dock

Panel **Audio Mixer** (Window ▸ panels, id `audio_mixer`). One row per bus in tree order (indented under its
parent): a fader (-80 to +6 dB), **M** (mute), **S** (solo) and three meter bars (peak, RMS, LUFS). While you play in
the editor the bars show the play world's mixer; otherwise the edit world's, when it has one (the dock says which:
`Meters: play`).

- **Solo**: with any bus soloed, only soloed buses, their ancestors and their descendants sound; a user mute still
  wins. The dock marks a bus silenced only by a solo with `(solo)`.
- **Undo**: every fader move, mute and solo is one undo step; consecutive moves of one fader are one step. Undo
  restores the previous value (and the play mixer follows at the next editor frame).
- The model starts from the project's `config/audio.ron` bus layout (else the default tree; a file that does not
  load is named in the dock's error line). The dock changes the play session's mixer, and the file only on
  **Save layout** (below).

`editor.audio_mixer_get` (read-only) returns the model and the live meters:

```json
{ "method": "editor.audio_mixer_get" }
→ { "source": "play", "floor_db": -100,
    "buses": [ { "name": "Music", "parent": "Master", "volume_db": 0.0, "muted": false, "solo": false,
                 "effective_muted": false,
                 "meter": { "peak_db": -14.2, "rms_db": -17.3, "short_term_lufs": -19.0 } } ] }
```

`editor.audio_mixer_set` takes `bus` and any of `volume_db` (-80..=6), `mute`, `solo`; it returns the same state
plus `changed` (false when nothing differed: no undo entry) and `undo_depth`.

```json
{ "method": "editor.audio_mixer_set", "params": { "bus": "Music", "volume_db": -6.0 } }
```

## Cue editor

Panel **Cue Editor** (id `cue_editor`). Open a project-relative `.cue.ron`; the panel lists the variations with
weights (edit, add, remove), the pick mode (random, shuffle, sequential), the steal policy (oldest, quietest, none),
the instance limit, the base volume, the volume range, the pitch range, a low-pass filter row (cutoff in Hz with an
octave offset; off = no filter) and a caption row (the Fluent key a play sends, its kind, priority and planned
length), and an **Audition** button. It reads and writes the file through `SoundCueDef::from_ron` and `to_ron`, the
serializer the runtime loader uses.

- **Byte-exact when untouched**: a cue with no edits is never rewritten; Save leaves the file's bytes as they were.
  An edited cue is written atomically with `to_ron`, so a hand-formatted file is reformatted on its first saved edit.
- **Undo**: every change is one undo step; scrubbing one value (a weight, the pitch range, the volume) is one step.
- **Audition** picks a variation the way the cue would (sequential cues walk the list; random and shuffle cues draw
  by weight) and draws the volume and the pitch from the ranges. With `play: true` the picked `.wav` also plays on the
  editor's preview output; the drawn volume and pitch are reported, not applied to the preview.

`editor.cue_edit` (`op` defaults to `get`; `path` opens a cue first):

| `op` | Params | Effect |
|---|---|---|
| `get` | | The open cue. |
| `set` | any of `mode`, `steal`, `no_repeat_last`, `pitch_min`, `pitch_max`, `volume_db`, `volume_db_min`, `volume_db_max`, `max_concurrent` (null = unlimited), `cooldown_ms`, `bus` (null = Master), `filter_cutoff_hz` (Hz, null removes the filter), `filter_offset_octaves`, `caption` (`{ key, kind? (sound \| speech \| narration), priority?, planned_ms? }` or null) | All or nothing: one bad value changes nothing. |
| `add_sound` | `sound`, `weight` (default 1) | Appends a variation. |
| `remove_sound` | `index` | Removes a variation. |
| `set_weight` | `index`, `weight` | Sets a weight. |
| `save` | | Writes the cue; `written` is 0 when it was unedited. |
| `audition` | `play` (default false) | Returns `audition: { index, sound, volume_db, pitch, played }`. |

The answer is `{ open, path, dirty, name, bus, mode, steal, no_repeat_last, pitch: [min, max], volume_db,
volume_range_db: [min, max], max_concurrent, cooldown_ms, filter_cutoff_hz, filter_offset_octaves, caption: { key,
kind, priority, planned_ms }, sounds: [{ sound, weight }], undo_depth }` plus `written`
or `audition` for those ops.

## Errors

Codes are in `data.reason`; `F0002` invalid params unless noted.

| Code | When |
|---|---|
| `audio_mixer_unknown_bus` | `bus` is not in the mixer (the message lists the buses). |
| `audio_mixer_bad_value` | `volume_db` is non-finite or outside -80..=6. |
| `cue_editor_no_project` | No project is open. |
| `cue_editor_bad_path` | `path` is absolute, has `..`, or does not end in `.cue.ron`. |
| `cue_editor_io_failed` | The file cannot be read or written (`F0003` when missing). |
| `cue_editor_parse_failed` | The file is not a valid cue (including one newer than this engine). |
| `cue_editor_no_cue` | No cue is open; pass `path`. |
| `cue_editor_bad_value` | A non-finite or negative weight, an inverted or non-positive range, an empty `sound`, a non-positive or non-finite `filter_cutoff_hz`, a non-finite `filter_offset_octaves`, or an empty `caption.key`. |
| `cue_editor_bad_index` | `index` is past the last variation. |
| `cue_editor_too_many_sounds` | More than 256 variations. |
| `cue_editor_empty` | Audition with no variation of positive weight. |
| `cue_editor_serialize_failed` | The cue cannot be written as RON. |

## Limits

Meters are per bus, not per voice, and send buses have none. The 50 ms hop sets the update rate; the peak is a
sample peak, not a true peak (use `audio.render_check` for dBTP). The cue editor does not play a pitch-shifted or
attenuated preview.

## Effect chains, sends and snapshots

Since `audio-buses` v1 (ADR-8105 (g), ADR-8354; `docs/formats/audio-buses-ron.md`) the dock edits the whole bus
layout, not only the faders.

- **Chain**: the **Chain** toggle of a strip opens its effect chain under it: one row per effect in order (its kind
  and main setting), **Bypass** (the effect keeps its settings and is skipped, so the signal passes that slot
  unchanged), **Move up**, **Move down** and **Remove**; below them an effect-kind list and **Add effect**, which
  appends the kind at a near-neutral setting (0 dB EQ, open filters, a dry reverb or delay, a 1:1 compressor, no
  drive, a dry convolution without an impulse response). A chain holds up to 16 effects.
- **Sends**: one row per send bus of the layout: the send level (-60 dB, silent, to +6 dB), **Pre** and
  **Remove send**, or **Add send** (it starts silent and post-fader) when the strip has no route to it. A send hears
  the bus after its effects. With **Pre** off (post-fader, the default) it also follows the fader and mute; with
  **Pre** on (pre-fader, ADR-8590) it is taken before them, so a reverb or monitor feed keeps its level when the
  fader is pulled to -inf or the bus is muted. Toggling **Pre** is one undo step and rebuilds the bus's tracks;
  snapshots store send levels only and never change a tap.
- **Snapshots**: below the strips, one row per named snapshot: **Apply** (blends every bus to the snapshot over its
  blend time), the blend time (0 to 60 s) and **Delete**; **Release** blends back from the applied snapshot; the
  name field and **Save snapshot** store every fader, mute and send level under that name (replacing a snapshot of
  that name).
- **Save layout** writes the faders, mutes, chains, sends and snapshots to `config/audio.ron` at version 1; the dock
  shows `· unsaved changes` until then. A game built with Regia builds that tree at start-up and can push the saved
  snapshots by name (`Audio::apply_snapshot`).
- **Undo**: every chain, send and snapshot change (save, blend time, delete) is one undo step. Apply and Release
  audition a snapshot on the mixer the dock drives and are not undo steps.
- **Live mixer**: the change reaches the play world's mixer (else the edit world's) at the next editor frame. A send
  level or snapshot change applies in place; a chain change, an added or removed send, a **Pre** toggle or a bypass
  rebuilds the bus tracks, which stops the voices playing at that moment (emitters restart at their next sync).
- **Keyboard**: every control is a focusable widget: Tab moves through the strips (fader, M, S, Chain), the open
  chain (Bypass, Move up, Move down, Remove, the kind list, Add effect, the send rows), the snapshot rows, the name
  field and Save layout; Space or Enter operates the focused one.

`editor.audio_mixer_chain` (mutating, one undo step):

| `op` | Params | Effect |
|---|---|---|
| `add` | `bus`, `kind` (`eq`, `low_pass`, `high_pass`, `reverb`, `delay`, `compressor`, `distortion`, `convolution`), `index` (default the end) | Inserts a new effect. |
| `remove` | `bus`, `index` | Removes the effect. |
| `move` | `bus`, `index`, `to` | Moves the effect to `to`. |
| `bypass` | `bus`, `index`, `bypass` (default true) | Bypasses or re-activates it. |

`editor.audio_mixer_send` takes `bus`, `send` (a send bus) and `volume_db` (-60..=6; adds the route when missing),
`tap` (`pre_fader` or `post_fader`: the **Pre** toggle; with `volume_db` both change in one undo step; a missing route
is added, silent unless `volume_db` is given) or `remove: true`. `editor.audio_mixer_snapshot` takes `op`: `save` (`name`, `blend_secs` 0..=60, default the replaced
snapshot's or 0.5), `blend` (`name`, `blend_secs`), `delete` (`name`), `apply` (`name`) or `release`.
`editor.audio_mixer_save` writes `config/audio.ron` and answers `{ path, version }` plus the state. Every one answers
the `editor.audio_mixer_get` state plus `changed` and `undo_depth`; the state now also has per bus `effects`
(`[{ kind, bypassed, params }]`), `sends` (`[{ bus, volume_db, tap }]`, `tap` `pre_fader` or `post_fader`) and `live_db` (the mixer's level now, the snapshot
blend included), and at the top `send_buses`, `snapshots` (`[{ name, blend_secs, bus_volumes, bus_mutes,
send_volumes }]`), `active_snapshot`, `applied`, `unsaved` and `version`.

```json
{ "method": "editor.audio_mixer_chain", "params": { "bus": "World", "op": "add", "kind": "low_pass" } }
{ "method": "editor.audio_mixer_send", "params": { "bus": "World", "send": "Reverb", "volume_db": -12 } }
{ "method": "editor.audio_mixer_send", "params": { "bus": "World", "send": "Reverb", "tap": "pre_fader" } }
{ "method": "editor.audio_mixer_snapshot", "params": { "op": "save", "name": "Hall", "blend_secs": 0.5 } }
{ "method": "editor.audio_mixer_snapshot", "params": { "op": "apply", "name": "Hall" } }
{ "method": "editor.audio_mixer_save" }
```

Chain, send and snapshot errors (`F0002` invalid params unless noted):

| Code | When |
|---|---|
| `audio_mixer_bad_op` | `op` is not one the method knows (the message lists them). |
| `audio_mixer_unknown_effect` | `kind` is not an effect kind (the message lists them). |
| `audio_mixer_bad_index` | `index` or `to` is past the chain. |
| `audio_mixer_chain_full` | The chain already holds 16 effects. |
| `audio_mixer_unknown_send` | `send` is not a send bus of the layout. |
| `audio_mixer_unknown_snapshot` | No snapshot has that name (the message lists them). |
| `audio_mixer_no_snapshot` | Release with no snapshot applied. |
| `audio_mixer_bad_name` | A snapshot name is blank, longer than 64 bytes or has a control character. |
| `audio_mixer_too_many_snapshots` | The layout already holds 64 snapshots. |
| `audio_mixer_no_project` | Save with no project open. |
| `audio_mixer_bad_layout` | The layout fails validation or cannot be written. |
| `audio_mixer_io_failed` | `config/audio.ron` cannot be written. |

`audio_mixer_bad_value` also covers a send level outside -60..=6, a blend time outside 0..=60 and a `tap` other than
`pre_fader` or `post_fader`.
