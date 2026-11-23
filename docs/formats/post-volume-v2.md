# `PostVolume` component, format version 2

A post volume stored inside `.level.ron` / `.prefab.ron` files like any reflected component
(`regia_render::post_volumes::PostVolume`, a pinned type path). The renderer blends it into the post
settings of every camera whose eye it reaches; design and blending rules are in
[`docs/specs/30-post-volumes-and-physical-camera.md`](../specs/30-post-volumes-and-physical-camera.md)
(ADR-1464). The editor inspector edits it through reflection; agents use `scene.add_component` or
`render.set_post_volume` (`book/agents/post-volumes.md`).

```ron
"regia_render::post_volumes::PostVolume": (
    format_version: 2,
    enabled: true,
    shape: Box(half_extents: (6.0, 3.0, 10.0)),
    priority: 1,
    blend_distance: 2.0,
    weight: 1.0,
    overrides: (
        exposure_compensation: Some(0.5),
        white_balance_kelvin: Some(3600.0),
        saturation: Some(1.1),
        tonemap: Some(Neutral),
        fog_tint: Some((1.0, 0.85, 0.7)),
        eye: Some((ev_min: 6.0, ev_max: 8.0, adapt_up_speed: 2.0, adapt_down_speed: 1.5)),
        lens_dirt_intensity: Some(0.6),
        chromatic_aberration: Some(0.003),
    ),
),
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `format_version` | `u32` | `2` | See Versioning. |
| `enabled` | `bool` | `true` | |
| `shape` | `Box(half_extents: (x, y, z))`, `Sphere(radius: r)` or `Unbound` | `Box(half_extents: (1.0, 1.0, 1.0))` | Local space, metres; the entity transform places and scales it. |
| `priority` | `i32` | `0` | Higher tiers apply after lower ones. |
| `blend_distance` | `f32` | `0.0` | Metres outside the shape over which the volume fades in. |
| `weight` | `f32` | `1.0` | 0–1. |
| `overrides` | struct of optional fields | all `None` | Fields left out of the RON are `None`. |

Override fields (each `Some(value)` or `None`): `exposure_compensation`, `exposure_min_ev100`,
`exposure_max_ev100`, `meter_center_weight`, `tonemap` (`AgX`, `AcesFitted`, `Neutral`), `contrast`,
`saturation`, `bloom_intensity`, `white_balance_kelvin`, `tint`, `lift` / `gamma` / `gain` (`(r, g, b)`),
`vignette`, `film_grain`, `local_shadow_contrast`, `local_highlight_contrast`, `local_detail_strength`,
`ao_intensity`, `fog_tint` (`(r, g, b)`), `eye` (`(ev_min, ev_max, adapt_up_speed, adapt_down_speed)`),
and since version 2 (ADR-1863) the lens-effect strengths `lens_flare_intensity` (0–1000),
`lens_flare_threshold` (0–65 504), `lens_dirt_intensity` (0–10) and `chromatic_aberration` (0–0.05), blended
into `PostSettings::lens_effects` (spec 30 § 9).

**Versioning** (`docs/specs/12-assets.md` § 4a): `format_version` 0 (a hand-written component that left
the field out) and 1 (before the lens overrides; [`post-volume-v1.md`](post-volume-v1.md)) are read as 2
by `PostVolume::migrate`: version 2 only added optional fields, which a version-1 component leaves unset, so
no value changes; a version above the engine's
`POST_VOLUME_FORMAT_VERSION` is skipped by the renderer with one warning instead of being misread, and
`render.set_post_volume` refuses to edit it (`newer_format`). The reflected field list is hashed by
`cargo xtask check-formats` (entry `post-volume`), so a field change without a version bump and a
step in `PostVolume::migrate` fails CI. `regia upgrade-project` works per file type and does not
rewrite components inside level files; the load-time migration covers them, as for the primitive
component.
