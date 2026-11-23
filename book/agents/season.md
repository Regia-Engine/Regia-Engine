# Seasons (agents)

`regia_framework::season` is one driver for foliage colour, leaf density and leaf fall, with a bare
winter tree (ADR-2449). It is opt-in: add `SeasonPlugin`, mark meshes with `SeasonalFoliage { kind }`.
A scene that never sets a season stays at summer and nothing changes.

The time of year is a fraction of the year: spring 0.0, summer 0.25, autumn 0.5, winter 0.75, wrapping.
For each foliage kind the look is interpolated between the four anchors (eased inside each quarter):

| Kind | Autumn | Winter |
|---|---|---|
| `broadleaf` | tint toward orange, density 0.6 | bare (density 0, leaf fall 1) |
| `evergreen` | nearly unchanged | slightly cool, density 0.95 |
| `grass` | straw tint, density 0.8 | pale and thin, density 0.55 |

A look has a `tint` (multiplies the base colour), a `density` (1 full crown, 0 bare) and `leaf_fall`
(1 minus density). The system swaps each `SeasonalFoliage` mesh's material for a tinted copy, shared by
all meshes with the same original (one allocation per material per season change), and restores the
original in summer. An alpha-masked material's alpha is scaled so the leaf cards shrink with the density
and vanish at zero, in the main pass and in the alpha-tested shadows; a blended one fades; an opaque one
(bark) only takes the tint.

Hosts that scatter instances themselves call `leaf_present(instance_hash, density)`: the instances that
drop out are a fixed subset in every run, and the set thins monotonically as the density falls.

| Method | Permission | What it does |
|---|---|---|
| `scene.season` | edit_scene | Read the season (no params), or set it. |

Params: `season` (`spring`, `summer`, `autumn`, `winter`) or `year_fraction` (0..1), not both;
`rate_per_s` (year fractions per second of game time, 0 holds still; the season then turns by itself, one
step of the game clock per frame); `hold_summer` (kinds to keep at their summer look).

```json
{ "season": "winter", "hold_summer": ["evergreen"] }
→ { "before": { "season": "summer", ... },
    "after": { "year_fraction": 0.75, "season": "winter", "rate_per_s": 0.0,
               "looks": { "broadleaf": { "tint": [1.5, 0.9, 0.5], "density": 0.0, "leaf_fall": 1.0,
                                         "held_at_summer": false }, ... },
               "seasonal_meshes": 1200 } }
```

`UNSUPPORTED` (`F0016`) without `SeasonPlugin`. `F0002` with `data.reason`: `season_both_time_and_name`,
`season_unknown_name`, `season_unknown_kind`.
