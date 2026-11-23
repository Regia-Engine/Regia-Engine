# Reference image (concept ghost)

Draw a picture in a viewport pane to check the model against a concept: behind the scene, over
it, as a difference, or as an outline. The reference shows in the live panel and in every
`editor.capture_viewport` of that pane. A pane without one costs nothing.

| Mode | Draws |
|---|---|
| `behind` | The reference replaces the background: pixels no mesh covers. The lit scene stays on top. |
| `over` | The reference alpha-blended over the scene. |
| `difference` | `abs(scene - reference)` per channel: black where they agree. |
| `silhouette` | The outline of the reference's subject, magenta, over the scene. The subject is the image's alpha when it has transparency, else everything unlike the corner colors. |

`opacity` (0..1, default 0.5) scales every mode. `rect_uv` `[u0, v0, u1, v1]` is the pane
rectangle (0..1, origin top-left) the whole image is stretched into; the default `[0,0,1,1]`
fills the pane, a smaller rectangle letterboxes. Match the camera to the concept with
`editor.focus` or the camera fields first.

## Set, read, clear

`editor.set_reference`:

```json
{ "view": "perspective", "png_base64": "<PNG or JPEG>", "mode": "difference", "opacity": 0.8 }
→ { "mode": "difference", "opacity": 0.8, "rect_uv": [0,0,1,1], "width": 1024, "height": 1024 }
```

`path` names a file on the editor's machine instead of `png_base64`. A later call without an
image changes only the settings of the pane's current reference. `editor.get_reference`
returns `{ "reference": {...} | null }`; `editor.clear_reference` removes one pane's (or all
panes' when `view` is absent).

## Capture it

`editor.capture_viewport` returns the reference in the PNG (under the grid, selection outline
and gizmo) and echoes its settings in `reference` (`null` when none). For a before/after,
capture once with the reference and once after `editor.clear_reference`.

## Limits

- `behind` reads the entity-id buffer, which meshes and proxies write; terrain and
  runtime-renderer content do not, so the reference shows through them.
- The pass is CPU compositing of the finished pane, in sRGB; while the game camera is possessed
  in play, no reference is drawn.

`editor.set_reference` accepts `view` (optional, defaults to the active pane), `png_base64` or `path`
(image source), `mode` (default `over`; `behind`, `difference`, `silhouette`), `opacity` (default 0.5,
0..1), and `rect_uv` (four normalized coordinates, default full pane). `editor.get_reference` and
`editor.clear_reference` accept only optional `view`; clearing without it clears every pane. Bad input
returns `F0002`. Workflow: set the reference for a pane, capture it with `editor.capture_viewport`, then
clear it and capture again for a before/after comparison.

Captured failing call (lavapipe; no image source and no existing reference):

```console
$ regia agent call editor.set_reference '{}' --project samples/arena
error: method call failed: `png_base64` or `path` is required (the pane has no reference yet) (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```
