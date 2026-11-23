# Quick draw 2D list: the instanced HUD fast path

`regia_quick::draw2d` is an instanced 2D draw list for HUDs and menus that redraw every frame. A
frame is a flat list of instances: rounded boxes, box outlines (rings), anti-aliased segments,
glyphs of shaped text and images. The layer draws the whole list with one pipeline, one shader
(`draw2d.wgsl`) and one draw call. Glyphs live in a paged atlas: a glyph is rasterized and its
page uploaded once, then only looked up. Once its buffers have grown, a steady frame allocates
nothing and uploads only its instances (80 bytes each).

It does not replace `Draw` ([quick draw](quick-draw.md)), which tessellates paths, meshes and
transformed shapes. Nothing in the list is an entity, so read it with `quick.draw2d_stats` and
look at it with `seeing.screenshot`.

## What the list held last frame

`quick.draw2d_stats`:

```json
{}
→ { "frame": 240, "instances": 57,
    "by_kind": { "box": 12, "outline": 3, "segment": 8, "glyph": 33, "image": 1 },
    "atlas_pages": 1, "glyphs_cached": 21, "glyphs_rasterized": 22, "glyphs_refused": 0,
    "pages_staged": 1, "bytes_uploaded": 4560, "instance_capacity": 262144,
    "layer_frames": 239, "pipelines": 1, "buffers": 2, "textures": 4,
    "tiles": { "dirty_regions": false, "tiles": 0, "painted": 0, "dirty": 0, "runs": 0,
               "painted_runs": 0, "redrawn_instances": 0, "redrawn_total": 0, "clean_frames": 0 } }
```

- `by_kind` counts the instances of the last extracted list.
- `glyphs_cached` is the number of glyphs in the atlas. `glyphs_rasterized` counts cache misses
  since start, blank glyphs (spaces) included once. On a steady HUD it stops growing after the
  first frame; a value that keeps climbing means the text changes size or the atlas recycles
  pages.
- `glyphs_refused` counts glyphs not drawn because every atlas page held glyphs the same frame
  had already drawn (too many sizes in one frame).
- `pages_staged` counts atlas pages handed to the layer for upload.
- `bytes_uploaded` is what the layer wrote to the GPU in its last draw (instances, new atlas
  pages, new images, the uniform when the target size changed). A steady frame uploads only its
  instances. It is one frame behind the list, because the layer draws after extraction.
- `layer_frames`, `pipelines`, `buffers` and `textures` count what the layer did and created.
  `pipelines` is one per target format. After `warm` these stay flat. A growing `buffers` means
  the instance buffer grew (it grows by powers of two and never shrinks).

An app without `Draw2dPlugin` answers `NOT_FOUND` (`F0003`).

## Dirty regions

With dirty regions on (the default under `Draw2dPlugin`; `Draw2dLayer::set_dirty_regions` for a
shell's own layer), the layer keeps a retained UI target and hashes the list per screen tile, 256 × 64
pixels by default. A tile whose instances did not change keeps last frame's pixels: it is not redrawn
and nothing is uploaded for it. Changed tiles merge into runs per tile row and redraw scissored, and a
composite blends the painted tiles of the UI target over the frame. The result carries a `tiles` object:

```json
{ "tiles": { "dirty_regions": true, "tiles": 136, "painted": 21, "dirty": 1, "runs": 1,
             "painted_runs": 14, "redrawn_instances": 3, "redrawn_total": 412,
             "clean_frames": 230 } }
```

- `tiles` is the grid size (columns × rows) and `painted` the tiles any instance touches.
- `dirty` and `runs` are what the last draw redrew: on a steady HUD both are 0 and `bytes_uploaded` is
  0. A changed value redraws its own tiles, usually one run in one tile row.
- `painted_runs` is the runs of painted tiles per tile row: the composite blends only those.
- `redrawn_instances` is the instances that touch a dirty tile (the ones uploaded and drawn).
- `redrawn_total` counts tiles redrawn since start and `clean_frames` the draws that redrew nothing.
  A `clean_frames` that never grows on a HUD that should be still means something changes every frame
  (an animated value, a text reshaped at another position).
- A resize, a recycled atlas page or a refilled image slot redraws every tile once.

All zero with dirty regions off (`Draw2dSettings { dirty_regions: false, .. }`).

## Assert on it

The same object is the `QuickDraw2d` resource of `play.wait_until` and `play.expect`
([testing](testing.md)):

```json
{ "assertions": [
  { "name": "HUD text drawn",
    "condition": { "resource": "QuickDraw2d", "path": "by_kind.glyph", "op": "gte", "value": 1 } },
  { "name": "no glyph lost",
    "condition": { "resource": "QuickDraw2d", "path": "glyphs_refused", "op": "eq", "value": 0 } },
  { "name": "a still HUD redraws nothing",
    "condition": { "resource": "QuickDraw2d", "path": "tiles.dirty", "op": "eq", "value": 0 } }
] }
```

## How a game draws with it

ECS apps add `Draw2dPlugin` after `RenderPlugin` and push through `ResMut<Draw2d>` in `Update`.
The list is cleared in `PreUpdate` and drawn as a window overlay layer above the quick layer and
under the runtime UI. Coordinates are physical pixels of the window, origin top-left, +Y down.
Shells that own their loop keep a `Draw2dList` and a `Draw2dLayer`: `warm` at load, then
`begin_frame`, push, `end_frame` and `draw_list` each frame.

- `push_box(rect, color, radius)`: a filled box. A radius of half a square's side draws a disc.
- `push_soft_box(rect, color, radius, feather)`: a box whose edge fades over `feather` pixels
  (a scrim or veil; a halo on a disc). It counts as a `box`.
- `push_gradient_box(rect, from, to, radius, horizontal)`: a linear gradient fill (a strip or a
  bar). It counts as a `box`.
- `push_outline(rect, color, radius, thickness)`: a band inside the box edge (a ring on a disc).
- `push_segment(a, b, width, color)` and `push_round_segment`: anti-aliased lines with butt or
  round caps (crosshairs, brackets, pointers, markers).
- `push_text(fonts, shaped, origin, color)`: a run shaped once with `TextSystem::shape` and kept.
  Shaping allocates, so a HUD shapes when the string changes, not every frame.
- `push_image(slot, rect, uv, tint)`: part of one of four image slots, filled with `set_image`.
- `set_clip(Some(rect))`: clips the instances pushed after it.

`HudLayer` (`regia_quick::draw2d::hud`) wraps a list for one frame with screen anchors and HUD
elements: `place(anchor, margin, size)` (nine `HudAnchor`s, margins measured inward, times the UI
scale), `panel`, `scrim`, `veil`, `halo`, `strip`, `bar`, `line`, `bracket`, `ring`, `disc`, `outline`,
`crosshair`, `plus`, `pointer`, `text` (with `TextAlign::Left`, `Center` or `Right` of its anchor),
`block` and `image`. The `hud_draw2d` sample is a complete HUD built with it.

## See it

`seeing.screenshot` includes the list from a visible or hidden window; a headless
`target: "offscreen"` capture includes it with `window_layers: true`, like the quick layer.
