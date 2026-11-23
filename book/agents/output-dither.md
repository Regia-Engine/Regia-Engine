# Output dither and star flux

An 8-bit display image has 256 code values per channel. A slow gradient, such as a clear sky, fog or a dusk
ramp, changes by less than one code value per pixel. Stored without dither, each code value becomes a flat
band with a visible edge. At 4K a band of the same sky is twice as wide as at 1080p, so the steps are easier
to see. With **output dither** on, the renderer adds triangular noise of plus or minus one code value before
the 8-bit store. The bands then turn into fine, even grain that the eye averages back into the gradient.
ADR-2486.

| Method | Permission | What it does |
|---|---|---|
| `render.get_output_dither` | read | Whether the dither is on: `{ enabled, code_values }`. |
| `render.set_output_dither` | edit_scene | Turn it on or off: `{ enabled }`, answers `{ before, after }`. |

```sh
regia agent call render.get_output_dither '{}' --project <project>
regia agent call render.set_output_dither '{"enabled": true}' --project <project>
```

- It is **off by default**, so every default frame and golden stays bit-identical. Turn it on for filming
  (`render.out_start` at 1440p or 4K) and for a game's own settings.
- It applies from the next frame and is not saved.
- A float display target is never dithered (a 16-bit render-out, `bit_depth: 16`, or HDR output), because
  nothing is quantized to 8 bits there.
- The noise is fixed in screen space: interleaved gradient noise at two offsets, summed to a triangular
  distribution. It does not change from frame to frame, so it adds no flicker and costs a video encoder
  little.
- In Rust: `FrameRenderer::set_output_dither(true)` (through `RuntimeRenderer`).

## Star flux

The physical sky draws each star as a Gaussian about one pixel wide, sized from the angle one pixel spans. At
3840x2160 two things go wrong, and most of the star field disappears:

- The renderer computes that angle with `acos` of the dot product of two neighbouring view directions, in `f32`.
  At 4K the dot product sits within a few `f32` steps of 1, so the angle comes out quantized or off by tens of
  percent, and the star Gaussians shrink below a pixel.
- With a fixed peak, a star a pixel wide gets a quarter of its 1080p light when the pixel angle halves.

With **star flux** on, the sky computes the pixel angle from the chord between the two directions, in `f64`. The
star brightness also scales by `(height / 1080)²`, so each star keeps its light. In the showcase's 4K night frame on
lavapipe, the stars found rise from 2169 (mean peak 64) to 3345 (mean peak 108), and the same constellations appear
as finer points; the 1080p frame finds 1468 (mean peak 147). 1080p brightness is unchanged, and a 720p frame gets
dimmer stars by the same rule. ADR-2486.

| Method | Permission | What it does |
|---|---|---|
| `render.get_star_flux` | read | `{ enabled, reference_height }` (the height the brightness is kept for, 1080). |
| `render.set_star_flux` | edit_scene | `{ enabled }`, answers `{ before, after }`. |

```sh
regia agent call render.set_star_flux '{"enabled": true}' --project <project>
```

- It is off by default and not saved. In Rust: `FrameRenderer::set_star_flux(true)`.
- It changes only the star field of the physical sky; moons, sky bodies and the sun disc keep their own angular
  size.
- Off keeps the old pixel angle, so default frames and goldens are unchanged. Making it the default is an owner
  decision, because it moves every night frame with stars.
- Known issue, on or off: at night, stars show through dense clouds. The cloud march stops at 1 % transmittance
  and lets that 1 % of the starry sky through, and night clouds get no moonlight, so the leak is most of their
  light. Treating the cutoff as opaque turns the clouds black, so the fix waits for moonlit cloud lighting.

## Errors

- `F0003` (not found): the app has no runtime renderer.
- `invalid_params` (`F0002`): `enabled` is missing or not a boolean.

## See also

- [Render-out](render-out.md): `bit_depth: 16` keeps the display image in 16 bits and needs no dither.
