# Quick draw: what the immediate layer drew

Games, prototypes and examples built on `regia_quick` draw every frame through two resources:
`Draw` (2D shapes, paths, meshes and text over the frame) and `Draw3d` (lit cubes, spheres,
cylinders, cones, planes, grids and lines in the world). Nothing they draw is an entity, so the
scene methods do not see it. Use `quick.draw_list` and the `QuickDraw` predicate resource
instead, and `seeing.screenshot` to look at it.

## What was drawn last frame

`quick.draw_list`:

```json
{}
→ { "frame": 240, "primitives": 14,
    "by_kind": { "ellipse": 6, "rect": 5, "text": 3 },
    "texts": ["SCORE 12", "LIVES 3", "PAUSED"],
    "background": [0.01, 0.01, 0.02, 1.0],
    "bounds": [-640.0, -360.0, 640.0, 360.0],
    "vertices": 412, "triangles": 380, "draw_calls": 3,
    "primitives_3d": 0, "lines_3d": 0, "frozen": false, "samples": 4 }
```

- `by_kind` counts 2D drawings: `rect`, `ellipse`, `line`, `arrow`, `tri`, `quad`, `polygon`,
  `path`, `mesh`, `text`.
- `texts` lists the strings of the text drawings in drawing order (at most 64). This is the
  cheapest way to read a HUD: the score, a prompt, a menu label.
- `bounds` is the box around every 2D vertex in logical units. The origin is the window center
  and +Y is up, so `[-w/2, -h/2, w/2, h/2]` is the whole window.
- `primitives_3d` / `lines_3d` count `Draw3d` drawings.
- `frozen` is `true` while the app holds its last frame (`DrawFrozen`). The numbers then describe
  that held frame.

An app without `QuickPlugin` answers `NOT_FOUND` (`F0003`).

## Assert on it

The same object is the `QuickDraw` resource of `play.wait_until` and `play.expect`
([testing](testing.md)):

```json
{ "assertions": [
  { "name": "HUD shows the score",
    "condition": { "resource": "QuickDraw", "path": "by_kind.text", "op": "gte", "value": 1 } },
  { "name": "ten bricks left",
    "condition": { "resource": "QuickDraw", "path": "by_kind.rect", "op": "eq", "value": 10 } }
] }
```

Text equality uses `path: "texts.0"` (the first string).

## See it

The 2D layer draws after tonemapping, over the 3D frame and under the runtime UI, with 4×
anti-aliasing. `seeing.screenshot` includes it, from a visible or a hidden window; a headless
`target: "offscreen"` capture includes it with `window_layers: true`. `Draw3d`
primitives go through the normal forward path with a default lit material, so they take the
level's sun, shadows and exposure. Their lines are gizmo group `quick`, which `debug.set_gizmos`
can restyle or hide.

## Recipes

- **Did the game-over text appear?** `play.wait_until` on
  `{ "resource": "QuickDraw", "path": "texts.0", "op": "eq", "value": "GAME OVER" }`.
- **Is something drawn off-screen?** Compare `bounds` with the window's half size.
- **Why does the frame look cleared?** A `background` color in the list means the game fills
  the window every frame, which covers the 3D scene.
