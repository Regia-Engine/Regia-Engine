# Baked GI debugging views

When a baked map looks wrong somewhere, three agent methods say why. They read the installed
`.fgiv` volume (see [Baked GI volumes](baked-gi.md)) on the CPU, with the same code as
`regia_render::sample_static_gi`, so what they show is what the lookup computes. They need a
runtime renderer with a volume installed; otherwise they fail with `F0003` (not found). Bad
parameters fail with `F0002`.

The light they report is irradiance divided by π, per channel, summed over the four components
(sun bounce, sky, fill, emissive) after the frame's component factors. `scales.source` says where the
factors came from: `"frame"` when a baked frame has lit (the last frame's factors), `"as_baked"`
before that (all 1, the level of the bake).

## `render.get_gi_probes`

Lists probes nearest first.

| Param | Meaning |
|---|---|
| `center` | `[x, y, z]`, default the middle of the volume. |
| `radius` | Metres > 0, default the whole volume. |
| `max` | Probes returned, 0 to 2000, default 100. `0` returns only the summary. |
| `filter` | `"all"` (default), `"valid"`, `"invalid"` or `"relocated"`. |
| `normal` | The normal the light is evaluated for, default up. |

The reply has `matched` (before `max`), `returned`, `bounds`, `scales`, and a `summary` of the whole
volume: `total`, `valid`, `invalid`, `relocated`, `under_terrain`, `inside_geometry`,
`terrain_probes`, `bricks`, `interior_bricks`. Each probe has `index`, `layer` (`"terrain"` or
`"brick"`), `brick`, `interior`, `position`, `valid`, `relocated`, `under_terrain`,
`inside_geometry`, `flags`, `irradiance`, `luminance` and `mean_distance`. `mean_distance` is the
mean of the probe's distance moments in metres: a small value means the probe sits close to walls.
An invalid probe reports zero light.

## `render.explain_gi`

`render.explain_gi {"p": [x, y, z], "n": [..], "v": [..]}` repeats one lookup and returns how it was
built. `n` defaults to up and `v` to `n`.
Supplied vector components and positive scalar parameters must remain finite in `f32`;
zero directions are rejected with `invalid_params`. Directions are normalized before lookup.

- `irradiance`, `components` (per component) and `coverage` are the result of the lookup. It is the
  lookup `render.sample_baked_gi` runs: the renderer's settings, the last frame's light factors and
  its sun relight (`sun_relight` is the factor on sun light bounced off the ground; 1 = as baked).
- `brick` is the detail brick that holds the point, or `null`. `brick_coverage` and `terrain_coverage`
  are the coverage each layer took. They add up to `coverage`. An interior brick supplies all of it.
- `brick_visibility` and `terrain_visibility` (0..1) say how much of each layer's cell sees the
  point. Each layer's light is weighted by its share times its visibility (plus a small floor), so a
  layer whose probes are hidden from the point gives way to the other.
- `brick_corners` and `terrain_corners` list the corner probes each layer visited. Each has `index`,
  `position`, `valid`, `walled`, `trilinear` (position weight), `visibility` (how well the probe
  sees the point, from its distance moments), `weight` (after the facing and visibility terms) and
  `share` (its part of the layer's light, summing to 1). A rejected corner has `valid: false` and no
  weight. A `walled` corner is valid, but a baked wall between probes (or the leak guard) puts it on
  the other side from the point, so it adds no weight either.
- `findings` holds short codes: `outside_volume` (no layer reaches the point), `all_corners_rejected`,
  `rejected_corners` (some corner was rejected, so the light leans on its neighbors),
  `walled_corners` (some corner sits behind a baked wall), `interior_brick`, `partial_coverage`
  (at the volume's edge).

## `render.set_gi_debug_overlay`

Draws a view as a gizmo in the world. A later call replaces the drawing and `"mode": "off"` removes
it. A bad call leaves the previous drawing alone.

| Mode | Draws |
|---|---|
| `validity` | A sphere per probe: green kept, yellow relocated by the bake, red inside geometry, magenta under the terrain, orange rejected for another reason. |
| `irradiance` | A sphere per probe in the light it holds, divided by the peak channel of the drawn set (`peak` in the reply). `exposure` scales the result. Rejected probes are dark grey. |
| `weights` | For the point `p` (with `n`, `v`): a white sphere at `p` and a line to each corner probe. Lines are blue to white by the corner's share; rejected corners are red, corners behind a baked wall orange. |

`validity` and `irradiance` take the same `center`, `radius`, `max` (default 400, at most 5000) and
`filter` as `render.get_gi_probes`. `size` sets the sphere radius (default 0.15 m). The reply has
`mode`, `drawn`, `matched`, `peak` and a `note`. Nothing is drawn when nothing matches.

## Finding out why a spot is dark

1. `render.get_gi_probes {"max": 0}`: are many probes `invalid`? `under_terrain` rejects are normal
   near steep ground; `inside_geometry` rejects mean the bake placed probes in meshes.
2. `render.set_gi_debug_overlay {"mode": "validity", "center": [..], "radius": 20}` and a
   `seeing.screenshot`: a ring of red or magenta spheres around the spot shows rejected probes.
3. `render.explain_gi {"p": [..], "n": [..]}` at the dark pixel's world point (`world.pick`): look
   for `rejected_corners`, a corner with most of the `share` and a low `luminance`, or `outside_volume`.
4. `render.set_gi_debug_overlay {"mode": "weights", "p": [..]}` draws the same corners.
5. `"mode": "irradiance"` shows whether the light varies smoothly. A probe far darker than its
   neighbors usually saw a wall up close (`mean_distance` small).

## Limits

- The views are the CPU mirror, not a capture of the GPU pass. The mirror is checked against the GPU
  lookup by `crates/regia_pt/tests/baked_gi_runtime.rs`.
- Capsule occluders (`render.set_gi_occluders`) are applied in the screen pass and are not part of
  `render.explain_gi`.
- The editor viewport installs no volume, so the methods apply to runtime apps and play-in-editor
  hosts that install one.
- `render.get_gi_probes` walks every probe of the volume per call, which costs a few milliseconds on
  a large bake. It is not a per-frame call.
