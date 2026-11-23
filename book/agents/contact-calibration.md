# Contact calibration

Two opt-in renderer switches bring contact-scale shading in line with the reference path tracer
(ADR-3202, spec 27 § 2.2 rows `ao_contact` and `sun_penumbra_1m`):

- **`horizon_ao`**: the GTAO rows of the SSAO run a horizon-slice kernel instead of the point
  obscurance kernel. A box's foot darkens the way the tracer's does, even seen from straight above,
  where the face itself is hidden.
- **`angular_sun_penumbra`**: a near-contact sun shadow edge follows the sun's angular size where
  the PCSS filter's footprint used to be wider, down to a ramp of 0.4 shadow texels.

Both are off in every preset because they move frames. The owner decides the defaults. Turning
either on at runtime leaves the preset recognised: `RenderSettings::preset` still names it, so
other systems keep the tier they chose by it.

## Agent protocol

| Method | Params | Result |
|---|---|---|
| `render.get_contact_calibration` | none | `{ horizon_ao, horizon_ao_active, angular_sun_penumbra, angular_sun_penumbra_active }` |
| `render.set_contact_calibration` | `horizon_ao?` (bool), `angular_sun_penumbra?` (bool), at least one | `{ before, after }` (each the get result) |

`*_active` says whether this tier can use the switch:

- The horizon kernel needs a GTAO SSAO row (Desktop High and Ultra), a depth prepass and no MSAA.
- The angular penumbra needs the PCSS-lite sun filter (Desktop High and Ultra).

On other tiers the switch is stored but does nothing.

Errors:

- `F0003`: the app has no `RenderSettings` (add `RenderPlugin`).
- Invalid params: a field that is not a boolean, an unknown field, or neither field given. Nothing
  changes.

Turning both on and reading the state back:

```json
{"method": "render.set_contact_calibration", "params": {"horizon_ao": true, "angular_sun_penumbra": true}}
{"method": "render.get_contact_calibration", "params": {}}
```

To score a live frame against the tracer, use `seeing.reference_trace` and `seeing.compare` as the
[Spec 27 calibration suite](spec27-calibration.md) page describes.

## What each switch changes

### Horizon AO

The point kernel (Alchemy obscurance) counts each depth sample by its own direction, with a
falloff on its 3D distance. Seen from above, the only depth samples beside a 1 m box's foot are the
box's top, 1 m away. The falloff dropped most of them, so the foot darkened 9.5 % where the tracer
darkens it 29.9 %.

The horizon kernel (GTAO, Jimenez et al. 2016) works per slice: it finds the highest occluder on
each side and integrates the cosine-weighted visible arc in closed form. Each depth sample stands
for a column behind it, one radius deep. The sample counts with the highest point of that column
within the world radius (1.2 m). A wall beside the receiver therefore blocks the low sky however far
away its top edge is. A pole far in front of the receiver does not, because its column ends before
the radius.

The forward pass's multi-bounce fit then lightens the occlusion by the albedo, which accounts for
the tracer's bounce light off the box. The kernel costs 4 slices × 4 steps × 2 sides = 32 depth
taps per AO texel, against the point kernel's 12–16. Its search reaches a quarter of the AO
target's height on screen, against the point kernel's tenth: at a tenth, a 1 m box seen from 3.5 m
was searched only 29 cm up its face.

### Angular sun penumbra

With shadow filter mode 4, the PCSS edge is steepened about its midpoint wherever the sun's disc
is narrower than the filter's own footprint. The footprint is the one-texel radius floor plus each
tap's bilinear tent, about 1.7 texels. On the spec 27 wall at 1 m from contact that was 14.1 mm
against the tracer's 6.8 mm.

The gain is 0.9 of the ratio of the two 10–90 % widths. The 0.9 comes from the fit: a steepened
S-shaped ramp comes out narrower than the plain ratio says. The ramp is never narrower than 0.4
texels, and the 0.5 contour stays where the filter drew it.

The constants were fitted per pixel on a CPU mirror of the filter, at every sub-texel position of
the edge. The result is within 10 % of the disc's width for a disc radius from 0.6 texels up.
Where the filter is already narrower than the disc (5 m and 20 m on that wall), mode 4 is mode 3
exactly. The shadow map still places an edge only to within half a texel: an edge that crosses
the texel grid shows its texel steps, now with a narrower ramp.

## Lightmaps and baked light

Neither switch touches baked diffuse:

- A lightmap receiver's diffuse term keeps only the material occlusion (ADR-2635). The SSAO, from
  either kernel, darkens only its specular.
- Under a baked GI volume, the horizon kernel weights each side's blocked share by what the probes
  do not already hold, as the point kernel does (ADR-1104).

## Tests

`crates/regia_render/tests/ao_contact_band.rs` checks both switches:

- **Analytic AO band.** The box-on-plane depth is ray-cast in a fragment shader. Both kernels run
  on it, and the 10 cm band is compared with the analytic form factor of the face, seen from above
  and at an angle.
- **PCSS mirror.** A CPU mirror of the PCSS filter shows where the 1 m floor comes from, and that
  mode 4 follows the disc.
- **Penumbra rows.** The frame's 1, 5 and 20 m rows are measured against the disc's analytic width.
- **Flicker.** A slowly moving sun counts flicker events with mode 3 and with mode 4.

Run them on lavapipe:

```sh
cargo test -p regia_render --test ao_contact_band -- --nocapture --test-threads=1
```
