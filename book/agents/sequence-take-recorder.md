# Take recorder

The take recorder turns play into a sequence. Pick what to record (an entity by scene guid, an
entity id or name, or whichever camera is rendering) and which channels of each, play the game or
PIE, stop, and the recording becomes a `.seq.ron` v3 take: keyed tracks that replay what happened,
with the keys reduced so a still object writes two keys instead of one per frame. Each take is
written as `<name>_take<N>.seq.ron` next to its source sequence, and the source gains a
sub-sequence track that plays it, so recording into an existing sequence adds a section and never
overwrites anything. The format does not change: a take uses the track kinds that already exist.
Guide topic: `agent.guide {"topic": "sequence_take_recorder"}`. Decision: ADR-2637. Players:
[Sequence players and camera takeover](sequence-player.md).

## What is recorded

Each **binding** has a `name` (the target name its tracks get; bind the same name to play the take
back) and one target:

- `guid`: the entity carrying that scene guid (survives save, load and level moves);
- `entity`: an entity id (runtime entities);
- `entity_name`: the entity with that `Name` (recorded by its guid when it has one);
- `camera: true`: the active camera, which is the one with the highest `order` (ties to the lowest
  entity id). If a different camera becomes active, that counts as a camera cut.

and a list of **channels**:

| Channel | Tracks (`<name>/<property>`) |
|---|---|
| `transform` (default) | `transform.position` (world metres for a root entity, as the player applies it; parent-relative for a child), `transform.rotation` (Euler degrees `[pitch, yaw, roll]`, unwrapped so a turn past 180 degrees keeps going), `transform.scale` |
| `camera` (default for `camera: true`) | `transform.position`, `transform.rotation`, `camera.fov` (degrees). A **cut** steps the key before it (`Ease::Step`) and adds a marker `<name>/cut_<k>` with payload `camera_cut`, so playback cuts instead of blending. Something counts as a cut when another camera becomes active, when the sequence holding the camera changes shot, or when the camera jumps further than `cut_distance` metres in one frame. |
| `animation` | `<name>/clip`: a `Clip` track of sections. Each section holds the clip the binding's graph animator weights most and the clip time it was at. A new section starts when the clip changes, or when the clip time leaves the predicted line by more than `clip_time` seconds (a wrap or a restart). Clip names must be in the player's clip library to play back. Single-clip animators report no clip; this shows up in `issues`. |
| `property:<path>` | One track per path: a reader path (`visible`, `light.intensity`, `light.color`, `camera.fov`, `transform.*`, or one a host registered in `PropertyReaders`), or a reflected field `reflect:<Component>.<field.path>` (`f32`, `f64`, `bool`, `Vec3`, `[f32; 3]`). The player applies `reflect:` paths through reflection, so they play back with no applier to register. |

With `audio_cues: true` the take also records **audio cues**: each `TakeCue` message gameplay sends
(the sound's name, its length, volume, bus and optional 3D source) becomes an `Audio` section at
the moment it was sent. The recorder cannot hear the mixer, so it records only cues that gameplay
announces this way.

**Clock.** The recorder samples once per gameplay frame on the same whole-nanosecond clock a
sequence player runs on. It samples in `PostUpdate`, after the sequencer has applied its tracks
and physics has synced its bodies, and before transforms propagate. Under fixed stepping
(`play.step`, PIE stepping, render-out) that is one sample per fixed step. Paused frames are not
sampled. `countdown` waits that many gameplay frames before the first sample.

**Sequences that are playing.** When a binding resolves to an entity a playing sequence drives
(one of its bound targets, or the camera its shots hold), the whole binding is left out of the
take, so recording during playback captures the game and not the sequence's own keys. To record it
anyway, pass `include_driven: true`. `excluded` lists what was left out.

**Key reduction.** Stopping reduces each channel with the Ramer-Douglas-Peucker simplification. The
error is measured at each sample's own time, which is what playback shows at that instant, and
must stay within the tolerance: `position` (0.001 m, distance), `rotation` (0.1 degrees of angle;
each Euler component keeps a third of it, which bounds the angle), `scale` (0.001), `property`
(0.0001, also `camera.fov` in degrees), `clip_time` (1/120 s). Every recorded sample is then checked
against the sequencer's own sampling, and a key is added wherever the replay misses. Every recorded
instant therefore replays within the tolerance, a straight move writes two keys, and a still object
writes two keys per track. Bool tracks keep the first sample, every change and the last sample.

## Methods

`sequence.record_start { bindings?, audio_cues?, countdown?, include_driven?, tolerance?,
cut_distance?, fps? }` starts a take. `bindings` is
`[{ name, guid | entity | entity_name | camera: true, channels?: [...] }]`, and each binding gives
exactly one target. `tolerance` is `{ position?, rotation?, scale?, property?, clip_time? }`, each
in (0, 1000]. Returns the state (below). Sampling starts in this frame's `PostUpdate`, or after the
countdown.

`sequence.record_state {}` returns the state: `status` (`idle`, `countdown`, `recording`, `full`
after 216000 frames), `countdown_left`, `frames`, `duration` (seconds), `cues`, `dropped_cues`
(past 4096), `bindings` (`{ name, entity, channels, frames, missing, excluded, cuts }`), `issues`
(problems met once each, such as a property that would not read), `unsaved_take` and `takes`.
It is a `read` method: it takes no parameter, writes no file and changes nothing. An app that has
not recorded yet answers an idle recorder's state and still has no recorder afterwards; "record" in
its name is the recorder whose state it reads (a named exception to the protocol's read-method
audit, ADR-5957).

`sequence.record_stop { sequence?, at?, cancel? }` stops the take and reduces its keys. It returns
`{ frames, duration, tracks: [{ name, samples, keys }], markers, excluded, dropped_cues, issues }`
plus:

- with `sequence` (a project-relative `.seq.ron`): the take is written to
  `<dir>/<stem>_take<N>.seq.ron`, where `N` is the lowest free number from 1. The source gains a
  `Sub` track `take<N>` that plays the take from `at` seconds (default 0). An existing source keeps
  every track it had, and a missing one is created holding just that section. Both files are
  written atomically. The result adds `take`, `number`, `sequence`, `section_track` and
  `created_sequence`, and the take joins the `SequenceLibrary`, so a player of the source plays it
  at once;
- without `sequence`: `seq_ron`, the take as text.

If the files cannot be written (`take_source_invalid`, `take_io`, `take_numbers_exhausted`,
`take_bad_path`), the take is kept: call `sequence.record_stop` again while idle, with another
path or with none. Until it is saved or dropped, `sequence.record_start` answers `take_unsaved`.
`cancel: true` drops the recording, or the kept take while idle. In the editor, the section shows
**Save again** and **Drop it** for a kept take.

## Example

A runnable session against a game with a moving entity named `Hero` and a camera. Each call is
`regia agent call <method> '<params json>'`; `play.step` advances the game by fixed steps:

```json
{ "method": "sequence.record_start", "params": {
    "bindings": [
      { "name": "hero", "entity_name": "Hero", "channels": ["transform", "property:visible"] },
      { "name": "camera", "camera": true }
    ],
    "countdown": 30, "audio_cues": true, "tolerance": { "position": 0.002 } } }
{ "method": "play.step", "params": { "frames": 30 } }
{ "method": "sequence.record_state", "params": {} }
{ "method": "play.step", "params": { "frames": 240 } }
{ "method": "sequence.record_stop", "params": { "sequence": "cinematics/intro.seq.ron", "at": 2.0 } }
```

The last call answers, for example,
`{ "take": "cinematics/intro_take1.seq.ron", "number": 1, "section_track": "take1",
"created_sequence": false, "frames": 240, "tracks": [{ "name": "hero/transform.position",
"samples": 240, "keys": 9 }, ...] }`. To play the take back, load the source with
`sequence.player { "action": "load", ... }` and bind `hero` (and `camera`) to the entities to
drive, or load the take on its own.

From Rust:

<!-- ignore-reason: a fragment spanning frames of a running game (`world`, `hero` and `project_root` come from it; the take is stopped later) -->
```rust,ignore
use regia_framework::sequencer::{
    Binding, TakeBinding, TakeChannel, TakeConfig, TakeRecorder, TakeTarget, save_take,
};

let mut recorder = world.resource_mut::<TakeRecorder>();
recorder.start(TakeConfig {
    bindings: vec![TakeBinding {
        name: "hero".into(),
        target: TakeTarget::Binding(Binding::Entity(hero)),
        channels: vec![TakeChannel::Transform, TakeChannel::parse("reflect:PointLight.radius")?],
    }],
    countdown: 30,
    ..TakeConfig::default()
})?;
// ... the game runs ...
let take = world.resource_mut::<TakeRecorder>().stop()?;
let files = save_take(project_root, "cinematics/intro.seq.ron", &take.sequence, 0.0)?;
```

Gameplay announces a sound to the recorder by sending
`TakeCue::new("door_slam", 1.2)` (a `Message`) next to playing it.

In the editor, the **Sequence Curves** panel has a **Record take** section. It lets you pick the
edited player's bound targets and the active camera, the channels (transform, animation, and
property paths as comma-separated text), audio cues, include driven, and the countdown in frames.
**Record** starts the take and **Stop** writes it next to the panel's sequence. If the edited player
plays that sequence, it reloads with the new take section. While PIE runs, the button reads
**Record (PIE)** and the take records the play world. The play world is a copy of the edit world,
so only targets bound by scene guid name the same entity there. An entity-id binding is refused
with `take_bad_binding`.

## Errors

| Code | When |
|---|---|
| `take_busy` | A take is already counting down or recording. |
| `take_unsaved` | A stopped take whose files could not be written is kept. Save it (`record_stop` with `sequence`) or drop it (`cancel: true`) before recording another. |
| `take_not_recording` | `record_stop` with no take recording and none kept unsaved. |
| `take_no_bindings` | No binding and no `audio_cues`. |
| `take_too_many` | Over 256 bindings, 32 channels per binding, or a countdown over 100000 frames. |
| `take_bad_binding` | A binding has no name, or its name is empty, repeated or contains `/`. Also when it has no channel, or does not give exactly one of `guid`, `entity`, `entity_name`, `camera`. |
| `take_bad_channel` | An unknown channel name, an empty property path, a repeated channel, or `transform` and `camera` on one binding. |
| `take_bad_tolerance` | A tolerance, `cut_distance` or `fps` is not a number in (0, 1000]. |
| `take_bad_path` | `sequence` is not a project-relative `.seq.ron` path, or leads out of the project. |
| `take_empty` | The take recorded nothing. Either it stopped before the first sample, or every binding was left out (driven). |
| `take_source_invalid` | The source exists and does not load. Nothing was written and the take is kept. |
| `take_invalid` | The take or the updated source would not validate (an internal error). Nothing was written. |
| `take_numbers_exhausted` | Every take number up to 9999 is used next to the source. |
| `take_io` | Writing failed, or there is no project root. The take is kept. |
| `bad_setting` | `countdown`, `at` or a boolean has the wrong type. |
| `bad_entity` | `entity` is not an entity id. |
| `unknown_entity` | `entity` does not exist, or no entity has `entity_name`. |

## Not done yet

- Single-clip animators expose no clip time, so only graph animators record `animation`.
- Audio is recorded from `TakeCue` messages, not from the mixer.
- Euler rotation tracks near a pitch of ±90 degrees (gimbal lock) can need many keys to stay within
  the tolerance. They still replay within it.
- Keys are linear. A Bezier fit with tangents would write fewer keys for smooth motion.
- Saving rewrites the source sequence from its parsed form: its tracks are kept, but comments and
  hand formatting are not, and an older version is written as v3 (as the editor's Save does).
- A take recording in the PIE play world is lost if PIE stops before **Stop**.
- Non-finite samples (a NaN pose or property) are dropped and listed in `issues`.
