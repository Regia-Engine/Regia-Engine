# Fibrous cirrus

The cirrus sheet (`CirrusSheet`, [Cloud types](cloud-types.md)) draws soft fallstreak trails by
default (ADR-1470). Six optional values turn the sheet fibrous (ADR-1509): combed fine strands
along each trail, hooked heads, tails that hang below the sheet, and broken patches instead of
continuous bands. **All six default to off**, so scenes that do not set them keep the trails
pixel for pixel. The shader takes the old path whenever fiber, shear, breakup and hook are all 0.

## Values

| Value (`CirrusSheet` / agent / `sky.ron`) | Range (default) | What it does |
|---|---|---|
| `fiber` / `cirrus_fiber` | 0..1 (0) | Strength of the combed strands. Strands follow each trail, spread as it widens and break along their length. Faint patches of fibers lie along the shear between the trails. |
| `fiber_scale_m` / `cirrus_fiber_scale_m` | ≥ 10 m (150) | Spacing of the strands across the shear at a trail's head (they spread toward the tail). |
| `shear_deg` / `cirrus_shear_deg` | -180..180° (0) | Direction of the wind shear, measured from the wind, counterclockwise seen from above. Trails and fibers lie along it. |
| `shear` / `cirrus_shear` | 0..1 (0) | Amount of wind shear. Trails grow longer downwind, and their tails fall up to 2.5 km below the sheet (`CIRRUS_FALL_DEPTH_M` × shear). Seen at a slant, a tail hangs toward the horizon. |
| `breakup` / `cirrus_breakup` | 0..1 (0) | Breaks the bands into separate patches with clear sky between them. At 1, about half of a full band is clear. |
| `hook` / `cirrus_hook` | 0..1 (0) | Uncinus hooks. Each head gets a dense tuft, the tail leaves the head along the view slant and turns downwind (a comma), and it bends sideways as the wind veers with depth. |

`CirrusSheet::FIBROUS` holds the values tuned by eye on the showcase shot s1v t11: fiber 1.0,
fiber scale 120 m, shear 0.6, breakup 0.6, hook 0.45. `CirrusSheet::with_fibrous_look()` sets
them over any sheet. It keeps coverage, height, optical depth, tile and stretch.

## How it is drawn

Fallstreaks form when ice crystals grow in generating cells and fall through air whose wind
differs from the cell's. In a wind that grows linearly with depth, a crystal's offset from its
head grows as the square of the depth. A wind that also veers with depth adds a sideways bend.
The tail lies below the sheet, so a slanted view sees it displaced on the sheet by the view's
horizontal direction over its vertical part, times the depth. Each trail is that curve
(`regia_render::cloud_types::cirrus_density_fibrous`, the CPU mirror of the shader):

- The search covers every generating cell whose trail can reach the pixel. The window grows
  with the slant, which is clamped to 3 tiles (`CIRRUS_SLANT_MAX`) near the horizon.
- The strand pattern is scaled to the trail's local width, so strands fan out toward the tail.
  A strand fades to its mean before a pixel footprint covers a sixth of its spacing. The sheet
  fibers have three octaves, and each one fades the same way. So the sheet does not alias into
  hair toward the horizon.
- The far field (footprint wider than a trail) is the area mean of the near field, within
  about 15 %.

A fibrous sheet keeps 40 % of the sheet's broad curl warp (`CIRRUS_FIBROUS_WARP`), so its
streaks run along the shear instead of curling into S-strokes.

Cost: a few tens of trail cells per march texel, evaluated once per texel (not per march). Only
fibrous sheets pay it.

## Agent

```json
{"method": "render.set_clouds", "params": {"cirrus_fiber": 1.0, "cirrus_fiber_scale_m": 120,
  "cirrus_shear": 0.6, "cirrus_breakup": 0.6, "cirrus_hook": 0.45}}
```

- `render.set_clouds` takes the six `cirrus_*` names above. Values are clamped. With a
  `Weather` driving the clouds, they are pinned over the genus look until
  `clear_look_override: true` (ADR-1492).
- `render.get_clouds` reports `look.cirrus { fiber, fiber_scale_m, shear_deg, shear, breakup,
  hook, fibrous }`. `fibrous` is true when the shader draws the fibrous field. Pinned values
  appear under `look_override.fields`.
- Errors: a value that is not a finite number answers `F0002`. No new error codes.

## Scene file

The showcase `sky.ron` `cloud_look` block takes the same six names as `Option<f32>` fields:

```ron
cloud_look: Some((cirrus_fiber: Some(1.0), cirrus_fiber_scale_m: Some(120.0), cirrus_shear: Some(0.6),
                  cirrus_breakup: Some(0.6), cirrus_hook: Some(0.45))),
```

Files without them parse and draw as before.

## Rust

```rust
use regia_render::{CirrusSheet, CloudGenus, CloudSettings};

fn fibrous_cirrus() -> CloudSettings {
    let mut s = CloudSettings::default().with_genus(CloudGenus::Cirrus);
    s.look.cirrus = s.look.cirrus.with_fibrous_look();
    s
}
```
