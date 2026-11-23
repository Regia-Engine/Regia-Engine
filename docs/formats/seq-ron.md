# .seq.ron: the sequencer timeline (v5)

Spec: `docs/specs/12-assets.md` § 4a (versioning). Code: `regia_framework::sequencer`
(`Sequence`). Example: [`seq.example.ron`](seq.example.ron). Decisions: ADR-1507, ADR-1614 (player),
ADR-1880 (v2), ADR-2405 (v3), ADR-2862 (v4), ADR-2948 and ADR-3197 (v5).

A `.seq.ron` is the cinematics sequencer's data model: **one master clock** of named tracks and
markers. A track animates a single property of a single named target with plain keys — a float,
a vec3 or a bool — and a marker is a named instant that fires when playback crosses it. Camera
moves stay in [`.shots.ron`](shots-ron.md) (ADR-268); the sequencer is the timeline around them.

Everything is plain keyed data (seconds on the master clock; each property means whatever the
host's target says it means), so a hand-written file, an agent and the editor's Sequence Curves
panel edit the same thing. Version **6** (v6: event tracks and the track lock, [below](#version-6-adr-8105)); a newer version is rejected before the body is parsed
and the loader runs the migration chain on older files. v1 → v2 (ADR-1880) only added enum
variants and optional fields, v2 → v3 (ADR-2405) only optional fields and v3 → v4 (ADR-2862) only an
enum variant, so such a file loads
unchanged, samples bit for bit as before, and `regia upgrade-project` rewrites only its version
number (keys at their default tangent and sections at their v3 defaults gain no new text). v4 → v5
adds two optional root fields (ADR-2948: `spawnables` and `restore_on_stop`, whose defaults are the v4
behavior) and turns Doppler off on every spatial section (ADR-3197: Doppler is on by default in v5), so a
v4 file sounds as it did; `regia upgrade-project` writes `doppler: 0.0` into a file that has spatial sections
and only the version number into one that has none. The shape is hashed by
`cargo xtask check-formats`; `regia upgrade-project` lists the format. Re-saving an unchanged
file is byte-identical (`Sequence::to_ron`).

## Schema

```
Sequence {
    version: u32,                    // 5
    name: String = "",
    fps: f32 = 30.0,                 // authoring rate; playback is by time
    tracks: [Track] = [],
    markers: [Marker] = [],
}
Track {
    name: String,                    // unique, non-empty
    target: String = "",             // binding name the host resolves ("hero", "ui/title");
                                     // required by keyed and Clip tracks
    property: String = "",           // property path on the target ("transform.position");
                                     // required by keyed tracks
    keys: Float([ScalarKey]) | Vec3([Vec3Key]) | Bool([BoolKey])
        | Clip([ClipSection]) | Audio([AudioSection]) | Sub([SubSection])    // the last three: v2
        | Video([VideoSection]),                                             // v4
    enabled: bool = true,            // false mutes the track (sampling skips it)
}
ScalarKey { time: f32, value: f32, ease: Ease = Linear,
            tangent: Tangent<f32> = Auto,           // v2, omitted when Auto
            weights: Option<(f32, f32)> = None }    // v2, (incoming, outgoing), omitted when None
Vec3Key   { time: f32, value: (f32, f32, f32), ease: Ease = Linear,
            tangent: Tangent<(f32, f32, f32)> = Auto, weights: Option<(f32, f32)> = None }
BoolKey   { time: f32, value: bool }
Ease = Linear | Step | Smooth | Bezier               // Bezier: v2
Tangent<S> = Auto | Flat | Linear | Aligned(S) | Broken(S, S)     // slopes in value units per second
ClipSection  { clip: String, start: f32, end: f32, offset: f32 = 0, speed: f32 = 1,
               looping: bool = false, blend_in: f32 = 0, blend_out: f32 = 0, weight: f32 = 1 }
AudioSection { sound: String, start: f32, end: f32, offset: f32 = 0, volume_db: f32 = 0,
               fade_in: f32 = 0, fade_out: f32 = 0, bus: Option<String> = None,
               time_scale: f32 = 1,                    // v3, omitted when 1; in (0, 16]
               spatial: Option<AudioSpatial> = None,   // v3, omitted when None
               time_scale_mode: TimeScaleMode = Varispeed }   // v5, omitted when Varispeed;
                                                       // PreservePitch needs time_scale in 0.5..=2
AudioSpatial { source: SoundSource,                    // v3
               min_distance: f32 = 1, max_distance: f32 = 50,   // metres, 0 < min < max
               rolloff: SoundRolloff = Inverse, spatial_blend: f32 = 1,   // blend in 0..=1
               doppler: f32 = 1,                       // v5, omitted when 1; 0 off, 1 physical, 0..=4
               occlusion: bool = false }               // v5, omitted when false
TimeScaleMode = Varispeed | PreservePitch                        // v5
SoundSource  = Binding(String) | Position((f32, f32, f32))      // v3
SoundRolloff = Inverse | Linear | Log(f32)                       // v3, Log: dB per doubling, 0..=96
SubSection   { sequence: String, start: f32, end: f32, offset: f32 = 0, speed: f32 = 1,
               bindings: { String: String } = {},
               repeat: SubRepeat = Once,               // v3, omitted when Once
               loop_length: Option<f32> = None }       // v3, sub-sequence seconds per pass, >= 0.001
SubRepeat    = Once | Count(u32) | UntilStop                     // v3, Count in 1..=1000000
VideoSection { video: String, start: f32, end: f32, offset: f32 = 0,  // v4, a project path of a
               target: VideoTarget = Fullscreen(),                    // .video.ron, .webm or .mkv
               volume_db: f32 = 0,                 // the source's own sound; at most 24, -80 is silent
               looping: bool = false }             // wrap over the source's length
VideoTarget  = Fullscreen(layer: i32 = 0, fit: VideoFit = Letterbox)  // v4, negative layers under the UI
             | Player(String)                                         // the bound entity's video player
             | Screen(binding: String, slot: VideoScreenSlot = Emissive)  // a bound mesh's material slot
             | UiImage(String)                                        // a bound UI image node
VideoFit     = Letterbox | Fill | Stretch                        // v4
VideoScreenSlot = Albedo | Emissive                              // v4
Marker { name: String, time: f32, payload: String = "" }
```

`Option` fields are written `Some(...)` or `None`. Times are seconds on the master clock, zero or
more.

## Meaning

- **Duration is derived**: the latest key time, section end or marker time (an empty sequence has
  duration 0 and is always finished). There is no explicit length to keep in sync; every section
  has an explicit `end` for that reason.
- **Channels.** A track has exactly one kind of value; two properties need two tracks. Float and
  vec3 keys hold the first value before the first key and the last value after the last, and in
  between lerp with the segment's `ease` (`Smooth` is smoothstep, `3u² − 2u³`; `ease` on a key
  shapes the segment that starts at it; `Step` holds that key's value). A vec3's components all
  ease the same way. Bool keys always step: the value of the last key at or before the time.
- **Bezier keys (v2).** A segment whose first key eases `Bezier` is a cubic Bezier from that key's
  outgoing tangent to the next key's incoming tangent; its handles are `weight × segment` long
  along the slopes (`weights` default to 1/3 each). Unweighted, the segment is exactly the cubic
  Hermite curve of the two slopes. Weighted, the two weights are scaled to sum to at most one (so
  time stays monotone) and the curve parameter of a time is found by 48 bisection steps (a fixed
  count: deterministic). Tangent modes: `Auto` is the slope through the two neighbors, zero at a
  local extreme and at most three times the smaller neighboring secant (no overshoot between
  monotone keys, Fritsch–Carlson), flat at the first and last key (a two-key auto curve equals
  `Smooth`); `Flat` is zero; `Linear` points each side at its neighbor; `Aligned(s)` is one slope
  on both sides; `Broken(in, out)` two. A vec3 key's tangent has one slope per component and its
  weights are shared.
- **Clip tracks (v2).** `target` binds a rig (an entity with a `SkinnedMesh`); each section plays
  a clip of the player's clip library from `start` to `end` at `offset + (t − start) × speed` clip
  seconds (wrapped over the clip's length when `looping`, else held at its ends). Sections may
  overlap: their poses blend by weight, the weight rising linearly over `blend_in` and falling over
  `blend_out` (so a blend out over a blend in of the same length is a constant-weight crossfade).
  Where the summed weight is below one the rig's own pose shows through by the rest. A section is
  in effect over `[start, end)`, and at its end instant unless another section of the track
  starts there (the last pose holds at the end of a sequence).
- **Audio tracks (v2).** Each section plays a sound of the audio library over `[start, end)`,
  `offset` seconds in at `start`, at `volume_db`, with linear fade gains over `fade_in` and
  `fade_out`, on `bus` (default `Master`). A player starts a section's sound where the clock is, so
  a seek or a start mid-section is in sync.
- **Sub-sequence tracks (v2).** Each section plays another sequence (by its library name, usually
  its project path) at `offset + (t − start) × speed`, in effect like a clip section. Its keyed
  values, clips and sounds join this sequence's frame; its target names are renamed through
  `bindings` (innermost section first; unlisted names are kept). Its markers fire on this clock
  as `track/marker` when the section is in effect at that instant, so a section split in two fires a
  marker on the seam once. A tree expands to at most 16 384 sections. Sub-sequences nest at most 8 deep; a
  missing or cyclic one is skipped and reported by the player.
- **Video tracks (v4).** Each section shows a source over `[start, end)` (and at its end instant
  when the sequence ends there, so the last picture holds), at the source time
  `offset + (t - start)`. See [Version 4](#version-4-adr-2862).
- **Sampling** (`Sequence::sample`) is a pure function of `(sequence, time)`: clamped to
  `0..=duration`, a non-finite time reads as 0, the end holds the last values, disabled tracks do
  not appear. The same input gives the same frame on every call, machine and frame rate.
- **Targets are names, not references.** The file does not say what `target` is or what
  `property` means; whoever plays it binds them (the engine's player binds a target name to an
  entity by scene guid or id and writes properties through registered appliers, ADR-1614; a host
  may use its own). Two tracks may name the same target and property — both are sampled, the
  later one is the one to apply.
- **Markers** fire when playback crosses them: `Sequence::markers_between(from, to)` returns the
  markers in `(from, to]`, time-ordered — `to` included, `from` not, so a marker fires exactly
  once as the clock reaches it. A backward or paused step fires nothing. A seek itself fires
  nothing; the player decides what a jump crosses. `payload` is free-form data for the host.
- **Composition with `.shots.ron`.** The sequencer owns the master timeline; a camera-shot
  sequence is a keyed camera move over the same wall of time. A player can drive both from one
  clock (ADR-1507 keeps the models separate on purpose). The engine's player is
  `regia_framework::sequencer::SequencePlayer` (ADR-1614, book `agents/sequence-player.md`): it
  resolves targets through `SequenceBindings` (scene guid or entity), applies properties through a
  registry of appliers, sends markers as `MarkerReached` messages and lets the shots take over a
  camera. The format is unchanged by it.

## Validation

`Sequence::validate` (run by `from_ron`) rejects: `fps` outside (0, 1000]; empty or duplicate
track names; an empty `target` (keyed and clip tracks) or `property` (keyed tracks); a channel
with no keys or sections, or more than 1024 sections; key times that are non-finite, negative,
repeated or unsorted; non-finite float/vec3 values or tangent slopes; weights outside (0, 1];
sections with a negative start, an end not after the start, an empty clip, sound or sequence
name, a speed outside (0, 1000], a non-finite offset (a negative one for audio), ramps or fades
longer than the section, a clip weight outside (0, 1], a volume above 24 dB, an empty bus or an
empty rename; empty or duplicate marker names; non-finite or negative marker times. v3 also
rejects a `time_scale` outside (0, 16], a spatial source with an empty binding or a non-finite
position, distances that are not `0 < min < max`, a `Log` rolloff outside 0..=96 dB, a
`spatial_blend` outside 0..=1, a `Count` outside 1..=1000000 and a `loop_length` below 0.001 s. v4
also rejects a video section with an empty path, a negative or non-finite offset, a volume above
24 dB or non-finite, or an empty target binding. Each message names the track, the section and the
field.

A sequence that validates always samples finite values (ADR-2135): a segment whose f32 arithmetic
overflows (keys of opposite sign near the f32 range, keys a few subnormals apart) is recomputed in f64
or clamped to the f32 range; results that were finite are unchanged bit for bit. The fuzz target
`sequence.eval` checks it.

## Version 3 (ADR-2405)

**Spatial sound sections.** `spatial` places a sound in the world. `Binding(name)` emits from the
entity a player binds to `name` (a binding name like a track `target`, renamed through the
sub-sequence sections above it, and listed by `Sequence::target_names`); the player follows its
world position every frame. `Position` emits from a fixed world position. The player computes the
gain and pan with the audio emitter math of `regia_audio` (`spatialize`: the section's distances
and rolloff, equal-power pan, no cone) against the nearest listener; occlusion, propagation,
binaural rendering and Doppler are not applied to sequence voices.

**Voice time scale.** `time_scale` is sound seconds per sequence second. The voice's rate is it times
the `speed` of every sub-sequence section above the section (`SoundSample::rate`), and the sound
position is `offset + (t - start) × time_scale` in the section's sequence time. The rule is
**varispeed**: the voice plays at the rate and its pitch ratio is the rate (twice as fast is an
octave up). There is no pitch-preserving time stretch.

**Looping sub-sequences.** With `repeat` `Count(n)` (n ≥ 2) or `UntilStop`, a section plays passes
of `loop_length` sub-sequence seconds from `offset` (default `pass = duration(sub) - offset`; a
default shorter than 0.001 s does not loop). With `e = (t - start) × speed`, the sub-sequence time
is `offset + (e mod pass)` (an exact floating-point remainder) in pass `floor(e / pass)`; after the
last pass of `Count(n)` it holds at `offset + pass`. Evaluation is a pure function of `t`: `t` and
`t + pass / speed` sample the same values whenever the clock values are exact. `Count(1)` is
`Once`. The sub-sequence's markers fire once per pass inside the section (`flat_markers` lists at
most 65,536 markers and visits at most 2^20 passes). Looping sections nest at most 4 deep
(`MAX_LOOP_DEPTH`): `SequenceLibrary::check` refuses a deeper tree with
`SequenceError::LoopTooDeep` (code `seq_loop_too_deep`), and sampling plays the loop past the limit
once.

**Binding renames** are an edit, not a format change: `regia_framework::sequencer::rename_binding`
renames one binding in every track target, spatial source and sub-sequence binding table of a
sequence (adding a `from -> to` entry for a name a sub-sequence binds implicitly, so the
sub-sequence file is never edited).

## Version 4 (ADR-2862)

**Video sections.** A `Video` track holds sections that place a clip of a video on the clock. The
source is a project path, resolved by `regia_media::MediaFiles`. `offset` is seconds into the source at
`start`, and a section under a sub-sequence section runs at that section's speed. Past the source's
end a section holds its last picture, or with `looping` wraps over the source's length.

The picture at source time `s` is the one with the greatest presentation time at or before
`s + 0.5 ms`: WebM timestamps are rounded to the millisecond, and a frame whose stamp rounds up must
still show on its output frame. So an output frame at time `t` shows the same source frame however
it was reached.

The target says where the picture goes. A binding name is a binding like a track `target`: the
player resolves it, sub-sequence sections rename it, `Sequence::target_names` lists it and
`rename_binding` renames it. Two sections on one entity at one instant: the later one in the tree
has it. `Player(name)` takes over the bound entity's `VideoPlayer` (adding one if it has none) and
restores it when the section ends. `Screen` binds the picture into the albedo or emissive slot of
the bound entity's mesh material and restores the material. `UiImage` shows it in the bound UI node
and restores the node's image. `Fullscreen` draws it over the view; a negative `layer` draws under
the UI, any other above it (with `regia_media::VideoOverlayPlugin`).

What plays it is not part of the file: the player's behavior (sync, seeks, frame-exact render-out)
is in the book's [sequence player page](../../book/agents/sequence-player.md). A video that cannot play
never stops the rest of the sequence: it reports `seq_video_missing`, `seq_video_failed` or
`seq_video_unavailable`.

## Version 5 (ADR-2948, ADR-3197)

**Spawnable bindings (ADR-2948).** The root gains `spawnables` (left out when empty) and `restore_on_stop`
(left out when `false`):

```ron
(version: 5, name: "intro", tracks: [
    (name: "hero_move", target: "hero", property: "transform.position",
        keys: Vec3([(time: 0.5, value: (0.0, 0.0, 0.0)), (time: 1.5, value: (5.0, 0.0, 0.0))])),
], spawnables: [
    (binding: "hero", prefab: "actors/hero.prefab.ron", start: 0.5, end: Some(1.5)),
    (binding: "crate", prefab: "guid:0000000000000000000000000000beef", start: 2.0, keep_on_stop: true),
], restore_on_stop: true)
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `binding` | string | (required) | The name tracks target. Unique among the spawnables. |
| `prefab` | string | (required) | A project-relative `.prefab.ron` path (`/`-separated, no `..`, no backslash, no `:`), or `guid:` and 32 hex digits naming a template already in the scene's prefab library. |
| `start` | f32 | `0` | Seconds the entity appears at (zero or more). |
| `end` | `Option<f32>` | `None` | Seconds it disappears at (after `start`); `None` keeps it to the end. |
| `keep_on_stop` | bool | `false` | Leave the entity in the world when the player stops or lets go. |

The entity exists while the clock is in `[start, end)`, and at `end` when that is the player's end
and the player holds its last frame. A spawnable's `end` (or `start`) counts toward the sequence's
length. `restore_on_stop: true` writes back, when the player stops or lets go, the value each keyed
track's property had before the player first wrote it (through the property readers; a property
with no reader is not restored). Validation: at most 256 spawnables, non-empty unique bindings, the
prefab rules above, a finite `start` of zero or more and an `end` after it.

What plays it is not part of the file: the pre-roll, the deterministic guids, despawning and the
`seq_spawn_failed` codes are in the book's [sequence player page](../../book/agents/sequence-player.md)
and the cutscene flow in [cutscenes](../../book/agents/cutscene.md). Only the top sequence's
spawnables spawn; a sub-sequence's are ignored.

**Doppler (ADR-3197).** `doppler` scales the Doppler shift of a spatial section (0 off, 1 physical, at most 4).
The emitter's velocity is the section's motion on the sequence clock: the slope of the bound
entity's `transform.position` track where it has one, else the bound entity's position change over
the frame step; a `Position` source is still. The pitch ratio is clamped to 0.5..2. A v5 file that
leaves `doppler` out gets 1; the v4 → v5 step writes 0 on every spatial section.

**Occlusion.** `occlusion: true` makes the section ask the audio occlusion query whether geometry
stands between the listener and the source; an occluded section plays quieter and low-passed by the
occlusion tuning (`regia_audio::OcclusionTuning`). Off by default.

**Time scale mode.** `time_scale_mode: PreservePitch` keeps the pitch while the time scale (times the
speeds of the sub-sequence sections above it) changes the speed: the player stretches the section's
range of the sound (`offset` for `(end - start) × time_scale` sound seconds) with a WSOLA time stretch
rendered ahead, and plays that. The section's `time_scale` must be in 0.5..=2; a voice rate outside
that range (from a sub-sequence speed) plays by varispeed and is reported. `Varispeed` (the default)
is the v3 behavior: pitch follows speed.

What plays these is not part of the file: the player's behavior (velocity sampling, the occlusion
ray and filter, the stretch cache) is in the book's
[sequence voices page](../../book/agents/sequence-voices-v5.md).

## Version 6 (ADR-8105)

v6 adds an event channel and a track lock. v5 → v6 changes nothing in a v5 file (it has no event
track and nothing locked): the step only rewrites the version number.

```ron
(name: "cues", keys: Event([
    (time: 2.0, event: "door_open", payload: "fast"),
    (time: 2.0, event: "door_sound"),
])),
(name: "door-angle", target: "door", property: "hinge.angle",
    keys: Float([(time: 2.0, value: 0.0)]), locked: true),
```

**Event tracks.** `Event([...])` keys each name an `event` (non-empty) and an optional `payload`
(left out of the file when empty). Times are finite, zero or more and in time order; equal times are
allowed and fire in file order. `target` is optional (the binding the event is about; empty for a
sequence-wide event); `property` is unused. When playback crosses a key, the player sends
`SequenceEventFired { player, track, target, event, payload, time, cycle, skipped }` (game code
reads it like `MarkerReached`): forward play only, once per pass of a loop, ordered by time, then
track order, then file order. `skip_to` delivers the keys it jumps over with `skipped: true`; a
seek, a scrub and reverse play (a backward external clock write is a seek) fire nothing. A muted
track (`enabled: false`) fires nothing. Only the sequence's own event tracks fire: the event tracks
of a sub-sequence do not (a known limit of v6).

**Lock.** `locked: true` (left out when `false`) is editor meaning only: the timeline and the
`editor.seq_*` edits refuse to change a locked track's keys, sections, name, binding or position
(agent code `seq_track_locked`); playback ignores it. Mute stays `enabled: false`. Solo is not in the
file: it is editor session state.

## Changing the format

A change to the shape needs a version bump and a migrator (`book/changing-a-content-format.md`):
bump `SEQUENCE_FORMAT_VERSION`, add the `VersionStep` in `sequence_migrations()` and a migrator
in `regia upgrade-project`, add a fixture of the old version, then `cargo xtask check-formats
--update`. v2 did exactly that: the 1 → 2 step, `migrate_sequence` in `regia upgrade-project`, the
v1 fixture in the `asset` tests. v3 did the same: the 2 → 3 step, `migrate_sequence` (unchanged:
it rewrites the version number) and the v2 fixture in the `asset` tests. v4 too: the 3 → 4 step,
`migrate_sequence` (unchanged) and the v3 fixture in the `asset` tests. v5 as well: the 4 → 5 step turns
Doppler off, and `migrate_sequence` re-saves the migrated sequence when a version-number bump alone
would read differently (a v4 file with spatial sections); the v4 fixtures are in the `asset` and
`upgrade_project` tests, and a v4 file in `regia_framework/tests/sequencer_cutscene.rs`.
