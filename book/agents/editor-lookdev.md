# Look-dev panel (`editor.lookdev_trace`, `editor.lookdev_status`, `editor.lookdev_cancel`)

The editor's **Look-dev** panel (`docs/specs/27-renderer-excellence.md` § 1.1, ADR-5896; panel id `lookdev`,
Window ▸ panels, also a tab of the default bottom group) traces the open level with the reference path tracer
(`regia_pt`): the ground truth every real-time technique is scored against. Editor only: the tracer is never part of a
game (`xtask check-layers` fails a runtime crate or a game sample that declares it or turns it on, with `lookdev`'s
offline plate tools as the one exception).

## What it traces

- `viewport` (default): the level through the **editor camera** (the perspective viewport's eye, target and field of
  view).
- `game`: the level through its own game camera (the first window camera), as the game would frame it.
- `cornell`, `furnace`: the tracer's own test scenes. They need no level, so they tell you the tracer runs on this
  machine.

The level is read through the same runtime extraction the real-time frame uses, then `regia_pt::extract`: the same
meshes, materials, lights, environment and atmosphere, with no second scene description. Editor-only helpers (the
preview sun of a level with no sun, proxy cubes, the backdrop) are **not** traced: a level without a sun traces dark
where the viewport shows the preview sun. `scene_info.sun_lux` is `null` then.

Each trace runs on its own thread with its own GPU device, so the editor keeps drawing while it traces, and a device
loss in the trace does not reach the editor's device. One trace runs at a time.

## The panel

Scene, size, preset (`none`, `draft`, `preview`, `final`, `reference`; see [PT presets](pt-presets.md)), samples per
pixel, maximum bounces (12 by default, specs/27 § 1.1), seed, and an output directory (empty: the project's
`.regia/lookdev/trace-<pid>-<n>/`, or the temp directory without a project). **Trace** starts; **Cancel** stops after
the sample in flight. The progress bar counts samples per pixel. When the trace finishes, the panel shows the preview
and the written files; click a path to copy it.

A preset uses its own sample count and path length unless you changed them in the panel (or gave them to
`editor.lookdev_trace`).

## Files

In the output directory:

- `reference.exr`: layered linear EXR, 32-bit float, every specs/27 § 1.2 AOV (`beauty`, `albedo`, `normal`, `depth`,
  `direct`, `indirect_diffuse`, `indirect_specular`, `emissive`, `volumetric`, `sky`, `shadow`, `variance`, `spp`)
  plus the metadata (scene hash, seed, spp, device, git tag, wall time);
- `reference_preview.png`: the AgX preview of the raw trace;
- `reference.json`: the metadata;
- `preview.png`: only for a preset that denoised or filtered; the panel shows it. It is never a reference:
  `seeing.compare` refuses it as `a`.

Compare a trace with a real-time frame with `seeing.compare` (`a` = `reference.exr`), see
[Renderer excellence](renderer-excellence.md).

## `editor.lookdev_trace`

Mutating, permission `assets`. Starts a trace and returns the status (the `editor.lookdev_status` shape). Settings
not given keep the panel's, and the panel shows what an agent asked for.

| Param | Type | Meaning |
|---|---|---|
| `scene` | string | `viewport` (default), `game`, `cornell`, `furnace` |
| `width`, `height` | number | 1..=4096 (panel default 320 × 180) |
| `spp` | number | 1..=65536 (default 64; a preset's own unless given) |
| `max_bounces` | number | 1..=64 (default 12) |
| `seed` | number | trace seed: the same seed gives the same image on one device |
| `preset` | string | `none`, `draft`, `preview`, `final`, `reference` |
| `ev100` | number | preview exposure; default the scene's sun-lit level, else 0 |
| `wait` | boolean | block the editor until the trace finishes (small traces only) |
| `out` | string | output directory, the panel's **Output dir** (ADR-7747): a relative path is under the project; any other path must lie under the project, the temp or working directory or an `output_roots` entry, without `..` (`path_outside_project`); absent or blank uses `.regia/lookdev/trace-<pid>-<n>` |

## `editor.lookdev_status`

Read-only. The current or last trace:

```json
{ "job": 1, "state": "done", "running": false, "scene": "viewport", "width": 320, "height": 180,
  "preset": null, "seed": 0, "spp": 64, "spp_done": 64, "fraction": 1.0,
  "out_dir": "…/.regia/lookdev/trace-4242-1",
  "scene_info": { "scene": "viewport", "instances": 12, "meshes": 9, "triangles": 30412, "materials": 7,
                  "terrain": false, "sun_lux": 100000.0, "local_lights": 2 },
  "exr": "…/reference.exr", "preview_png": "…/reference_preview.png",
  "raw_preview_png": "…/reference_preview.png", "metadata_json": "…/reference.json",
  "ev100": 14.9, "wall_ms": 2210.5, "traced": true,
  "settings": { "scene": "viewport", "width": 320, "height": 180, "spp": 64, "max_bounces": 12, "seed": 0,
                "preset": null, "ev100": null } }
```

`state` is `idle` (no trace yet), `running`, `done`, `cancelled` (stopped early; the files hold what was traced, and
`traced: false` with no files when it stopped before the first sample) or `failed` (`error` says why, for example
`no GPU for the trace: …`). `last_error` repeats the last refused start.

## `editor.lookdev_cancel`

Mutating, permission `assets`. Stops the running trace after the sample in flight; `cancelled` is false when none
was running. `wait: true` returns once the trace thread has stopped (then `state` is `cancelled`).

## Errors

Refusals carry `data.lookdev_error`:

| Code | When |
|---|---|
| `lookdev_busy` | a trace is already running |
| `path_outside_project` (in `data.reason`) | `out` leads outside the allowed output roots (an absolute path elsewhere, `..`, a drive or UNC path, a symlink leading out); nothing is written |
| `lookdev_nothing_to_trace` | the level has no mesh (`viewport`, `game`) |
| `lookdev_no_game_camera` | `scene: "game"` and the level has no game camera |
| `lookdev_invalid_settings` | a panel setting out of range (agents get `invalid_params` instead) |
| `lookdev_spawn_failed` | the trace thread could not start |

## Checking it

```sh
regia agent call editor.lookdev_trace '{"scene":"cornell","width":64,"height":64,"spp":16,"wait":true}'
regia agent call editor.lookdev_trace '{"width":320,"height":180,"preset":"draft"}'
regia agent call editor.lookdev_status
```

Tests: `regia_editor` `lookdev_panel::tests` (`spec27_1_1_*`; the trace test runs on lavapipe and skips without a
Vulkan device), `xtask` `layers::tests::spec27_1_1_*` for the layer rule.
