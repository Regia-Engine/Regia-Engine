# The sequencer

The sequencer plays a timeline: tracks that animate one property of a named thing each, sounds,
animation clips, other sequences, and markers that tell your game when a moment arrives. Camera
moves live beside it in a shot list. One clock drives both, so a cutscene is a pair of data files and
one player. The same files play in a running game, scrub in the editor, and render frame-exact into a
video.

This chapter is the whole workflow. [Camera shot sequences](shot-sequences.md) covers the camera side
in depth and [Rendering a video](render-out.md) the output side. Method-level detail is on the agent
pages [Sequence players](../agents/sequence-player.md) and
[Sequence curves](../agents/seq-curve-panel.md); the format is `docs/formats/seq-ron.md`, with a full
example in `docs/formats/seq.example.ron`.

## The model in one minute

- A **sequence** (`.seq.ron`) holds **tracks** and **markers** on one master clock, in seconds. A
  track animates one property of one **target** name (`hero`, `ui/title`). The file says what the
  target is called, not what it is: the player binds the name to an entity.
- A **shot list** (`.shots.ron`) is the camera's move. A player takes over a camera while the shots
  are on screen and gives it back after.
- A **player** (`SequencePlayer`) owns the clock, the bindings and the end behavior.
- Evaluation is a pure function of the time: the same sequence at 3.5 s gives the same values on every
  machine and frame rate.

## 1. Write a sequence

```ron
(
    version: 3,
    name: "intro",
    fps: 30.0,
    tracks: [
        (
            name: "title-opacity",
            target: "ui/title",
            property: "opacity",
            keys: Float([
                (time: 0.0, value: 0.0),
                (time: 1.0, value: 1.0, ease: Smooth),
                (time: 3.0, value: 1.0),
                (time: 4.0, value: 0.0, ease: Smooth),
            ]),
        ),
        (
            name: "hero-position",
            target: "hero",
            property: "transform.position",
            keys: Vec3([
                (time: 0.0, value: (-6.0, 0.0, 0.0), ease: Bezier),
                (time: 3.0, value: (0.0, 0.0, 0.0)),
            ]),
        ),
        (
            name: "lamp",
            target: "porch_light",
            property: "light.intensity",
            keys: Float([(time: 0.0, value: 0.0, ease: Step), (time: 2.0, value: 800.0)]),
        ),
    ],
    markers: [
        (name: "door_open", time: 2.0, payload: "front"),
    ],
)
```

The duration is derived (the latest key, section end or marker), so there is no length to keep in
sync. `Sequence::from_ron` validates a file and the message names the track. It rejects, among
other things, an `fps` outside (0, 1000], empty or duplicate track names, a keyed track without a
`target` or `property`, key times that are negative, repeated or unsorted, and non-finite values.
Saving with `to_ron` is canonical, so an unchanged file re-saves byte for byte.

### Tracks and curves

A track has exactly one kind of value: `Float`, `Vec3` or `Bool` keys, plus the section tracks
below. Two properties need two tracks.

- Values hold the first key's value before the first key and the last key's value after the last.
- `ease` on a key shapes the segment that starts there: `Linear`, `Step` (holds that key's value),
  `Smooth` (smoothstep) or `Bezier`. Bool keys always step.
- A `Bezier` key is a cubic segment. Each key has a `tangent`: `Auto` (the default; it never
  overshoots between monotone keys and is flat at the ends), `Flat`, `Linear`, `Aligned(slope)` or
  `Broken(in, out)`, slopes in value units per second. Optional `weights: Some((in, out))` set the
  handle lengths as fractions of the segment (each in (0, 1]).
- `enabled: false` mutes a track without deleting it.

The properties the engine applies out of the box:

| Path | Channel | Writes |
|---|---|---|
| `transform.position` | Vec3 | position, meters |
| `transform.rotation` | Vec3 | Euler degrees `[pitch, yaw, roll]` |
| `transform.scale` | Vec3 or Float | scale (a Float scales uniformly) |
| `visible` | Bool | visible or hidden |
| `light.intensity` | Float | point or spot intensity, or directional illuminance |
| `light.color` | Vec3 | the light's linear RGB |
| `camera.fov` | Float | vertical field of view, degrees |

Your game can register more with `PropertyAppliers::register`. A path with no applier shows once in
the player's issues as `unknown_property`; a value an applier refuses is `apply_failed`. Neither
stops the other tracks.

### Edit curves in the editor

The editor's **Sequence Curves** tab edits one component of one track at a time. Type a project path
and press **Open** to spawn a stopped player of that file; **Save** writes it back. Drag a key to
move it, double-click empty space to add one, drag empty space to scrub the player, drag a tangent
handle to set a slope (hold Alt to move one side only), and right-click a key for its ease, tangent
mode and **Delete key**. The curve is drawn with the engine's own sampling, so what you see is what
plays. Every edit is validated on a copy and is one undo step (a drag is one step). Clip, audio and
sub-sequence tracks show a section strip instead of a curve.

## 2. Animation clip tracks

A `Clip` track binds a rig (an entity with a skinned mesh) and plays clips from the clip library:

```ron
(
    name: "hero-body",
    target: "hero",
    keys: Clip([
        (clip: "walk", start: 1.0, end: 3.5, looping: true, blend_out: 0.5),
        (clip: "wave", start: 3.0, end: 6.0, blend_in: 0.5),
    ]),
)
```

Each section plays `clip` from `start` to `end` at `offset + (t - start) x speed` clip seconds.
Overlapping sections crossfade by weight; below a summed weight of one the rig's own pose shows
through, and when the last section ends the rig has its pose back. Register clips with
`sequence.player { "action": "clip", "name": "walk", "asset": "anim/hero.glb#walk" }`. A missing clip
is the issue `unknown_clip`.

## 3. Audio tracks

An `Audio` section plays a sound from your game's sound assets over `[start, end)`, `offset` seconds
in, with `volume_db` (up to 24), `fade_in`, `fade_out` and a mixer `bus`:

```ron
(
    name: "sound",
    keys: Audio([
        (sound: "music_intro", start: 0.0, end: 6.0, volume_db: -6.0, fade_out: 1.5, bus: Some("Music")),
        (sound: "footsteps", start: 1.0, end: 3.5, time_scale: 1.25,
            spatial: Some((source: Binding("hero"), max_distance: 25.0))),
    ]),
)
```

A seek, or a start in the middle of a section, starts the sound where the clock is, in sync. A sound
stops when the player pauses, stops, finishes or despawns, and when the game is paused; it returns
at the clock's offset. Sounds run on the mixer's clock, so the game's time scale does not bend them.
A sound the game does not have is the issue `unknown_sound`.

### Spatial sections

Add `spatial` to play a section in 3D. `source` is `Binding("hero")`, a binding name like a track
target whose entity the voice follows every frame, or `Position((x, y, z))` for a fixed point.
`min_distance` (default 1) and `max_distance` (default 50) must satisfy 0 < min < max;
`rolloff` is `Inverse`, `Linear` or `Log(db_per_doubling)` (0 to 96); `spatial_blend` is 0 to 1.
Gain and pan come from the engine's emitter math against the nearest listener. Occlusion, sound
propagation, binaural rendering and Doppler do not apply to sequence voices. A binding name nothing
is bound to plays at the listener and shows as an `unbound_target` issue.

`time_scale` (greater than 0, up to 16, default 1) is sound seconds per sequence second. It works by
varispeed: rate 2 plays twice as fast and an octave higher. There is no pitch-preserving stretch, so
keep a sound that must hold its pitch at 1.

`sequencer.spatial_voices` shows each spatial voice's resolved source, distances, gain and pan at the
player's time, which is the quickest way to find out why a footstep is silent.

## 4. Sub-sequences

A `Sub` section plays another sequence from the sequence library, so a shared effect is written once:

```ron
(
    name: "sparkle",
    keys: Sub([
        (sequence: "cinematics/fx/sparkle.seq.ron", start: 2.5, end: 4.0,
            bindings: {"lamp": "hero/light"}, repeat: Count(2), loop_length: Some(0.75)),
    ]),
)
```

- The sub-sequence plays at `offset + (t - start) x speed`.
- `bindings` renames its targets, so its `lamp` becomes your `hero/light`, which you bind in the
  parent. Unlisted names pass through.
- Its markers fire on the parent's clock as `track/marker`.
- `repeat` is `Once` (default), `Count(n)` (1 to 1,000,000 passes) or `UntilStop`. `loop_length` is
  the sub-sequence seconds per pass (at least 0.001; the default is the sub-sequence's length after
  `offset`). The time wraps by an exact remainder, so the instant `t` and `t` plus one pass sample the
  same values.
- Sub-sequences nest at most 8 deep, and looping sections at most 4 (`seq_loop_too_deep`). A tree
  expands to at most 16,384 sections. A missing or cyclic sub-sequence is skipped and shows as a
  `sub_sequence` issue.

Fill the library with `sequence.player { "action": "library", "name": "cinematics/fx/sparkle.seq.ron",
"seq_ron": "..." }`; the name is what the section's `sequence` says. Opening a file in the editor's
curve panel adds its sub-sequences too. `sequencer.sub_loops` lists every sub-sequence section in
effect at a time with its pass number and local time.

## 5. Bindings

A binding maps a target name to an entity:

- by scene guid, which survives save, load and level moves;
- by entity id, for things spawned at runtime;
- by name, which binds by guid when the entity has one.

A target with no binding, or bound to something that no longer exists, is reported once as
`unbound_target` and its tracks are skipped until it binds. To rename a target everywhere (tracks,
spatial sources and sub-sequence tables) use `sequencer.rename_binding { "from": "hero", "to":
"guard" }`; it refuses a rename that would merge two bindings (`seq_binding_in_use`). The editor runs
the same rename as one undo step.

## 6. Markers

A marker is a named instant with a free-form `payload`. Playback fires `MarkerReached` when the
clock crosses it, once, in the half-open interval `(from, to]`. Markers of one step arrive in time
order, then by name. **Seeking never fires markers.** Use them to hand control back to gameplay at
the right moment: open the door on `door_open`, enable input on `cutscene_end`. When the player
finishes under `hold` or `once` it sends `SequenceFinished`.

## 7. Play it in a game

Add the plugin, make a player, bind the targets, and start it:

<!-- ignore-reason: a fragment of a game setup; the app, commands and hero_guid come from the game -->
```rust,ignore
use regia_framework::sequencer::{
    Binding, EndPolicy, MarkerReached, Sequence, SequenceBindings, SequencePlayer, SequencerPlugin,
};
use regia_framework::shots::ShotSequence;

app.add_plugins(SequencerPlugin);

let sequence = Sequence::from_ron(&std::fs::read_to_string("intro.seq.ron")?)?;
let shots = ShotSequence::from_ron(&std::fs::read_to_string("intro.shots.ron")?)?;
let mut player = SequencePlayer::new(sequence).with_shots(shots);
player.end = EndPolicy::Once;
player.takeover.blend_in = 0.75;
player.play();
commands.spawn((player, SequenceBindings::default().with("hero", Binding::Guid(hero_guid))));

fn on_marker(mut markers: MessageReader<MarkerReached>) {
    for m in markers.read() { /* m.name, m.payload */ }
}
```

Settings on the player:

| Setting | Values |
|---|---|
| `end` | `Hold` (default; keeps the last frame and the camera), `Loop` (each wrap is a camera cut), `Once` (applies the last frame once and lets go) |
| `start_delay` | seconds of game time between `play` and the clock starting |
| `clock` | `Gameplay` (advances by the game's time step) or `External` (the host writes the time, as render-out does) |
| `takeover` | `enabled`, `camera` (default: the camera with the highest `order`), `blend_in`, `blend_out` (default 0.5 seconds, 0 cuts) |

On the gameplay clock the player advances by the game's time step, so pause, time scale and `play.step`
all apply and no wall clock is read. Whole nanoseconds are counted, so sixty 60 Hz steps reach exactly
the instant thirty 30 Hz steps reach: a replay at either rate fires the same markers.

### The camera

With a shot list, the player claims a camera while the shots are on screen. The pose blends from the
live gameplay pose over `blend_in` and back over `blend_out`, with no jump on the first frame. A
camera rig keeps running underneath, so the blend out lands where gameplay is now. Of several players
claiming one camera the most recently started wins. Hard changes (a shot change, a loop wrap, a seek,
a switch of winning player) are camera cuts that restart the renderer's temporal history. Director
shakes play on top of the cinematic pose. `takeover: false` keeps the shots as data only.

During a cutscene in a streamed level the player keeps a prefetch source ahead of the camera, so the
next stretch of the world is loaded when the camera gets there (see
[World streaming](world-streaming.md)).

### Driving it from a script

The same player answers `sequence.player`: `load` (from `seq_ron` and `shots_ron` text, optionally
with `bindings` and `play`), `play`, `pause`, `stop`, `seek`, `sample` (the values at any time
without moving the clock), `bind`, `set` and `clear`. Calls land at a frame boundary and apply later in
that frame, so read the result from the next call. A rejected call changes nothing and carries a
lowercase `data.code`: for example `bad_sequence`, `bad_shots`, `bad_binding`, `bad_time`,
`too_large` (more than 1024 tracks, 4096 keys or markers per track, 256 shots), `no_player`,
`several_players` (name the `player`) and `unknown_player`. The full list is on the
[agent page](../agents/sequence-player.md).

## 8. Check it

1. Load the sequence and bindings with `sequence.player { "action": "load", ..., "play": false }`.
2. `sequence.player { "action": "sample", "time": 1.0 }` returns every track value and the shot pose
   at that instant. Compare it against what you authored, at several times.
3. Read `state.bindings` and `state.issues`: every target should be `bound`, and the issue list
   empty.
4. Step with `play.step` and take `seeing.screenshot` at the moments that matter.

## 9. Render it out

For a video, take the sequencer off the gameplay clock. Load the player with `"clock": "external"`
and start a render-out job: the job restarts every external-clock player at 0 and writes the
timeline time into each one every frame, shutter subframes included, so tracks apply and markers
fire on the render clock. Two ways:

- **Straight render-out.** `render.out_start` with `frames` or `duration_s`, `fps` and the size; see
  [Rendering a video](render-out.md). Align `cuts` with the shot boundaries (frame = shot start
  x fps).
- **A render job** (`.render_job.ron`). One file names the shot list, the sequence, the output
  settings and a list of takes cut by `Whole`, `Frames`, `Seconds`, `Markers` or `Shots` ranges. A take
  is byte-identical to the same frames of a full render. Run it with
  `regia render --job cinematics/trailer.render_job.ron` against a running app, add `--dry-run` for
  the plan (takes, cuts, pre-roll) first. The format is `docs/formats/render-job-ron.md` and the
  methods are on [Render jobs](../agents/render-jobs.md).
- **From the editor.** **Sequence ▸ Render movie…** renders a saved `.seq.ron` to a folder of the project
  (format, frame range, size, frame rate) with the editor's renderer, cancellable, with progress. It runs the
  same render job as `render.out_job`, so the frames are the same. See [Render movie](../agents/render-movie.md).

Give the shot list to the job or to a sequence player, not both. If the job finds no external-clock
player it warns `sequence_tracks_not_applied`: the tracks will not move. The finished camera work
looks best on the offline Cinematic profile ([Cinematic profile](../agents/cinematic.md)), which
marches clouds several times per pixel and uses larger shadow cascades. It needs a High or Ultra tier
GPU (or `force: true`) and is much slower per frame, which does not matter offline.

## Limits to know

- Sequence voices have no occlusion or Doppler, and time scale changes pitch.
- A shot list and a sequence are separate files on purpose; the player is what joins them.
- Authoring is in world meters. In a streamed level the sequence stays put across a floating-origin
  rebase: camera and root-entity tracks convert on their own.
