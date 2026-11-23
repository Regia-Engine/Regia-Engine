# Camera paths and shot sequences (agents)

A shot sequence is a camera move written as data: where the camera is, where it looks, its field of
view and depth of field, the cuts between shots and the fades at their edges. The same data plays in
a game, renders frame-exact for a video, and is what a person edits by hand (or, later, in an editor
panel). Guide topic: `agent.guide {"topic": "camera_paths"}`. Format: `docs/formats/shots-ron.md`.

Everything is a pure function of a time value. Seeking to 3.5 s gives the same pose on every machine
and every frame rate; there is no wall clock in it. Playing advances the clock by the game's time
step, so `play.pause`, `play.step` and the time scale all apply.

## The model

- A **sequence** is shots played back to back. Each **shot** has its own clock starting at zero.
- The boundary between two shots is a hard **cut**. A time exactly on a boundary belongs to the shot
  that starts there. The end of the sequence holds the last frame.
- A shot has a **path** (position keys with `Linear`, `Step` or `CatmullRom` interpolation), a
  **look-at** rule (`Heading` = along the direction of travel, `Point`, or a moving `Track`), and
  tracks for **fov** (degrees), **roll** (degrees) and optional **dof** (focus distance, aperture,
  `focus_on_target`, `bokeh`). `fade_in` and `fade_out` are seconds at the shot's edges.
- A shot may key its **rotation** (v2): `[{time, rotation: [x, y, z, w], ease?}]`, a unit quaternion
  per key, interpolated by spherical interpolation along the shortest arc (`Smooth` smoothsteps the
  parameter, `Step` holds). Keyed rotation replaces the look-at rule for the camera's orientation;
  `roll` still applies, and `look_at` still gives the sample's `target` (and the `focus_on_target`
  distance). Without keys the orientation comes from `look_at` as before.
- A **path** or `Track` **look-at** key may sit past its shot's end (v3, ADR-8640): a shortened
  Catmull-Rom shot keeps its tail keys as control keys, so the curve before the cut is unchanged; the
  shot's clock never reaches them. A v2 file loads as v3 (`regia upgrade-project` rewrites only the
  number); a build that reads v2 refuses a v3 file with `shot sequence version 3 is newer than
  supported version 2; upgrade the engine`.
- The **fade** is drawn: the compositor's `fade` layer blends `fade_color` over the finished frame, above
  the UI. It shows in windows, in `seeing.screenshot` with `window_layers: true`, in screenshots taken
  while a fade is visible, and in `render.out_start` frames, so a render-out fades like the preview.
- Every shot change, seek into another shot and loop wrap is counted as a camera cut and the renderer
  drops its temporal history ([camera cuts](camera-cuts.md)), so no frame blends the old shot into the new.

## Methods

- `render.sequence { action?, ... }` runs the player. `action` is one of:
  - `load` with `ron` (the `.shots.ron` text) or `sequence` (the same data as JSON; enums are
    externally tagged: `"Heading"`, `{"Point": [0, 1, 0]}`). Also `play`, `loop`, `camera`.
  - `seek` with `time` (seconds) or `frame` (at the sequence's `fps`).
  - `sample` with `time` or `frame`: the pose at any instant without moving the clock.
  - `play` (optional `time`/`frame` to start from, `loop`, `camera`), `pause`, `stop` (back to zero).
  - `export`: the canonical RON, which loads back to the same sequence.
  - `query` (the default): the outline, the clock, the cut count and the current sample.
  - `clear`: unload.
- `camera.set_path { path, shot?, ... }` authors one shot. With `shot` it replaces that shot's path
  in the loaded sequence (the clock and play state stay); without it, it creates a one-shot sequence
  named `name` (default `adhoc`) and loads it. Optional `duration`, `fov`, `roll`, `rotation` (`null` clears the
  keys), `dof` (`null` removes it), `fade_in`, `fade_out`, `camera`, `play`. `preview: n` (0 to 64) returns `n` evenly spaced samples
  (eye, target, field of view) so a move can be checked without playing it, and `dry_run: true`
  validates and previews without applying. Everything is validated; an error names the shot and field. Without `duration` the new shot lasts
  until its last key (give `duration` when that key is at time zero). The reply has `shot`, `index`,
  `created`, `applied`, `duration`, `keys`, `preview` and, when applied, `state`.
  Errors: `INVALID_PARAMS` for a bad field or a path that fails validation; `NOT_FOUND` for a `shot`
  that is not in the loaded sequence (the hint lists the shot names) or when `shot` is given and no
  sequence is loaded.
- `camera.raycast { u, v | x, y, ... }` turns a point of the camera's image into a ray and casts it
  like [`world.pick`](world-pick.md): `hit` (`entity`, `point`, `normal`, `distance`, `layer`) or, with
  `all_layers`, `hits`. `x`/`y` are pixels from the top-left corner (a pixel centre is at +0.5),
  `u`/`v` the same point as fractions; the image size is `width`/`height` or the primary window.
  `entity` names a camera (default: the `Camera` with the highest `order`); `ray_only` skips the cast.
  It uses the camera's pose, field of view or orthographic size. CPU only, no window needed.

Every `render.sequence` reply has `action`, `state` (`loaded`, `playing`, `time`, `cuts`,
`camera`, and the outline and current sample once loaded) and `fade_layer` (`installed`: the fade layer
has been added to the window overlay, which happens the first frame a sequence shows a fade; `amount` and
`color`: what it draws; it follows the sample from the next frame); `sample` adds `sample` and `export` adds
`ron`. `seek`, `sample` and `play` before anything is loaded fail with `NOT_FOUND`. `time` and
`frame` together are `INVALID_PARAMS`. A sample has `time`, `shot`, `shot_name`, `local_time`, `eye`,
`target`, `up`, `rotation`, `roll_degrees`, `fov_y_degrees`, `dof`, `fade`, `fade_color` and
`finished`.

`camera.raycast` returns `camera`, `origin`, `direction` and `screen` (`x`, `y`, `u`, `v`, `width`,
`height`, `aspect`) plus the pick result unless `ray_only`. It fails with `NOT_FOUND` when no `Camera`
exists or `entity` has none.

`render.sequence` and `camera.set_path` need the `edit_scene` permission (they move the camera);
`camera.raycast` is read-only.

```json
{"method": "camera.set_path", "params": {
  "path": {"interp": "CatmullRom",
           "keys": [{"time": 0, "position": [0, 2, 12]}, {"time": 2, "position": [4, 2, 7]}, {"time": 4, "position": [6, 2, 3]}],
           "look_at": {"Point": [0, 1.5, 0]}},
  "fov": [{"time": 0, "value": 40}, {"time": 4, "value": 28, "ease": "Smooth"}],
  "fade_in": 0.5, "preview": 5}}
{"method": "render.sequence", "params": {"action": "seek", "time": 2.0}}
{"method": "seeing.screenshot", "params": {}}
{"method": "camera.raycast", "params": {"u": 0.5, "v": 0.5}}
```

More calls, one per method and action:

```json
{"method": "render.sequence", "params": {"action": "load", "ron": "(version: 1, shots: [ ... ])", "loop": true, "play": true}}
{"method": "render.sequence", "params": {"action": "sample", "frame": 45}}
{"method": "render.sequence", "params": {"action": "export"}}
{"method": "camera.set_path", "params": {"shot": "b", "path": {"keys": [{"time": 0, "position": [0, 5, 0]}, {"time": 3, "position": [0, 5, 6]}]}, "dry_run": true, "preview": 3}}
{"method": "camera.raycast", "params": {"x": 960, "y": 540, "width": 1920, "height": 1080, "entity": 8589934592, "all_layers": true}}
```

## Errors

| Call | Condition | Code |
| --- | --- | --- |
| `render.sequence` | `play`, `seek`, `sample`, `export` with no sequence loaded | `NOT_FOUND` |
| `render.sequence` | unknown `action`; `time` and `frame` together; negative or non-numeric `time`/`frame`; a sequence that fails validation (duplicate or empty shot names, `fps` outside (0, 1000], fades longer than the shot, keys before time zero or not increasing, keys past the shot's end on any track but the camera path and look-at (those are control keys, ADR-8640), a zero or non-finite rotation quaternion, values outside the track limits, unknown `version`); `ron` and `sequence` together, or `load` with neither; more than 256 shots or more than 4096 keys on one track | `INVALID_PARAMS` |
| `camera.set_path` | `shot` names no shot of the loaded sequence, or there is no sequence | `NOT_FOUND` |
| `camera.set_path` | missing or invalid `path`, `duration` not greater than zero (or the last key is at time zero with no `duration`), `preview` above 64, a track out of range or with more than 4096 keys | `INVALID_PARAMS` |
| `camera.raycast` | no `Camera` in the app, `entity` is not a camera | `NOT_FOUND` |
| `camera.raycast` | no point, a point outside the image (`u`/`v` outside 0..=1; the edges are inside), only one of `width`/`height`, a non-positive `aspect` | `INVALID_PARAMS` |

A rejected call changes nothing: the loaded sequence, the clock and the cut count are as before.
`seek` to the same time twice, replacing a shot with the same path twice, `pause`, `stop` and `clear`
are idempotent. `sample`, `export`, `camera.raycast` and a `dry_run` never change the player.

## Typical loop

1. Write the move: `camera.set_path` with `preview`, or `render.sequence {action: "load", ron: ...}`
   for several shots.
2. Check it without playing: `render.sequence {action: "sample", time}` at the keys, at each side of a
   cut and inside the fades.
3. Look at it: `render.sequence {action: "seek", time}`, then `seeing.screenshot` on a later call (a
   call lands at a frame boundary and the camera moves later in that frame). Screenshots of a seek
   are frame-exact; nothing needs stepping.
4. Verify what is in frame: `camera.raycast` at the image centre or at a thing that must be visible.
5. Keep it: `render.sequence {action: "export"}` returns the canonical text to save as `*.shots.ron`.
6. For motion: `render.sequence {action: "play"}`, then `play.step` for fixed steps.

## Notes

- For the file format, the game-side API and what is not included yet, see
  [Camera shot sequences](../user-guide/shot-sequences.md).

- Depth of field needs a quality profile that allows it. The sequence sets the camera's
  `PhysicalCamera` focus distance, aperture and mode, and turns its motion blur off when it creates
  the component; an existing `PhysicalCamera` keeps its other settings.
- The fade is drawn when a `WindowOverlay` exists: the compositor blends `fade_color` over the
  finished frame, above the UI. With no overlay nothing is drawn, and the fade stays on the sample
  (`fade`, `fade_color`).
- Do not put a `CameraRig` on the camera a sequence drives: the rig would overwrite the pose.
- Paths kilometres from the world origin pan smoothly (ADR-1488): the renderer keeps the view
  direction to `f32` precision, and `Heading` shots take their direction of travel in `f64`. To
  check a slow pan for ticks, compare frame-to-frame image shifts of a distant region: the shift
  should change smoothly, not jump by a pixel at a fixed period. A camera's roll does not reach
  the image yet (main views render with +Y up).
- Game code uses the same player:

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let frame: u64 = 0;
use regia_framework::shots::{ShotPlayer, ShotPlugin, ShotSequence};

app.add_plugins(ShotPlugin); // AgentPlugin adds it too
let sequence = ShotSequence::from_ron(&std::fs::read_to_string("intro.shots.ron")?)?;
app.world_mut().resource_mut::<ShotPlayer>().load(sequence.clone());
app.world_mut().resource_mut::<ShotPlayer>().play();
// Or sample by hand, e.g. per rendered video frame:
let pose = sequence.sample_frame(frame); // Option<ShotSample>: eye, target, rotation, fov_y_degrees, dof, fade
```

### More `camera.raycast` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `max_distance` (number): Ray length cap, metres (world.pick filter).
- `layers` (array): Accepted MapLayer ids (world.pick filter).
- `cull` (string): "none" or "back" (world.pick filter).
- `terrain` (boolean): Cast against terrain (default true).
- `meshes` (boolean): Cast against meshes (default true).
- `include_hidden` (boolean): Also test hidden entities.
- `skip` (array): Entity ids to ignore.
