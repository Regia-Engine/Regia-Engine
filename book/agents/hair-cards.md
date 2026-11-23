# Hair cards

Hair, eyebrows, lashes, fur and foliage drawn as alpha-tested cards get a strand shading model
from the standard material: two Kajiya-Kay highlight lobes, light through the strands, a
root-to-tip colour and alpha ramp, and an alpha strategy. The feature is `LabMaterial::hair`
(`HairFactors`). It defaults off, and a material without it shades exactly as before. The
decision is ADR-245; the papers are in `docs/provenance.md`.

## What is shaded

| Factor | Effect |
|---|---|
| `primary_shift`, `primary_exponent`, `primary_strength` | The first lobe: a tight line, takes the light's colour |
| `secondary_shift`, `secondary_exponent`, `secondary_strength` | The shifted, broader lobe, takes the hair colour |
| `shift_noise`, `streak_density` | Per-strand jitter of both shifts across the card's `u`, so the highlight breaks into streaks |
| `transmission` | Wraps the diffuse cosine, so a card lit from behind glows instead of going black |
| `root_color`, `tip_color`, `root_alpha`, `tip_alpha`, `ramp_start`, `ramp_end` | Multipliers on base colour and alpha, blended along the card's `v` (`ramp_start` above `ramp_end` flips it) |
| `alpha` | `Hashed`, `Sorted` or `Coverage` (below) |

The strand direction is the mesh **tangent** (glTF `TANGENT`), root to tip, and `v` runs root
(0) to tip (1) by default. Cards need tangents; a card with none shades its diffuse and ramp
but no highlight.

## Alpha strategies

- **`Hashed`** (default): the material is alpha tested. On a TSR frame the threshold is a
  per-pixel, per-frame hash, the same in the depth prepass and the colour pass, so TSR's
  accumulation turns it into fractional coverage. Without TSR it is the hard cutoff
  (`LabMaterial::alpha_cutoff`). Cards write depth, so they need no sorting.
- **`Sorted`**: the material draws in the sorted alpha-blend pass, back to front, with no depth
  writes. Soft tips, no TSR needed; overlapping cards need the draw order to be right. The
  order-independent pass excludes hair cards, so turning that pass on does not change them.
- **`Coverage`**: alpha-to-coverage on multisampled frames. This build has no coverage
  pipeline, so it draws as `Hashed` (ADR-245).

`HairFactors::apply_alpha_mode` writes the matching `alpha_mode` and cutoff on the material;
the renderer also derives the mode itself (`hair::effective_alpha_mode`).

## Shadows

A hair card casts its shadow through its alpha test, using the translucency maps (see
[Translucent shadows](translucent-shadows.md)): the ramp alpha is tested against the cutoff, so
the shadow has the card's strands and not its square outline. Every preset but Mobile Low has
the maps (Mobile High one cascade, Desktop Low and up two or more plus local lights); Mobile Low
casts no hair shadow.

## Agent methods

`render.hair_get {entity}` reads the factors. `render.hair_set {entity, ...}` patches them; any
patch turns hair on, and `enabled: false` turns it off and keeps the tuning. Turning it off
also puts back the alpha mode and cutoff the material had before `render.hair_set` turned hair
on (an opaque mesh is opaque again). The entity gets its own copy of the material, so other
instances of the shared material keep their look.

```json
{"method": "render.hair_set", "params": {"entity": 4294967297, "tip_color": [0.3, 0.2, 0.1],
  "tip_alpha": 0.3, "ramp": [0.2, 0.9], "alpha": "hashed"}}
```

Then `seeing.screenshot` to look at it. Hair on a material graph surface is not supported: the
graph path skips the ramp and the alpha factor.

## Authoring in Rust

```rust,ignore
let mut material = LabMaterial::default();
material.hair = HairFactors {
    tip_color: [0.35, 0.28, 0.22],
    tip_alpha: 0.35,
    ..HairFactors::strands()
};
material
    .hair
    .apply_alpha_mode(&mut material.alpha_mode, &mut material.alpha_cutoff);
```

## Tests

`cargo test -p regia_render --test hair_cards` renders a card mesh: pixels change only for the
material that enables hair, the lobes add a highlight, the ramp darkens the tip, the strategies
draw, and the shadow follows the alpha test. `REGIA_PREVIEW=/tmp/hair` also writes the
look-dev fan (hair off and on). `--test hair_cards_warm` checks that no pipeline is created
after warm-up.
