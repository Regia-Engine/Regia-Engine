# Plates and the reference overlay

Spec 27 § 5 gives the look reviews two agent tools: `seeing.capture_plate` renders a named shot to
files with a sidecar, and `render.set_reference_overlay` draws a reference image over the live frame
so a technique can be judged against it. Decision: ADR-2463. The shot list format is
[`.shot.ron`](../../docs/formats/shot-ron.md).

## `seeing.capture_plate`

A plate is one frame of a render-out job (`render.out_start`, see [render-out](render-out.md)) taken
after the shot is applied. Start one with a `shot` object, or with `shots_file` (a `.shot.ron`) and
the shot's `name`:

```
{"method": "seeing.capture_plate", "params": {
  "shot": {"name": "wide_noon",
           "camera": {"eye": [0, 2, 8], "target": [0, 1, 0], "fov_y_deg": 50},
           "time_of_day": {"hour": 12}, "profile": "desktop_high",
           "width": 1280, "height": 720, "warmup_frames": 8, "aovs": ["depth"]},
  "out": "plates", "tag": "pin1234"}}
```

The shot fields are `name`, `camera` (`{eye, target, fov_y_deg?}` moves the scene's first camera, or
`{name}` renders through the camera entity of that name: its `order` is raised above every other
camera's so its view is drawn last, and it stays raised after the plate), `time_of_day` (`{hour, day_of_year?, latitude_deg?}`,
written to the level's atmosphere clock), `profile` (a preset name `render.get_profile` lists),
`width`/`height` (default 1280 x 720), `warmup_frames` (default 8), `aovs` (`depth`, `id`, `motion`),
`format` (`png` or `exr`) and `level` (recorded only: the host loads levels). `out` defaults to
`plates` and is confined like `render.out_start`: the project, the temp dir and
`agent.toml` `output_roots`. `tag` is appended to the file stem (`<name>-<tag>`).

The call returns `{plate, job, status: "running"}` at once. Poll with `{"plate": id}` until
`status` is `done`; the files are then complete and the sidecar `<stem>.plate.json` has been written
(a `.part` file, then a rename). The frame itself is `<stem>_000000.png` (or `.exr`), with AOV files
beside it and the render-out manifest `<stem>_render_out.json`.

The sidecar holds `schema` (`regia.plate/1`), the engine version, the shot as taken, the profile
preset and the camera as rendered (`eye`, `forward`, `fov_y_deg`; both taken when the frame was delivered, so a
plate polled after a later plate moved the camera still records its own), the render-out `frame_hash` and,
for every file, its name, size and BLAKE3 hash. It holds no time stamp: the same shot on the same
build gives the same file hashes (lavapipe: byte for byte).

### Error reasons

Errors carry the usual `F` code and a lowercase `data.reason`:

| Reason | Meaning |
| --- | --- |
| `bad_shot` | The shot is missing a field, has an unknown field, or a value is out of range. |
| `shot_list_invalid` | `shots_file` is missing, unparsable or fails the `.shot.ron` checks. |
| `shot_not_found` | `name` is not a shot of the list. |
| `path_outside_project` | `out` or `shots_file` is outside the allowed roots. |
| `camera_not_found` | A named camera does not exist, or the scene has no camera to move. |
| `unknown_profile` | `profile` is not a preset name. |
| `no_renderer` | The app has no `RenderPlugin`. |
| `plate_not_found` | `plate` is not a started plate (only the last 32 are kept). |
| `plate_failed` | The render-out job ended in failure, or its files could not be read. |

A busy renderer (another render-out running) answers `F0018`, as `render.out_start` does.

## `render.set_reference_overlay`

Draws a reference PNG over the frame at the `overlay` slot, after the post stack and before the UI.
Off by default: the pass is registered on the first call, and mode `off` (or no image) does no GPU
work.

```
{"method": "render.set_reference_overlay", "params": {"mode": "wipe", "path": "plates/ref.png", "split": 0.5}}
```

| Mode | What it draws |
| --- | --- |
| `off` | Nothing. |
| `blend` | The reference mixed over the frame by `opacity` (default 0.5). |
| `wipe` | The reference right of `split` (a fraction of the width, default 0.5), the frame left of it. |
| `difference` | `abs(frame - reference)` per channel, mixed by `opacity` (default 1). A frame against itself is black. |
| `flip` | A perceptual colour-difference heat map (CIELAB HyAB distance through a black-blue-magenta-orange-yellow ramp). A per-pixel stand-in: the FLIP score of `seeing.compare` stays the authoritative number. |

The PNG is read once and cached by file (path, size and modification time); a later call with the
same file, or with no `path`, only changes mode, opacity or split. The answer reports `loads` (files
decoded), `uploads` (GPU uploads), `draws` and `failures`. `difference` and `flip` copy the display
target once per frame; a target that cannot be copied counts a failure and the frame goes on
without the overlay.

**Size mismatch.** The reference is fitted to the frame's view. An aspect within 1 % of the view's
covers the whole view (bilinear, or read pixel for pixel when the sizes are equal). Any other aspect
is letterboxed: the whole reference is visible, centered, and the frame shows around it.

### Error reasons

| Reason | Meaning |
| --- | --- |
| `unknown_mode` | `mode` is missing or not `off`, `blend`, `wipe`, `difference` or `flip` (lowercase). |
| `bad_opacity` | `opacity` is not a number in 0..=1. |
| `bad_split` | `split` is not a number in 0..=1. |
| `path_required` | A mode other than `off` was set with no image loaded and no `path`. |
| `path_outside_project` | `path` is outside the allowed roots. |
| `image_not_found` | `path` does not exist or cannot be read. |
| `image_unreadable` | `path` is not an 8-bit PNG the decoder accepts. |
| `image_too_large` | The image is over 4096 pixels a side, or the file is over 128 MiB. |
| `no_renderer` | The app has no `RenderPlugin`. |
