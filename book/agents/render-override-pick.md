# Render overrides and surface picks

## `editor.set_render_override`

Tints, ghosts, x-rays or unlights one entity without touching its material. Params: `entity`
(bits, required), `tint` `[r, g, b, a]` (linear 0-1, `a` multiplies the coverage), `emissive`
`[r, g, b]` (display-referred 0-1, added on top), `alpha` (coverage 0-1; below 1 draws blended),
`xray` (drawn through whatever is in front), `unlit`, `clear` (remove it). A non-finite number (or
one outside the `f32` range, `1e39`) is refused with `F0002` and stores nothing.

```json
{ "method": "editor.set_render_override", "params": { "entity": 4294967301, "tint": [1, 0.2, 0.2, 1] } }
{ "method": "editor.set_render_override", "params": { "entity": 4294967301, "alpha": 0.3 } }
{ "method": "editor.set_render_override", "params": { "entity": 4294967301, "xray": true, "alpha": 0.6 } }
{ "method": "editor.set_render_override", "params": { "entity": 4294967301, "clear": true } }
```

Returns `{ entity, name, override: { tint, emissive, alpha, xray, unlit, blends } | null,
undoable: false }`. An override on a node also covers its children and a model's parts that have
none of their own; an override with no fields set on a child stops that. Ghosts and x-ray draw in
the blend pass: they cast no shadow. Material-graph surfaces take tint, alpha, unlit and emissive,
but not x-ray. Check it worked: `editor.capture_viewport` before and after, and compare.

## `editor.pick_hit`

The surface under a capture pixel: which mesh, which triangle, where and which way it faces.
Params: `x`, `y` (required), `width`, `height`, `view` (as `editor.capture_viewport`), `cull`
(`none` = both faces, the default; `back` = front faces only).

```json
{ "method": "editor.pick_hit", "params": { "x": 160, "y": 120, "width": 320, "height": 240 } }
```

Returns `entity` and `name` (the mesh entity hit; a model part, not its owner), `owner` and
`owner_name` (what a click selects), `position` (world), `normal` (world, unit, the triangle's
front side as drawn), `smooth_normal` (interpolated vertex normal), `triangle` (index into the
mesh's index list), `indices` (its three vertices), `barycentric` (their weights), `distance`
(metres from the near plane) and `front_face`; `{ entity: null }` on background. Skinned meshes
are hit where they are drawn (current palette). Terrain is not returned (use `editor.pick`), but it hides: where terrain is nearer than the mesh, or a proxy cube covers the pixel in front of it, the result is `null`, not the mesh behind.

To place something on a surface: `pick_hit`, then `editor.set_transform` at `position`, oriented
along `normal`.
