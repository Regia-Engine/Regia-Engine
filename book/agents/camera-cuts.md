# Camera cuts (agents)

A frame blends in what earlier frames left: the temporal upscaler's history (TAAU or TSR), the
froxel fog's history, the static-screen SSGI/SSR cache, cached shadow cascades and pages, and the
auto-exposure meter. Across a camera cut (cockpit to chase camera, a replay cut, menu to map)
that history belongs to another view: before cuts existed, a tenth of the old image was still
visible four frames later, with TAA off as well. Guide topic: `agent.guide {"topic": "camera_cut"}`.

After a cut the next frame is the first frame a fresh renderer would draw. Pixels match within
±2 per channel (`crates/regia_render/tests/camera_cut.rs`, Desktop High and Deck, TAA on and
off, TSR). GPU resources, pipelines and loaded meshes stay, so a cut costs no more than a first
frame: every cached shadow is redrawn once and TAA starts a new accumulation.

## Methods

- `render.camera_cut { cut?, exposure?, detection? }` requests a cut on the next frame
  (`cut`, default `true`). `exposure` is `"snap"` (default) or `"fade"`. Snap forgets the metered
  exposure: the view starts at the initial exposure and adopts its own first histogram outright,
  two frames later. Fade keeps adapting from the old view, for a deliberate eye-adaptation
  effect. `detection` configures automatic cuts: `false` turns them off (explicit cuts only),
  `true` restores the defaults, and an object `{ max_eye_jump_m, max_turn_degrees,
  max_kept_share }` is merged onto the current thresholds. Returns `{ requested, exposure,
  detection, last_cut: { frame, reason } | null, cuts, frame }`. `reason` is `requested`,
  `eye_jump`, `turn` or `scene_change`. Mutating; permission `edit_scene`. `{"cut": false}` only
  reads the state and applies any `detection` change.
  Errors: `F0003` (not found) in an app without a `RuntimeRenderer`; `F0002` (invalid params) for a
  non-boolean `cut`, an `exposure` other than `snap`/`fade`, a `detection` that is not a boolean or
  object, or a negative or non-numeric threshold. A rejected call changes nothing.

Thresholds must be nonnegative and finite as `f32`; `max_kept_share` is then clamped to 1.
An oversized threshold answers `F0002` before changing detection or requesting a cut.

```json
{"method": "render.camera_cut", "params": {}}
{"method": "render.camera_cut", "params": {"cut": false, "detection": {"max_eye_jump_m": 20}}}
```

## Automatic detection

The renderer cuts on its own, with exposure snapping, when any of these happens between two
frames of the same view:

- the eye moves more than `max_eye_jump_m` (default 50 m),
- the view direction turns more than `max_turn_degrees` (default 90°),
- the frame is a scene change: less than `max_kept_share` (default 25 %) of its objects were
  drawn last frame, *and* less than that share of last frame's objects remain. A large spawn
  into a kept scene is not a cut. Objects are compared twice, by object key and by content
  (model matrix, vertex/index counts, a few vertex positions), and both must turn over. Moving
  objects keep their keys, and a tool that rebuilds the same scene every frame under new keys
  keeps its content, so neither looks like a scene change. Objects without stable keys are
  judged on content alone.

Reflection-probe faces and render-to-texture views (mirrors, UI previews) never detect or take a
cut; a pending cut waits for the host's view. Split-screen viewports drawn by one renderer each
compare with their own last frame.

## Typical loop

1. Move the camera to a new shot (`scene.set_property` on the camera, or a level load).
2. `render.camera_cut` if the game does not cut on its own, then `seeing.screenshot`: the image
   carries nothing from the previous shot.
3. Check `last_cut` in the result of `render.camera_cut {"cut": false}` to see whether the
   renderer took a frame as a cut (an unexpected `eye_jump` from a fast vehicle means the
   threshold is too low for that game).

Game code calls `RuntimeRenderer.0.camera_cut()` (or `camera_cut_with(CutExposure::Fade)`) on
the frame of the cut, and `set_camera_cut_detection` to tune or disable detection.
