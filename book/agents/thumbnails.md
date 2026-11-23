# Batch thumbnails

## `editor.thumbnails`

Many small renders of models or entities in one call, on one warm renderer. For candidate cards,
library cards, feed tiles and multi-view proofs. Params: `jobs` (required, 1 to 256) and `size`
(default picture size, 128).

A job is `{ path | entity, view?, yaw?, pitch?, size?, debug? }`:

- `path` (project-relative or `builtin:cube`) loads a model; `entity` (bits) draws that entity and
  its descendants (and model parts) from the open world. Give exactly one.
- `view` is `perspective` (default, three-quarter), `top`, `front` or `side`. `yaw` and `pitch`
  (radians) win over `view`; yaw 0, pitch 0 is the front.
- `size` is the square picture's side in pixels, clamped to 16..=2048.
- `debug` is `lit` (default) or `wireframe` (green triangle edges over the lit picture).

```json
{ "method": "editor.thumbnails", "params": { "size": 96, "jobs": [
  { "path": "assets/chair.glb" },
  { "path": "assets/chair.glb", "view": "side" },
  { "path": "assets/chair.glb", "yaw": 2.4, "pitch": 0.3, "debug": "wireframe" },
  { "entity": 4294967301, "size": 64 } ] } }
```

Returns `images`, one per job in order: `{ index, width, height, mime, png_base64 }`, and `stats`:
`jobs`, `rendered`, `failed`, `sizes` (distinct sizes drawn), `cpu_ms`, and the frame-path counters
the call caused (`pipelines_created`, `bind_groups_created`, `textures_created`, `buffers_created`).

Every job is framed by its own bounds on the asset viewer's stage, so a picture depends only on
its job: equal jobs give equal pixels, in any batch. Jobs run grouped by size, so each render
target is allocated once per size. A job with nothing to draw (an entity without meshes) comes
back as a blank (transparent) picture of its size and counts in `failed`.

Check it worked: call twice; the second call's `stats.pipelines_created` is 0 and equal jobs have
equal `png_base64`. Compare a `lit` and a `wireframe` job of the same model: they differ.

The Rust API is `regia_editor::thumbnails::ThumbnailRenderer` (`new(&device)`, `render(&jobs)`,
`render_in(&mut world, &jobs)`).

The request limits are 1–256 nonempty `jobs`; top-level `size` defaults to 128, per-job `size` inherits
it, and both clamp to 16–2048 pixels. `path` must be a string project path (or `builtin:cube`); `entity`
is an unsigned entity-bits integer. Exactly one subject is required. `view` defaults to `perspective`
and accepts `top`, `front`, or `side`; numeric `yaw`/`pitch` override it and each missing angle defaults
to zero. `debug` defaults to `lit` and also accepts `wireframe`. Invalid request shape/view/debug uses
`F0002`; an asset that cannot be loaded returns `F0003` (`not_found`). Workflow: submit a small batch,
inspect each returned PNG, adjust view/size, then repeat the same batch to confirm reuse via the stats.

Captured failing call (lavapipe; `jobs` cannot be empty):

```console
$ regia agent call editor.thumbnails '{"jobs":[]}' --project samples/arena
error: method call failed: `jobs` (a non-empty array) is required (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```
