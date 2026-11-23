# .shots.ron: shot sequences and camera paths (v3)

Spec: `docs/specs/23-agent-native.md` (agent surface), `docs/specs/12-assets.md` § 4a (versioning).
Code: `regia_framework::shots` (`ShotSequence`). Example: [`shots.example.ron`](shots.example.ron).
Guide: `book/agents/camera-paths.md`.

A `.shots.ron` is a list of shots played back to back. Each shot is a camera move with its own clock,
starting at zero: a position path, a look-at rule, and tracks for field of view, roll and depth of
field, plus a fade in and out. The boundary between two shots is a hard cut. Everything is plain keyed
data (seconds, metres, degrees, f-stops), so a hand-written file, an agent and a later editor panel
edit the same thing. Version **3** (v2 adds the optional `rotation` track; v3 lets path and look-at keys
pass a shot's end, ADR-8640); a newer version is rejected before the body is parsed and the loader runs the
migration chain on older files (v1 → v2 and v2 → v3 only change the version; validation runs after the chain,
so a v2 file a v0.6 development build wrote with tail keys loads as v3). The shape is hashed by
`cargo xtask check-formats`; `regia upgrade-project` lists the format. Re-saving an unchanged file
is byte-identical (`ShotSequence::to_ron`).

## Schema

```
ShotSequence {
    version: u32,                    // 3
    name: String = "",
    fps: f32 = 30.0,                 // authoring rate; only converts frame numbers to seconds
    shots: [Shot],                   // at least one
}
Shot {
    name: String,                    // unique, non-empty
    duration: f32,                   // seconds, > 0
    camera: ShotCamera,
    fade_in: f32 = 0.0,              // seconds from fade_color to the picture
    fade_out: f32 = 0.0,             // seconds from the picture to fade_color; fade_in + fade_out <= duration
    fade_color: (f32, f32, f32) = (0, 0, 0),    // linear RGB, 0..=1
}
ShotCamera {
    path: CameraPath,
    fov: Track = [],                 // vertical degrees, 1..=170; empty = 45
    roll: Track = [],                // degrees about the view axis; empty = 0
    dof: Option<DepthOfFieldTracks> = None,
    rotation: [RotationKey] = [],    // v2; empty = orientation from path.look_at
}
RotationKey {
    time: f32,                       // strictly increasing within 0..=duration
    rotation: (f32, f32, f32, f32),  // quaternion (x, y, z, w), non-zero, normalized on use
    ease: Linear | Step | Smooth = Linear,
}
CameraPath {
    interp: Linear | CatmullRom | Step = CatmullRom,
    keys: [PathKey],                 // at least one, times >= 0 and strictly increasing (may pass duration, below)
    look_at: Heading | Point((x, y, z)) | Track([PathKey]) = Heading,   // Track keys: as path.keys
}
PathKey { time: f32, position: (f32, f32, f32) }
                                     // a path or look_at key may sit past the shot's duration (ADR-8640): a
                                     // shortened CatmullRom path keeps its tail keys as control keys; the
                                     // shot's clock never reaches them. v3 only: a build that reads v2 refuses
                                     // a v3 file by its version ("shot sequence version 3 is newer than
                                     // supported version 2; upgrade the engine"), never by a key time.
Track = [ScalarKey]                  // times strictly increasing within 0..=duration
ScalarKey { time: f32, value: f32, ease: Linear | Step | Smooth = Linear }
DepthOfFieldTracks {
    focus_distance: Track = [],      // metres, > 0; empty = 10; ignored when focus_on_target
    aperture: Track = [],            // f-stops, 0.1..=128; empty = 2.8
    focus_on_target: bool = false,   // focus distance = eye-to-target distance
    bokeh: bool = false,             // hexagonal bokeh instead of a Gaussian blur
}
```

`Option` fields are written `Some((...))` or `None`. Times are seconds since the start of the *shot*.

## Meaning

- **Position.** `Linear` joins the keys with straight segments; `Step` holds each key until the next;
  `CatmullRom` is a smooth curve through every key (centripetal parameterization, so it cannot loop or
  cusp; the first and last segments mirror their end keys). Time maps linearly onto each segment, so
  speed changes at keys unless they are evenly spaced along the curve. Before the first key and after the
  last, the camera holds the end positions.
- **Look-at.** `Heading` looks along the direction of travel (a ±1/120 s difference of the path; a still
  path falls back to first-to-last key, then to -Z). `Point` is a fixed world point. `Track` is a moving
  target interpolated like the path.
- **Orientation.** The camera looks down -Z with +Y up, as every Regia Engine camera. Looking straight up or
  down, or at its own eye, picks a fixed frame instead of failing. Positive `roll` rolls the camera
  clockwise as seen from behind it.
- **Rotation keys (v2).** With a non-empty `rotation`, the keyed orientation replaces the look-at rule for
  the camera's rotation. Between two keys the orientation is a spherical linear interpolation along the
  shortest arc (`q` and `-q` are the same orientation); `Smooth` smoothsteps the parameter, `Step` holds
  the earlier key. Before the first key and after the last it holds. `roll` is applied on top, about the
  keyed view axis. `look_at` still gives `ShotSample::target` and the `focus_on_target` distance, so keep
  it pointing at the subject when using depth of field. Empty (the default) keeps the v1 behavior.
- **Tracks.** Hold the first value before the first key and the last value after the last. `ease` on a
  key shapes the segment that starts at it; `Step` holds that key's value.
- **Cuts.** Shot *k* covers `[start, start + duration)`. A time exactly on a boundary belongs to the shot
  that starts there. The end of the sequence holds the last frame of the last shot. A time outside the
  sequence is clamped into it; a non-finite time reads as 0.
- **Fades.** Opacity of `fade_color` over the finished frame: 1 at the start of a shot with `fade_in`,
  falling linearly to 0 at `fade_in`; rising from 0 at `duration - fade_out` to 1 at the end. A fade
  through black across a cut is `fade_out` on the shot before and `fade_in` on the shot after. The engine
  reports the opacity (`ShotSample::fade`) and draws it: the compositor's fade layer blends `fade_color`
  over the finished frame, above the UI, in windows, in screenshots with `window_layers` and in
  render-out frames (ADR-1522).

## Validation

`ShotSequence::validate` (run by `from_ron` and by every agent call that takes a sequence) rejects: no
shots, empty or duplicate names, non-positive or non-finite durations, an empty path, key times that are
unsorted, repeated, outside the shot or non-finite, non-finite positions, lens values outside their
ranges, rotation keys that are unsorted or have a zero or non-finite quaternion, fades longer than the shot, `fade_color` outside 0..=1, and `fps` outside (0, 1000]. Each message
names the shot and the field.

## Versions

| Version | Change | Migration |
| --- | --- | --- |
| 1 | First version. | — |
| 2 | Optional `rotation` track per shot. | v1 → v2: the number only. |
| 3 | Path and look-at keys may pass the shot's end (tail control keys of a shortened Catmull-Rom shot, ADR-8640). Same shape. | v2 → v3: the number only (`regia upgrade-project` loads and re-saves; a second run changes nothing). |

A v0.5 build (supported version 2) refuses a v3 file with `shot sequence version 3 is newer than supported
version 2; upgrade the engine`. The corpus fixtures are `crates/regia_agent/tests/fixtures/format-corpus/shots/`
(`v1.shots.ron`, `v2.ron`, `v3.shots.ron`, which has a Catmull-Rom path key and a look-at key past its shot's end).

## Changing the format

A change to the shape needs a version bump and a migrator (`book/changing-a-content-format.md`): bump
`SHOT_SEQUENCE_FORMAT_VERSION`, add the `VersionStep` in `shot_sequence_migrations()` and a migrator in
`regia upgrade-project`, add a v1 fixture, then `cargo xtask check-formats --update`.
