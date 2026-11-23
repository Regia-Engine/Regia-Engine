# Independent viewports

One world can have several viewports open at once: the editor's main viewport, plus any number
of `ViewportInstance`s that a tool draws (a review lightbox, a before/after toggle, a
side-by-side concept compare, a candidates grid). Each instance has its own camera, quality
preset, single/quad layout, show flags and game view. Instances in one *link group* orbit
together.

These methods see and drive the instances. The main viewport keeps its own methods
(`editor.capture_viewport`, `editor.set_viewport`, `editor.focus`; see [Editor surface](editor.md)).
The selection, gizmo mode, bookmarks and undo stack belong to the world, so every viewport
shares them.

## Methods

| Method | What it does |
|---|---|
| `editor.viewport_instances {}` | Lists every instance: `id`, `live` (a panel drew it this frame or the last), `camera` (`target`, `distance`, `yaw`, `pitch`, `fov_deg`, `eye`), `quality`, `layout` (`single`/`quad`), `active` pane, `game_view`, `possessed`, `show` flags, `settings` (`grid`, `labels`, `realtime`, `audio`), `link` (group id or null), the `panes` of its last draw (`index`, `kind`, `eye`, `target`, `width`, `height`) and `fps`. |
| `editor.viewport_instance_set { id, create?, target?, distance?, yaw?, pitch?, fov_deg?, quality?, layout?, grid?, labels?, realtime?, audio?, icons?, bounds?, outline?, proxies?, stats?, preview_light?, game_view? }` | Edits one instance. The panel applies the edit on its next draw; captures see it at once. On a linked instance, a camera edit moves the whole group. `create: true` registers an unknown id, seeded from the main viewport, for headless captures. |
| `editor.viewport_link { id, group? }` | Links the instance's camera to group `group`. If another instance already holds that group it joins it and adopts the group's camera; otherwise it starts the group with its own camera. `group: null` (or absent) unlinks; the camera stays where it is. |
| `editor.capture_viewport_instance { id, view?, width?, height?, quality?, game_view?, warm_frames? }` | Renders the instance headless with its own camera, preset and show flags, overlays painted in, and returns `png_base64`, the `eye` and `target` it used, the viewport measurements (`sky_luminance`, `lit_peak`, `shading_range`, `selection_highlight`, `grid_pixels`, `gizmo_axes`), `quality` and `info` (as in `editor.capture_viewport`). `quality` and `game_view` apply to this call only. A linked instance first follows its group. |
| `editor.viewport_instance_remove { id }` | Forgets the instance: its registry entry, link membership and the renderers it owns. A panel that draws it again registers it afresh. |

Errors: an unknown `id` is `not_found` on `editor.viewport_instance_set` (unless `create: true`),
`editor.viewport_link` and `editor.capture_viewport_instance`. `editor.viewport_instance_remove` of an id
that was never registered returns success `{ "id", "removed": false }` and does not change the registry.
An unknown `quality`, `layout` or `view` name is `invalid_params`. `editor.viewport_instance_set`,
`editor.viewport_link` and `editor.viewport_instance_remove` need `edit_scene` and are mutating
(ADR-3285): they change the registry a later `editor.viewport_instances` reads.

## Recipes

**A/B compare at one camera.**

```
editor.viewport_instance_set {"id": 1, "create": true, "yaw": 0.6, "distance": 4}
editor.viewport_instance_set {"id": 2, "create": true, "quality": "game"}
editor.viewport_link {"id": 1, "group": 7}
editor.viewport_link {"id": 2, "group": 7}        # 2 adopts 1's camera
editor.capture_viewport_instance {"id": 1}
editor.capture_viewport_instance {"id": 2}        # same eye, different preset
```

**Orbit both.** `editor.viewport_instance_set {"id": 2, "yaw": 1.2}` moves 2 and, through the
group, 1. Check with `editor.viewport_instances`: both `camera.yaw` values match.

**Split them.** `editor.viewport_link {"id": 2, "group": null}`, then move 2 alone.

**Per-view show flags.** `editor.viewport_instance_set {"id": 2, "game_view": true}` hides the
helpers in 2 only. A capture of 1 still has the grid (`grid_pixels` > 0).

## Notes

- Instance ids are chosen by the tool that draws them. Keep them unique per world.
- Linking copies the whole perspective camera (target, distance, yaw, pitch, field of view, fly
  speed). Orthographic panes of a quad layout keep their own cameras.
- In the live panel, a pane drawn earlier in the frame follows a later pane's move on the next
  frame (one frame of lag). When two linked panes move in the same frame, the first one drawn
  wins.
- When several viewports draw through a world's shared `RuntimeRenderer` in the same frame,
  temporal upscaling (TAAU) is off for all of them, so one view's history never leaks into
  another's. `info.taau` reports it.

## Idle repaint

With realtime disabled, a viewport does not request another egui frame simply because it was
drawn. Pointer, keyboard, SpaceMouse, linked-camera synchronization, and other editor changes
still trigger redraws as needed. The stats overlay also stays idle when its displayed values are
unchanged; a changed displayed value can request a redraw.

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `editor.capture_viewport_instance`

Render one viewport instance headless with its own camera, quality preset and show flags (overlays painted in) and return a PNG, the eye it used, measurements and frame statistics.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | number | yes | Viewport instance id. |
| `view` | string | no | Pane: perspective (default), top, front or side. |
| `width` | number | no | Frame width in pixels. Default 320, clamped to 64..=2048. |
| `height` | number | no | Frame height in pixels. Default 240, clamped to 64..=2048. |
| `quality` | string | no | Preset for this call only. Default: the instance's. |
| `game_view` | boolean | no | Hide every editor helper for this call. Omitted uses the instance’s game_view. |
| `warm_frames` | number | no | Frames rendered before the kept one. Default 0; capped at 16. |

Result: `{ id, width, height, mime, png_base64, view, eye, target, sky_luminance, lit_peak, shading_range, selection_highlight, grid_pixels, gizmo_axes, quality, info }`.

Errors besides the universal codes: `F0002`, `F0003`, `F0011`.

`F0011` when the adapter returns no image. Width and height default to 320 and 240 and clamp to 64..=2048. `warm_frames` clamps to 0..=16.

```json
editor.capture_viewport_instance {"id": 7, "width": 64, "height": 64, "warm_frames": 0}
→ {
  "eye": [
    4.067445755004883,
    4.479723930358887,
    5.945368766784668
  ],
  "gizmo_axes": 0,
  "grid_pixels": 1548,
  "height": 64,
  "id": 7,
  "info": {
    "atmosphere": false,
    "counters": {
      "bind_groups": 9,
      "blocking_polls": 1,
      "pipelines": 20,
      "submits": 1
    },
    "cpu_ms": 4328.0009765625,
    "debug_legend": [],
    "draws": {
      "culled": 0,
      "double_sided_per_object": 0,
      "drawn": 1,
      "instanced": 0,
      "per_object": 1
    },
    "gpu_ms": {
      "atmosphere": 0.0,
      "clouds": 0.0,
      "fog": 0.0,
      "forward": 0.0,
      "frame": 0.0,
      "post": 0.0,
      "prepass": 0.0,
      "shadows": 0.0,
      "ssao": 0.0,
      "ssr": 0.0,
      "taau": 0.0
    },
    "local_lights": 0,
    "meshes": 0,
    "possessed": false,
    "preview_sun": true,
    "profile": "editor",
    "proxies": 0,
    "quality": "editor",
    "runtime_renderer": true,
    "taau": false
  },
  "lit_peak": 0.78578120470047,
  "mime": "image/png",
  "png_base64": ...,
  "quality": "editor",
  "selection_highlight": false,
  "shading_range": 0.007009446620941162,
  "sky_luminance": 0.8894098401069641,
  "target": [
    0.0,
    1.0,
    0.0
  ],
  "view": "perspective",
  "width": 64,
  ...
}
```

Elided: `png_base64` (png 4628 chars).

<!-- capture:editor.capture_viewport_instance -->

### `editor.viewport_instance_remove`

Forget a viewport instance: its registry entry, link membership and viewport-owned renderers. A panel that draws it again registers it afresh.

*EditScene, mutating. Changes the editor viewport registry.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | number | yes | Viewport instance id. |

Result: `{ id, removed }`. `removed` is false when that id was never registered; that call is not `not_found`.

Errors besides the universal codes: `F0002`, `F0003`.

```json
editor.viewport_instance_remove {"id": 7}
→ { "id": 7, "removed": true }
```

<!-- capture:editor.viewport_instance_remove -->

### `editor.viewport_instance_set`

Edit one viewport instance: perspective camera, quality preset, single/quad layout, show flags and game view. The panel applies it on its next draw; a linked camera that moves leads its group. `create` registers a new instance (seeded from the main viewport) for headless captures.

*EditScene, mutating. Changes the editor viewport registry.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | number | yes | Viewport instance id. |
| `create` | boolean | no | Default false. Create the instance when the id is unknown; seed it from the main viewport. Omitted edit fields keep the instance’s values. |
| `target` | array | no | Orbit target [x, y, z]. |
| `distance` | number | no | Orbit distance, meters; clamped to 0.2..=5000. |
| `yaw` | number | no | Yaw around +Y, radians. |
| `pitch` | number | no | Pitch above the horizon, radians (clamped to ±1.45). |
| `fov_deg` | number | no | Vertical field of view, degrees; clamped to 5..=150. |
| `quality` | string | no | Quality preset name (editor, game, mobile_low, ..., deck). |
| `layout` | string | no | single or quad. |
| `grid` | boolean | no | Reference grid. |
| `labels` | boolean | no | Selection name labels. |
| `realtime` | boolean | no | Render every frame. |
| `audio` | boolean | no | Audio gizmos. |
| `icons` | boolean | no | Light/camera/decal/audio icons. |
| `bounds` | boolean | no | Selection bounds, light ranges, frustums. |
| `outline` | boolean | no | Selection outline. |
| `proxies` | boolean | no | Proxy cubes for entities with nothing to draw. |
| `stats` | boolean | no | Stats overlay. |
| `preview_light` | boolean | no | Preview sun when the level has none. |
| `game_view` | boolean | no | Hide every editor helper. |

Result: `the instance, as in editor.viewport_instances`.

Errors besides the universal codes: `F0002`, `F0003`.

`create: true` registers an id the list does not have yet, seeded from the main viewport. Without it an unknown id is `F0003`.

```json
editor.viewport_instance_set {"id": 7, "create": true, "distance": 8, "fov_deg": 50}
→ {
  "active": 0,
  "camera": {
    "distance": 8.0,
    "eye": [
      4.067445755004883,
      4.479723930358887,
      5.945368766784668
    ],
    "fov_deg": 50.0,
    "pitch": 0.44999998807907104,
    "target": [
      0.0,
      1.0,
      0.0
    ],
    "yaw": 0.6000000238418579
  },
  "fps": 0.0,
  "frame": 0,
  "game_view": false,
  "id": 7,
  "layout": "single",
  "link": null,
  "live": false,
  "panes": [],
  "possessed": true,
  "quality": "editor",
  "settings": {
    "audio": true,
    "grid": true,
    "labels": true,
    "realtime": true
  },
  "show": {
    "audio": true,
    "bounds": true,
    "grid": true,
    "icons": true,
    "outline": true,
    "preview_light": true,
    "proxies": true,
    "stats": false
  }
}
```

<!-- capture:editor.viewport_instance_set -->

### `editor.viewport_instances`

List the world's independent viewports (lightbox, before/after, compare panes): each one's camera, quality preset, layout, show flags, link group, the panes of its last draw and whether it is live (drawn this frame or the last).

*Read, does not mutate.*

No parameters.

Result: `{ instances: [{ id, live, frame, camera, quality, layout, active, game_view, possessed, show, settings, link, panes: [{index, kind, eye, target, width, height}], fps }] }`.

Errors besides the universal codes: `F0002`.

```json
editor.viewport_instances {}
→ { "instances": [] }
```

<!-- capture:editor.viewport_instances -->

### `editor.viewport_link`

Link a viewport instance's camera to a group (members orbit together; joining adopts the group's camera) or unlink it with `group: null`.

*EditScene, mutating. Changes the editor viewport registry.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | number | yes | Viewport instance id. |
| `group` | number | no | Link group id; null or absent unlinks. |

Result: `the instance, as in editor.viewport_instances`.

Errors besides the universal codes: `F0002`, `F0003`.

```json
editor.viewport_link {"id": 7, "group": 1}
→ {
  "active": 0,
  "camera": {
    "distance": 8.0,
    "eye": [
      4.067445755004883,
      4.479723930358887,
      5.945368766784668
    ],
    "fov_deg": 50.0,
    "pitch": 0.44999998807907104,
    "target": [
      0.0,
      1.0,
      0.0
    ],
    "yaw": 0.6000000238418579
  },
  "fps": 0.0,
  "frame": 0,
  "game_view": false,
  "id": 7,
  "layout": "single",
  "link": 1,
  "live": false,
  "panes": [],
  "possessed": true,
  "quality": "editor",
  "settings": {
    "audio": true,
    "grid": true,
    "labels": true,
    "realtime": true
  },
  "show": {
    "audio": true,
    "bounds": true,
    "grid": true,
    "icons": true,
    "outline": true,
    "preview_light": true,
    "proxies": true,
    "stats": false
  }
}
```

<!-- capture:editor.viewport_link -->

## Workflow: open a second view and capture it

1. `editor.viewport_instances` to see ids already registered.
2. `editor.viewport_instance_set` with a new `id` and `create` true, plus the orbit (`target`, `distance`, `yaw`, `pitch`) and a `quality` name.
3. `editor.viewport_link` with that `id` and a `group` when a second pane should orbit with it. `group` null unlinks.
4. `editor.capture_viewport_instance` with the `id`, a small `width` and `height`, and `warm_frames` 0 to capture without warm-up frames. The answer carries `png_base64`. `F0011` means the adapter produced no image.
5. `editor.viewport_instance_remove` when the pane is done. A panel that draws the id again registers it afresh.

<!-- /depth-2:method-reference -->

## Clip planes and 2- and 3-pane layouts (ADR-7755)

`editor.viewport_instance_set` also takes `near` and `far` (meters; near 0.001 to 100, far 1 to 100000 and at least ten
times near, checked together against the instance's current planes; a bad pair is `bad_clip` and changes nothing), and
`layout` takes `single`, `quad`, `side_by_side`, `stacked` or `one_large_two`. `editor.viewport_instances` reports each
instance's `near`, `far` and `layout` name. Every pane of a 2-, 3- or 4-pane layout keeps its own camera and view kind;
the main viewport's layout is `editor.viewport_layout` (see [Viewport tools](viewport-tools.md)).

```json
{ "method": "editor.viewport_instance_set", "params": { "id": 7, "create": true, "near": 1.0, "far": 50.0, "layout": "side_by_side" } }
```

Check it worked: `editor.viewport_instances` lists instance 7 with `near: 1.0`, `far: 50.0`, `layout: "side_by_side"`.
