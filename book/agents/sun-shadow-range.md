# Sun shadow range

The sun's shadows come from a set of cascades: shadow maps that each cover a slice of the view, near slices
finely and far slices coarsely. Mobile Low draws **one** 1024² cascade. On the lite forward path (tile GPUs, so
every phone) that cascade reaches 40 m, so it stays fine enough for small casters. On any other adapter, such
as a desktop GPU, an integrated GPU or the software rasterizer, the one cascade covers the whole shadow range,
up to 4 km. Its texels are then several metres wide, and a tree or a crate casts no visible shadow at all. The
showcase forest at Mobile Low drew no tree shadows for this reason.

With a **sun shadow range** set, the cascades stop at that distance from the camera, so a one-cascade tier
spends its map near the camera. On the lite forward path, shadows fade out over the last 15 % of the range.
On the desktop full-forward shader the fade to unshadowed light is the last 15 % of the last cascade's own
interval (from the previous split to this range). That interval is the whole range only when the tier has
one cascade, because the shader then starts the interval at 0. ADR-2486.

| Method | Permission | What it does |
|---|---|---|
| `render.get_sun_shadow_range` | read | `{ range_m, min_m, max_m }`; `range_m` is `null` when the tier decides. |
| `render.set_sun_shadow_range` | edit_scene | `{ range_m }` in metres, or `null` to clear; answers `{ before, after }`. |

```sh
regia agent call render.get_sun_shadow_range '{}' --project <project>
regia agent call render.set_sun_shadow_range '{"range_m": 150}' --project <project>
regia agent call render.set_sun_shadow_range '{"range_m": null}' --project <project>
```

- It is **off by default** and not saved, so default frames are unchanged. It applies from the next frame.
- The renderer clamps the range to 8-4000 m. A non-finite value clears it. The agent refuses a value of 0 or
  below with `invalid_params`.
- It caps the reach and never extends it: on the lite Low path the smaller of 40 m and this range applies.
- It works on every tier. On a tier with several cascades, it pulls all of them closer, so near shadows get
  sharper and shadows beyond the range disappear.
- In Rust: `FrameRenderer::set_sun_shadow_range(Some(150.0))` (through `RuntimeRenderer`).

What the showcase forest shows at Mobile Low on lavapipe (1280x720, camera 10 m over the meadow; images in the
lane report):

| Range | Tree shadows |
|---|---|
| Not set (default) | None |
| 40 m | None: the nearest meadow in this view is farther than 40 m |
| 80 m | The near trees cast shadows across the meadow |
| 150 m | As 80 m, plus the trees at the far right of the frame |
| 300 m | As 150 m, with softer, fainter shadows (coarser texels) |

## Errors

- `F0003` (not found): the app has no runtime renderer.
- `invalid_params` (`F0002`): `range_m` is missing, not a number or `null`, or not above 0.

## See also

- [Mobile forward](mobile-forward.md): the lite forward shader and its 40 m sun range.
