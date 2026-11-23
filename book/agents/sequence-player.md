# Sequence players and camera takeover (agents)

A sequence player plays a `.seq.ron` sequence (tracks that animate one property of a named target
each, plus markers) and, optionally, a `.shots.ron` shot list on the same clock. Its tracks move,
show, light and frame things in the scene; its markers tell gameplay when a moment arrives; its
shots take over a camera, blending in from where gameplay had it and back out again. Guide topic:
`agent.guide {"topic": "sequence_player"}`. Formats: `docs/formats/seq-ron.md` and
`docs/formats/shots-ron.md` (neither changes for the player). Decision: ADR-1614.

## The model

- A **player** is an entity with a `SequencePlayer` component (`regia_framework::sequencer`). Several
  can exist at once; each has its own clock.
- The **clock** counts whole nanoseconds. On the `gameplay` clock it advances by the game's time
  step, so `play.pause`, `play.step` and the time scale apply and nothing reads the wall clock;
  sixty 60 Hz steps reach exactly the instant thirty 30 Hz steps reach, so a replay at either rate
  applies the same values and fires the same markers. On the `external` clock the host (render-out)
  writes the time; forward moves behave like steps and backward moves like seeks.
- The player's length is the longer of the sequence and the shot list. At the end, `end` decides:
  `hold` (default) keeps the last frame applied and the camera held; `loop` wraps (each wrap is a
  camera cut); `once` applies the last frame once and lets go. `start_delay` waits that many seconds
  of game time after `play` before the clock starts.
- **Bindings** map a track's `target` name to an entity: by scene guid (survives save, load and
  level moves), by entity id (spawned at runtime), or by name, which binds by guid when the entity
  has one. A target with no binding, or bound to something that does not exist, is reported once in
  `issues` (`unbound_target`) and its tracks are skipped until it binds.
- **Properties** apply through a registry keyed by property path, in track order (a later track on
  the same target and property wins). Built-ins:

  | Path | Channel | Writes |
  |---|---|---|
  | `transform.position` | Vec3 | position, metres |
  | `transform.rotation` | Vec3 | Euler degrees `[pitch x, yaw y, roll z]`, yaw then pitch then roll |
  | `transform.scale` | Vec3 or Float | scale (a Float scales uniformly) |
  | `visible` | Bool | `Visibility::Visible` / `Hidden` |
  | `light.intensity` | Float | point/spot intensity or directional illuminance |
  | `light.color` | Vec3 | the light's linear RGB |
  | `camera.fov` | Float | vertical field of view, degrees |

  A game registers more with `PropertyAppliers::register`. A path with no applier is reported once
  (`unknown_property`); a value the applier refuses (wrong channel kind, missing component) is
  reported once (`apply_failed`). Neither stops the other tracks.
- **Markers** fire as `MarkerReached` ECS messages (`player`, `name`, `time`, `payload`, `cycle`).
  A step covers the half-open interval `(from, to]`; the first step after a start at zero also
  covers zero. Markers of one step arrive by time, then by name; across a loop wrap, the markers up
  to the end come before the new pass's markers at zero. **Seeking never fires markers.** A step
  longer than eight whole loops fires the first eight passes and counts the rest in
  `skipped_cycles`. `SequenceFinished` is sent when a `hold` or `once` player reaches its end.
- **Event tracks** (`.seq.ron` v6, ADR-8105) fire as `SequenceEventFired` ECS messages (`player`, `track`,
  `target`, `event`, `payload`, `time`, `cycle`, `skipped`) by the marker rules: forward play only, once per
  pass of a loop, ordered by time, then track order, then the keys' file order; `skip_to` delivers the keys it
  jumps over with `skipped: true`; a seek, a scrub and a backward external clock write fire nothing. A muted
  (`enabled: false`) event track fires nothing; only the sequence's own event tracks fire, not a
  sub-sequence's. `target` is the track's binding name (empty for a sequence-wide event). A host driving a
  player by hand reads them with `SequencePlayer::take_fired_events` (`FiredEvent`).

  <!-- ignore-reason: a game system's body; the door it opens and the app that schedules it are the game's -->
  ```rust,ignore
  fn on_event(mut events: MessageReader<SequenceEventFired>) {
      for e in events.read() {
          if e.event == "door_open" && !e.skipped {
              // open the door named e.target, e.payload says how
          }
      }
  }
  ```

## Camera takeover

- A player with a shot list **claims** a camera while it is playing, paused or holding its end and
  its clock is inside the shot list: the camera named by `camera`, or the `Camera` with the highest
  `order` (ties to the lowest entity id). `takeover: false` keeps the shots as data. A player
  paused during its start delay has not started, so it shows nothing and claims nothing.
- Of the players claiming one camera, the **most recently started wins**. Where no player has an
  active shot (a shot list shorter than its sequence, a stopped player), control falls back to the
  next claimant, or back to gameplay.
- The pose **blends** from the live gameplay pose over `blend_in` seconds and back over
  `blend_out` (defaults 0.5; `0` cuts): smoothstep on the weight, position and field of view
  linearly, orientation along the shortest arc. There is no jump on the first frame, and the blend
  out ends exactly on the gameplay pose. The blend counts whole nanoseconds of game time and a blend
  out at the end of the shots starts at the instant they end, so a takeover of a camera without a
  rig renders the same poses at 30 and 60 Hz.
- A **camera rig keeps running underneath**: every frame the takeover gives the rig its own pose
  back before the rig updates, so the rig's lag and boom never see the cinematic pose and the blend
  out lands where gameplay is now. A camera without a rig keeps the pose it had when the takeover
  began. While a player holds a camera, other writes to that camera's transform are overridden.
- **Hard changes are camera cuts** (the renderer drops its temporal history,
  [camera cuts](camera-cuts.md)): a shot change, loop wrap or jump of the clock (seek, restart) while
  the shots are on screen, a switch of winning player, a camera claimed again during its blend out,
  and a zero-time blend in or out. A blend is not a cut.
- **Director shakes play on top** of the cinematic pose (the shot player's too): the cinematic pose
  is written before the director runs, and the director shakes every rig and every camera a
  cinematic player drives. Before ADR-1614 a shot pose overwrote every shake.
- The winning shot's **fade** reaches the compositor's fade layer when the `render.sequence` shot
  player has nothing loaded; the shot's **depth of field** is written to the camera's lens while it
  holds, and the gameplay lens settings come back when it lets go.

## Method

`sequence.player { action?, player?, ... }`. `player` names the player entity; every action except
`load` (and the listing `query`) may leave it out when exactly one player exists.

- `query` (default): without `player`, `{ count, players: [state] }` (up to 64); with it, that
  player's `state`.
- `load` with `seq_ron` (the `.seq.ron` text) or `sequence` (the same data as JSON), and/or
  `shots_ron` or `shots`. Without `player` it spawns a player (named `sequence:<name>`) and returns
  its id; with `player` it replaces that player's sequence and/or shot list, keeping its clock.
  Also takes `bindings`, `play` and every setting below.
- `play` (optional `time` or `frame` to start from; from stopped or finished that is a fresh start),
  `pause`, `stop` (back to zero, lets go),
  `seek` with `time` (seconds) or `frame` (at the sequence's `fps`).
- `sample` with `time` or `frame`: every track value and the shot pose at that instant, without
  moving the clock.
- `bind` with `bindings`: `{ "<target>": "<guid>" | <entity id> | { "name": "Hero" } | null }`
  (`null` unbinds). A name matches the ECS `Name` that `scene.spawn` inserts, then the scene
  `Name`; the lowest entity id wins. Every action accepts `bindings`.
- `set`: settings only. Settings (also on `load` and `play`): `end` (`hold`, `loop`, `once`),
  `start_delay` (seconds), `clock` (`gameplay`, `external`), `camera` (entity id, `null` = top
  camera), `blend_in`, `blend_out` (seconds), `takeover` (bool).
- `clear`: despawns the player; a camera it held blends back to gameplay.

The `state` reports `status` (`stopped`, `waiting`, `playing`, `paused`, `finished`), `time`,
`frame`, `duration`, the settings, `delay_left`, `cycle`, `skipped_cycles`, `start_stamp` (larger
started later), `markers_fired`, `shots` (`count`, `duration`, the `active` shot sample,
`shot_changes`), `takeover.held` (`{ camera, weight }` while its shots hold a camera), `bindings`
(every track target and every bound name: `binding`, resolved `entity`, `bound`, `tracks`) and
`issues` (`{ kind, message }`).

Calls land at a frame boundary and the systems apply them later in that frame, so read the applied
values and the camera pose from the next call (or after `play.step`).

```json
{ "method": "sequence.player", "params": {
    "action": "load", "seq_ron": "(version: 1, name: \"intro\", tracks: [...], markers: [...])",
    "shots_ron": "(version: 2, shots: [...])",
    "bindings": { "hero": { "name": "Hero" } }, "end": "once", "blend_in": 0.5, "play": true } }
```

```json
{ "method": "sequence.player", "params": { "action": "sample", "time": 1.0 } }
```

## Errors

Every error carries a lowercase code in `data.code`. Nothing changes when a call is rejected.

| Code | When |
|---|---|
| `bad_action` | `action` is not a string or not one of the actions above. |
| `bad_entity` | `player` or `camera` is not an entity id. |
| `bad_sequence` | The `.seq.ron` text or JSON does not parse or validate, both forms were given, or `load` got neither a sequence nor a shot list. |
| `bad_shots` | The `.shots.ron` text or JSON does not parse or validate, or both forms were given. |
| `bad_setting` | `end`, `clock`, `start_delay`, `blend_in`, `blend_out`, `takeover` or `play` is invalid. |
| `bad_binding` | A `bindings` entry is not a guid string, an entity id, `{ name }` or `null`. |
| `bad_time` | `seek`/`sample` without `time` or `frame`, a negative or non-numeric value, or both given. |
| `too_large` | More than 1024 tracks, 4096 keys on a track or markers, 256 shots, or 1024 bindings in a call. |
| `no_player` | No player exists. |
| `several_players` | More than one player exists and `player` was not given. |
| `unknown_player` | `player` has no `SequencePlayer`. |
| `unknown_entity` | A binding names an entity id that does not exist, or a name no entity has. |
| `bad_name` | `library` or `clip` without a valid `name` (1..=256 bytes) where one is needed. |
| `bad_clip` | `clip` JSON that does not parse, an unreadable or non-clip `asset`, both given, or a negative duration. |

## `.seq.ron` v2: Bezier keys, clips, audio, sub-sequences (ADR-1880)

Version 2 of the format ([`docs/formats/seq-ron.md`](../../docs/formats/seq-ron.md)) adds four things, and
every v1 file still loads (`regia upgrade-project` re-saves it at v2):

- **Bezier keys.** A key with `ease: Bezier` starts a cubic Bezier segment; each key carries a `tangent`
  (`Auto` by default, `Flat`, `Linear`, `Aligned(slope)`, `Broken(in, out)`, slopes in value units per second)
  and optional `weights: Some((in, out))` (handle lengths as fractions of the segment). `Auto` never
  overshoots between monotone keys. Edit them in the editor's [Sequence Curves](seq-curve-panel.md) panel or
  with `editor.seq_curve_edit`.
- **Clip tracks** (`keys: Clip([...])`). `target` binds a rig (`SkinnedMesh`); each section plays a clip of the
  clip library from `start` to `end`, with `offset`, `speed`, `looping`, `blend_in`, `blend_out` and `weight`.
  Overlapping sections crossfade; below a summed weight of one the rig's own pose shows through. A blend does
  not depend on the frame rate, and when the last section ends the rig has its own pose back. On a rig with an
  `Animator` the clip blends into the animator's pose, and ragdoll blending, drivers, springs, gaze, the skin
  and sockets follow it. Register
  clips with `{ "action": "clip", "name": "walk", "clip": <AnimationClip JSON> }` or
  `{ "action": "clip", "name": "walk", "asset": "anim/hero.glb#walk" }` (a cooked clip); `{ "action": "clip" }`
  lists them; `remove: true` drops one. A missing clip is the issue `unknown_clip`.
- **Audio tracks** (`keys: Audio([...])`). Each section plays a sound of the app's `SoundAssets` (by name)
  over `[start, end)`, `offset` seconds in, with `volume_db`, `fade_in`, `fade_out` and `bus`. A player starts
  the sound where its clock is (a seek or a start mid-section stays in sync), restarts it on any jump of the
  clock or an edit of its section, and stops it when it pauses, stops, finishes or despawns or the game is
  paused (it comes back at the clock's offset). A sound runs on the mixer's clock, so the time scale does not
  bend it. A missing sound is `unknown_sound`. The
  state's `voices` counts the player's running voices.
- **Sub-sequence tracks** (`keys: Sub([...])`). Each section plays a sequence of the sequence library at
  `offset + (t − start) × speed`; its targets are renamed through the section's `bindings`
  (`{"lamp": "porch_light"}`) and bound through this player's bindings; its markers fire on this clock as
  `track/marker`. Fill the library with `{ "action": "library", "name": "cine/fx.seq.ron", "seq_ron": "..." }`
  (the name is what the `Sub` section's `sequence` says); `{ "action": "library" }` lists it,
  `remove: true` drops one. Players pick up library changes on the next frame. A missing or cyclic
  sub-sequence is the issue `sub_sequence`.

`sample` now also returns `clips` (each clip track in effect: `blends` with clip, clip time and weight, and
`total_weight`) and `sounds` (each sound in effect: sound, position, rate, fade, time left, bus); values of
sub-sequence tracks carry `depth` 1 or more and their renamed `target`. The state adds `version`,
`track_kinds`, `sub_sequences`, `targets` (every name the player binds, sub-sequences included),
`flat_markers` and `voices`.

```json
{ "method": "sequence.player", "params": { "action": "library", "name": "fx/spark.seq.ron",
  "seq_ron": "(version: 2, tracks: [(name: \"glow\", target: \"lamp\", property: \"light.intensity\", keys: Float([(time: 0.0, value: 0.0, ease: Bezier), (time: 0.5, value: 800.0)]))])" } }
{ "method": "sequence.player", "params": { "action": "load", "play": true,
  "seq_ron": "(version: 2, tracks: [(name: \"fx\", keys: Sub([(sequence: \"fx/spark.seq.ron\", start: 1.0, end: 2.0, bindings: {\"lamp\": \"porch\"})]))])",
  "bindings": { "porch": { "name": "PorchLight" } } } }
{ "method": "sequence.player", "params": { "action": "sample", "time": 1.25 } }
```

## `.seq.ron` v3: spatial sounds, voice time scale, looping sub-sequences, binding renames (ADR-2405)

A v1 or v2 file loads unchanged (only its version number moves; `regia upgrade-project` rewrites
it) and samples exactly as before. v3 adds optional fields, left out of a saved file at their
defaults:

- **Spatial sound sections.** An `Audio` section with `spatial` plays in 3D:

  ```ron
  (sound: "footsteps", start: 1.0, end: 3.5,
      spatial: Some((source: Binding("hero"), min_distance: 1.0, max_distance: 25.0, rolloff: Inverse, spatial_blend: 1.0)))
  (sound: "bell", start: 0.0, end: 4.0, spatial: Some((source: Position((0.0, 2.0, -12.0)), rolloff: Log(6.0))))
  ```

  `Binding(name)` is a binding name like a track `target` (renamed by the sub-sequence sections
  above it, listed in `state.targets`, bound with `bindings`). The voice follows the bound entity's
  world position every frame; an unbound name plays at the listener and shows as an
  `unbound_target` issue. Gain and pan come from the audio emitter math (`regia_audio::spatialize`:
  the section's distances and rolloff, equal-power pan) against the nearest listener. v5 adds Doppler
  (`doppler`) and occlusion (`occlusion`) on spatial sections, see
  [sequence voices v5](sequence-voices-v5.md); sound propagation and binaural rendering do not apply
  to sequence voices.
- **Voice time scale.** `time_scale` on an `Audio` section, in `(0, 16]` (default 1), is sound
  seconds per sequence second. A voice's rate is its `time_scale` times the `speed` of every
  sub-sequence section above it (`sample.sounds[].rate`). Voices follow the rate by **varispeed**:
  at rate 2 the sound plays twice as fast and an octave higher, at 0.5 half as fast and an octave
  lower. A v5 section with `time_scale_mode: PreservePitch` keeps its pitch instead (a WSOLA stretch
  rendered ahead, time scales 0.5 to 2; [sequence voices v5](sequence-voices-v5.md)).
  The game's time scale does not bend a voice (voices run on the mixer clock).
- **Looping sub-sequences.** A `Sub` section may set `repeat: Once` (default), `Count(n)` (1 to
  1,000,000 passes) or `UntilStop` (until the section ends or the player stops), and
  `loop_length` (sub-sequence seconds per pass, at least 0.001; default: the sub-sequence's length
  after `offset`). The sub-sequence time wraps over `[offset, offset + pass)` by an exact
  remainder, so evaluation is a pure function of the time: the instant `t` and `t` plus one pass
  sample the same values. Markers of the sub-sequence fire once per pass (`flat_markers` lists at
  most 65,536). After `Count(n)` passes the end of the last pass holds until the section ends.
  Looping sections nest at most **4** deep (a looping section inside a looping section is depth 2):
  a deeper tree is refused by the library check with `seq_loop_too_deep` (the player reports it as a
  `sub_sequence` issue) and the loop past the limit plays once.

### Methods

```json
{ "method": "sequencer.spatial_voices", "params": {} }
```

(v5 adds `all` and the Doppler, occlusion and stretch fields: [sequence voices v5](sequence-voices-v5.md).)

Returns `{ player, time, voices: [{ track, path, sound, source: { binding } | { position }, emitter,
bound_entity, min_distance, max_distance, rolloff, spatial_blend, rate, voice }] }` for the spatial
sections in effect at the player's time. `voice` is `{ id, volume_db, pan, gain, position }` from the
last audio sync (`gain` is the spatial gain `0..=1`, `volume_db` includes it), or `null` before the
sync ran or without an audio device.

```json
{ "method": "sequencer.sub_loops", "params": { "time": 2.75 } }
```

Returns `{ player, time, sections: [{ path, track, sequence, repeat, count, pass_length, pass,
local_time, depth }] }`: every sub-sequence section in effect at `time` (default: the player's time;
the clock does not move), in tree order. `pass_length` is `null` for a section that plays once.

```json
{ "method": "sequencer.rename_binding", "params": { "from": "hero", "to": "guard" } }
```

Renames one binding everywhere the player's sequence refers to it: keyed and clip track targets,
spatial sources, video section targets (v4), and sub-sequence binding tables (a value that is `from` becomes `to`; a name a
sub-sequence binds without an entry gets an entry `from -> to`, so sub-sequence files are never
edited). The player's binding table entry moves too, so the same entity stays bound. Returns
`{ player, from, to, tracks, sounds, videos, sub_sections, unresolved }`; `unresolved` counts sub-sequence
sections whose sub-sequence is not in the library (only their explicit entries were renamed). There
is no undo through this method; the editor's section strip runs the same rename as one undo step
(`editor.seq_section_edit { "op": "rename_binding" }`, [sequence curves](seq-curve-panel.md)).
`sequence.player` `sample` reports `spatial` and `emitter` for each sound.

### Errors (v3)

| Code | When |
|---|---|
| `seq_loop_too_deep` | `sequencer.sub_loops`: looping sub-sequence sections nest more than 4 deep. |
| `seq_invalid` | `sequencer.sub_loops`: the tree is refused otherwise (a missing sub-sequence, one inside itself). |
| `seq_binding_empty` | `sequencer.rename_binding`: `from` or `to` is empty. |
| `seq_binding_same` | `to` equals `from`. |
| `seq_binding_in_use` | `to` already names a binding of the sequence (the two would merge). |
| `seq_binding_unknown` | nothing in the sequence refers to `from`. |
| `seq_binding_table_in_use` | the player's binding table already binds `to`. |
| `seq_section_out_of_range` | the renamed sequence would not validate (nothing changed). |
| `bad_name` | `from` or `to` is missing. |
| `bad_setting` | `time` is negative or not a number. |
| `bad_entity`, `no_player`, `several_players`, `unknown_player` | as for `sequence.player`. |
| `seq_marker_csv` | `sequencer.marker_sheet`: the CSV is refused. The row number is in the message and in `data.row`. Nothing is written. |

## Subtitles for sound sections (ADR-3334)

A sound section whose sound is the sound of a dialogue line shows that line's subtitle, with no change
to `.seq.ron`. The section's sound name is matched against the variants of the loaded
`DialogueLibrary` (`.dialogue.ron`): the variant's `sound` path, its audio-locale resolution, or its file
stem. The line's cues (inline, else the `<sound>.subs.ron` sidecar of the active text locale) follow the
section's playback position, so the section's `offset`, `time_scale` and the speed and loops of the
sub-sequence sections above it are already in the timing: at a time scale of 0.5 each cue lasts twice as
long. One caption set is queued per running cue through `regia_ui::captions`, so the speaker name, the
board's priority and wrapping and the player's caption options apply, and a **locale change swaps the
text at the next cue**.

- **Scrub, seek:** the cue at the new position shows with its remaining time.
- **Pause:** the cue is held while the player is paused (the audio sync still stops the voice).
- **Loop of a sub-sequence:** each pass shows its cues again.
- **Stop, skip, finish, despawn:** the player's cues are cleared.
- **Render-out:** no subtitles are requested while a render-out renders unless `burn_in` is set
  (off by default), so a rendered video carries them only when the job asks.
- **Opt-out:** a sound track whose name ends in `.nosubs`, a track named in `opt_out_tracks`, or a sound
  named in `opt_out_sounds` shows none.

```json
{ "method": "sequencer.subtitles", "params": {} }
```

Returns `{ player, status, time, cues: [{ path, sound, dialogue, line, speaker, cue, text, locale,
sound_start, sound_end, caption }], settings }`: the cues the player's sound sections show now.
`caption` is the caption set on the board (`null` when the cue set's priority is 0).

```json
{ "method": "sequencer.subtitle_settings", "params": { "burn_in": true, "opt_out_tracks": ["sfx"] } }
```

Sets the given settings (`enabled`, `burn_in`, `opt_out_tracks`, `opt_out_sounds`; each list replaces
the old one) and returns all four. With no parameters it only reads.

| Code | Meaning |
|---|---|
| `bad_setting` | A setting has the wrong type, or a list is too long. |
| `bad_entity`, `no_player`, `several_players`, `unknown_player` | as for `sequence.player`. |

## From Rust

```rust,ignore
use regia_framework::sequencer::{Binding, EndPolicy, SequenceBindings, SequencePlayer, SequencerPlugin};

app.add_plugins(SequencerPlugin);
let mut player = SequencePlayer::new(sequence).with_shots(shots);
player.end = EndPolicy::Once;
player.takeover.blend_in = 0.75;
player.play();
commands.spawn((player, SequenceBindings::default().with("hero", Binding::Guid(hero_guid))));
// Gameplay reacts to markers:
fn on_marker(mut markers: MessageReader<MarkerReached>) {
    for m in markers.read() { /* m.name, m.payload */ }
}
```

## Marker sheet

`sequencer.marker_sheet` (guide topic `sequence_marker_sheet`, ADR-4597) reads or replaces the
player's marker list as CSV. The sheet is an exchange file. `.seq.ron` does not change version,
and an import does not change any track or channel.

Columns, in order, are `time_seconds`, `name`, `payload`. A header row is required. Times are
decimal seconds (a dot), finite, and zero or more. Names are non-empty, and no two markers share
a name. The payload may be empty. A field that contains a comma, a quote, a line break, or
leading or trailing space is quoted, and a quote inside it is doubled.

```json
{ "method": "sequencer.marker_sheet", "params": { "op": "export" } }
```

`export` returns `{ player, op, count, csv, markers: [{ time_seconds, name, payload }] }`. Marker
order is the order stored on the sequence.

```json
{ "method": "sequencer.marker_sheet", "params": { "op": "import", "csv": "time_seconds,name,payload\n0,cue,hello\n" } }
```

`import` replaces the marker list and nothing else. The returned `csv` is the sheet written back
from the markers that were applied. A bad row, a repeated name, a negative or non-finite time, an
empty name, or an unknown column answers `seq_marker_csv` (the row is in the message and in
`data.row`) and writes nothing. A wrong header is row 1.

`player` is optional when exactly one sequence player exists. `op` must be `export` or `import`.
`import` requires a string `csv`. Errors: `bad_entity`, `bad_setting`, `no_player`,
`seq_marker_csv`, `several_players`, `unknown_player`. `errors.explain` gives the long form of
`seq_marker_csv`.

From Rust the same exchange is `regia_framework::sequencer::marker_sheet`: `markers_to_csv`,
`csv_to_markers` and `apply_marker_csv`.

## Not done yet

- A spec-35 camera runner does not exist yet; when it lands, the takeover becomes a push onto its
  transient stack with the same blend.
- No sound propagation or binaural rendering for sequence voices (Doppler, occlusion and the
  pitch-preserving time scale arrived with v5, ADR-3197).
- The editor has a curve panel for keyed tracks and a section strip for clip, audio and sub-sequence
  sections ([sequence curves](seq-curve-panel.md)).

## Streaming prefetch lifecycle

With `LevelStreaming` enabled and `StreamPrefetch.seconds > 0`, each playing sequence with a shot
list has one look-ahead streaming source; the standalone shot player has its own source. Pausing,
stopping or despawning a player removes only its source at the next `PostUpdate`. Resuming recreates
it. Disabling or removing `LevelStreaming`, or setting the prefetch seconds to zero, removes every
prefetch source; restoring the settings recreates sources for the players still playing.

Changes to `StreamPrefetch.seconds` and `StreamPrefetch.priority` apply to existing sources on the
next `PostUpdate`, without replacing their entities. The point is sampled from the player's current
clock plus the look-ahead time, including across a camera cut. Its local position subtracts the
current `OriginOffset` once, so changing the origin while a player is paused is reflected when it
resumes.

## Video sections

`.seq.ron` v4 ([format](../../docs/formats/seq-ron.md), ADR-2862) adds `Video` tracks: a section shows a
source on the clock, so a movie or a screen in the world stays in sync with the sequence.

```ron
(name: "movies", keys: Video([
    (video: "movies/logo.video.ron", start: 0.0, end: 1.0),
    (video: "movies/news.webm", start: 2.0, end: 6.0, offset: 1.5, volume_db: -6.0, looping: true,
        target: Screen(binding: "props/tv", slot: Emissive)),
])),
```

| Field | Meaning |
|---|---|
| `video` | The source: a project path of a `.video.ron`, `.webm` or `.mkv`, through `regia_media::MediaFiles`. |
| `start`, `end` | Sequence seconds. |
| `offset` | Seconds into the source at `start`. |
| `target` | Where the picture goes (below; default a fullscreen layer). |
| `volume_db` | The source's own sound: at most 24, -80 or less is silent. |
| `looping` | Wrap over the source's length. Off: the last picture holds until `end`. |

| Target | Shows on | Needs |
|---|---|---|
| `Fullscreen(layer, fit)` | The whole view; a negative `layer` under the UI. `fit`: `Letterbox`, `Fill`, `Stretch`. | `VideoFramePlugin` (and `VideoOverlayPlugin` above the UI) |
| `Player(binding)` | The bound entity's own `VideoPlayer`, taken over and put back when the section ends. The host decides what shows its picture. | nothing more |
| `Screen(binding, slot)` | The albedo or emissive slot of the bound mesh's material, put back when the section ends. | `VideoFramePlugin` |
| `UiImage(binding)` | The bound UI node, its image put back when the section ends. | `VideoFramePlugin`, `UiVideoPlugin` |

A build needs the `media` feature (`regia/media`, plus `media-codecs` for AV1, Opus and Vorbis) and the app
`regia_media::MediaPlugin`. Video as an arbitrary material graph texture is not available: only the two
slots above.

### Sync

- **Picture.** The source time at sequence time `t` is `offset + (t - start)`, at the speed of any sub-sequence
  section above it, wrapped when looping. The picture shown is the last one at or before it (half a
  millisecond of slack covers WebM's rounded timestamps). The player's clock is slaved: the sequence writes
  it every frame, on the frame the picture is published.
- **Seek and scrub.** A time behind the picture, or more than a second ahead of it, restarts the source at the
  previous keyframe and decodes forward: the same rule as linear play, so a seek shows what linear play
  shows. A paused player's sections stay open and follow a scrub with their sound held.
- **Drift correction.** In real time decoding runs on its own thread and never blocks the frame, so a picture can
  run late. Past 0.25 s behind, or with the source's audio past 0.1 s from the clock (a time scale, a hitch),
  the session repositions, at most once per second of source time. `drift` and `audio_drift` report it.
- **Frame-exact.** Under a manual clock (`render.out`, `render.out_job`, `play.step`, headless tests) or with
  `SequenceVideos::force_exact`, sessions decode on the calling thread and every frame blocks until its
  exact picture is decoded: no drops, nothing read from the wall clock, the same pictures on every run. A
  frame decodes at most `SequenceVideos::exact_budget` pictures (20,000); past that the issue
  `seq_video_failed` with `seek_bound` says so and the next frame goes on. A render job needs a `bindings`
  table to play its own sequence, as for any job.

### Method

```json
{ "method": "sequencer.video_sections", "params": {} }
```

Returns `{ player, time, frame_exact, opened, closed, sections, active, issues }`.

- `sections`: the player's own tracks' sections: `{ track, index, video, start, end, offset, target: { kind,
  binding, layer, fit, slot }, volume_db, looping, in_effect }`.
- `active`: one per section in effect, sub-sequences included: `{ track, path, video, target, media, state,
  source_time, frame_shown, picture_time, drift, audio_drift, certain, exact, seeks, resyncs, frames_dropped,
  error }`. `media` is the session's `VideoPlayer` entity, so `video.status` and `video.frame_hash` work on it.
  `frame_shown` is the picture's index at the source's nominal rate. `drift` is how far the picture is behind
  the clock past its own frame period, in seconds (0: the right picture). `certain`: the picture is known to
  be the one of `source_time`.
- `issues`: `{ kind, message }`, the sequence's video problems.

To check a trailer: `render.out_job`, then read each frame's picture hash, or under `play.step` compare
`frame_shown` with `source_time` per step.

### Issues and errors

Issues are reported once on the player (also in `sequence.player` `query`), and the rest of the sequence plays:

| Kind | When |
|---|---|
| `seq_video_missing` | The source does not exist. The section shows nothing. |
| `seq_video_failed` | The source did not open or stopped decoding (`message` names the media code, like `codec_unsupported` or `limit_exceeded`), or `seek_bound` (a frame-exact seek outran its budget). |
| `seq_video_unavailable` | No `media` feature, no `MediaPlugin`, or no plugin the target needs. |

The method's errors are the ones of `sequencer.spatial_voices`: `bad_entity`, `no_player`, `several_players`,
`unknown_player`. In the editor, the [section strip](seq-curve-panel.md) shows and edits video sections.


## Spawnable bindings (`.seq.ron` v5, ADR-2948)

A spawnable is a binding the player creates from a prefab instead of finding an existing entity:

```ron
spawnables: [
    (binding: "hero", prefab: "actors/hero.prefab.ron", start: 0.5, end: Some(1.5)),
    (binding: "crate", prefab: "guid:0000000000000000000000000000beef", start: 2.0, keep_on_stop: true),
],
restore_on_stop: true,
```

- **Pre-roll.** `play` on a sequence with spawnables sets the status to `waiting` with `prerolling:
  true`: nothing is applied and the clock and the start delay hold until every prefab is in the
  scene's prefab library. A project `.prefab.ron` (with its `.meta` guid, or one hashed from its
  path) is read and baked on a worker thread in real time, and on the calling thread under manual
  time (render-out, `play.step`, headless tests), on an external clock, or with
  `SequenceSpawnables::force_sync`, so those runs start on the same frame every time. A `guid:`
  prefab must already be registered.
- **Life.** Each frame after the clocks advance, an actor whose range holds the clock is spawned (a
  wrapper entity with the binding as its `Name`, a `Transform`, `SpawnedBySequence`, and the
  prefab's entities under it) and one whose range does not is despawned, so tracks targeting the
  binding animate it from its first frame. The wrapper's guid is a hash of the player's guid (or
  the sequence name), the binding and a generation that moves only past a guid already taken; spawns
  run in the file's order, despawns in binding order, players in entity order. A render-out job and
  a repeated run create the same entities with the same guids.
- **Stop.** A stop, a `once` player letting go, or a despawned player despawns its actors, except
  `keep_on_stop` ones, which stay as the game's (the `SpawnedBySequence` marker is removed). A seek
  out of a range despawns even a kept actor.
- **Failures.** A prefab that does not load or instantiate is reported once as issue
  `seq_spawn_failed` with a code: `prefab_missing`, `prefab_read`, `prefab_parse`, `prefab_bake`,
  `prefab_not_registered` or `prefab_spawn`. Its binding stays unbound and its tracks are skipped
  (never reported as `unbound_target`); the rest of the sequence plays. A new `play` retries.
- **Skip.** `sequence.player { "action": "skip", "time"? }` (`SequencePlayer::skip_to`,
  `skip_to_end`) jumps forward like a seek but delivers every marker in between, in playback
  order, with `skipped: true` on `MarkerReached`; at the end the player finishes as playback would.
  During a pre-roll the skip waits for it. [Cutscenes](cutscene.md) skip this way.
- **Restore.** With `restore_on_stop` (the file's, or `sequence.player { "action": "set",
  "restore_on_stop": true }`), each keyed track's property is read before the player first writes
  it (through the take recorder's `PropertyReaders`, or a `reflect:` path) and written back when the
  player stops or lets go, last write first.

`sequence.player` states add `prerolling`, `restore_on_stop`, `markers_skipped` and
`spawnables: { list: [{ binding, prefab, start, end, keep_on_stop, live, entity, guid, load,
load_code }], spawned, despawned, kept, loads, load_failures }`. The `SpawnableChanged` message
reports each spawn and despawn. In the editor, the section strip's **Binding kind** row switches a
binding between Bound and Spawnable and picks its prefab, range and `keep_on_stop`
(`editor.seq_section_edit { "op": "binding_kind", "binding", "kind": "bound" | "spawnable",
"prefab"?, "start"?, "end"?, "keep_on_stop"? }`, one undo entry, error `seq_spawnable_invalid`).
