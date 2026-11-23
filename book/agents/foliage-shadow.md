# Soft leaf shadows (agents)

An alpha-tested leaf card casts its shadow with a hard cutoff, so leaf edges in the shadow maps are
razor sharp and shimmer. `FrameRenderer::set_foliage_shadow_softness` (ADR-2449) softens them. It is off
by default (0 keeps the hard cutoff and every existing image), and it only matters with alpha-tested
shadows on.

With softness `w`, the shadow coverage of a masked caster rises from 0 to 1 over the band
`cutoff - w .. cutoff + w` (a smoothstep of the texture alpha times the material and vertex alpha). The
coverage is compared with a hash of the shadow-map texel, so a leaf edge writes a fraction of its texels,
in a fixed pattern that is the same every frame and every run; the shadow filter then averages the
pattern into a penumbra. The covered area stays about the same (the band is symmetric).

Changing the softness rebuilds the alpha-tested shadow pipelines and the stored shadow maps.

| Method | Permission | What it does |
|---|---|---|
| `render.foliage_shadow` | edit_scene | Read the setting (no params), or set `softness` (0..0.5, hundredths) and `alpha_tested`. |

```json
{ "softness": 0.25, "alpha_tested": true }
→ { "before": { "softness": 0.0, "max_softness": 0.5, "alpha_tested": false },
    "after":  { "softness": 0.25, "max_softness": 0.5, "alpha_tested": true } }
```

`F0003` when the app has no `RuntimeRenderer`. A softness above 0.5 is clamped; negative is `F0002`.

Not built: lower-detail leaf shadow casters at distance (it needs a shadow mesh per LOD).
