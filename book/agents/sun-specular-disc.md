# Sun specular disc

The forward pass lights the sun's specular as if the sun were a point. The sun is a disc 0.27°
in radius, and the reference path tracer (`regia_pt`) samples it as one. On rough surfaces the
difference is invisible: the GGX lobe is much wider than the disc. On a 0.1-rough surface
(GGX α = 0.01) the lobe is only about twice the disc's width. The point sun's highlight core
comes out too bright and its rim too dark. That was the whole `specs/27` § 2.2 direct-lighting
miss on smooth spheres (ADR-3190).

With the disc on, each isotropic GGX lobe of the sun takes its mean over the disc instead of its
value at the disc's centre. The mean has a closed form: GGX is a Cauchy distribution of
microfacet slopes, and the sun's cone maps to a small disc in slope space. The forward pass
scales the lobe it already has by mean ÷ point value (`pbr_lighting.wgsl` `disc_lobe_scale`).
The cost is one square root and a few multiplies per pixel, and only when the disc is on.

| Roughness | What changes |
|---|---|
| 0.1 | The highlight core drops and widens to match the traced disc |
| 0.3 | Slightly (under 1 % at the core) |
| 0.5 and up | Nothing visible |

The disc is **off by default**. Every preset, shipped frame and golden image keeps the point sun.

## Where it applies

- **Runtime (`RenderPlugin`).** The render profile's `sun_specular_disc` field. When it is on,
  the extracted sun carries the sun's angular radius (0.004675 rad, the radius the sun shadows'
  PCSS and the path tracer use). `RenderSettings::preset` ignores the field, so opting in keeps
  the preset other systems take their tier from.
- **Hosts that build `FrameScene` themselves.** Call
  `DirectionalLightUniform::with_specular_disc(radius)` on the sun. Pass the planet's
  `sun_angular_radius`. A radius of 0, a negative or non-finite radius gives back the exact point
  sun. A radius above `MAX_SPECULAR_DISC` (0.2 rad) is clamped.
- **Time of day.** The sun that `TimeOfDay` computes keeps the authored light's disc.
- **Shading.** The forward pass and the visibility-buffer resolve (built from the same shader).
  Each isotropic lobe uses its own α, including a material's second specular lobe. Anisotropic
  and hair lobes keep the point sun. So does the lite (Mobile) shader.
- **Not changed.** The diffuse term, the sun's shadows and every local light.

## Methods

- `render.sun_specular_disc` reads the state:
  `{ "enabled": false, "angular_radius": 0.0, "preset": "desktop_high", "applies_to": "..." }`.
  The `angular_radius` is the radius the extracted sun carries (0 for the point sun). It fails
  with a not-found error when the app has no `RenderSettings`.
- `render.set_sun_specular_disc { "enabled": true }` sets the field and answers the new state.
  The next extracted frame's sun takes it.

```json
{"id": 1, "method": "render.set_sun_specular_disc", "params": {"enabled": true}}
```

### Error codes

| Code | Meaning |
|---|---|
| `sun_specular_disc_bad_enabled` | `enabled` is missing or not a boolean. |

## Measuring it

`crates/regia_pt/tests/specular_peak.rs` renders the `specs/27` § 2.2 material grid (dielectric,
metal and clearcoat spheres at roughness 0.1 to 1.0, sun only) with and without the disc. It
compares both against a converged (1024 samples per pixel) trace of the disc sun. Hand-run it on
a device (lavapipe works):

```sh
cargo test -p regia_pt --test specular_peak -- --ignored --test-threads=1 --nocapture
```

The suite's own direct row (`spec27_calibration.rs`, `target_direct_rel_rmse`) still traces one
sample per pixel and renders the point sun. On the 0.1-rough cores that one sample is far from the
disc's mean: the one-sample trace alone is 10.9 % (rel. RMSE) from the converged one, nearly the
whole 11.3 % the row reported. Against the converged trace, the point sun scores 1.06 % and the disc
sun 0.45 % (target 1 %). ADR-3190 records the numbers and the proposed suite change.
