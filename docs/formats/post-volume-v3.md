# `PostVolume` component, format version 3

A post volume stored inside `.level.ron` / `.prefab.ron` files like any reflected component
(`regia_render::post_volumes::PostVolume`, a pinned type path). Design and blending rules:
[`docs/specs/30-post-volumes-and-physical-camera.md`](../specs/30-post-volumes-and-physical-camera.md)
(ADR-1464; lens overrides ADR-1863; this version ADR-2323). Version 2 is described in
[`post-volume-v2.md`](post-volume-v2.md); version 3 adds two things and changes nothing else.

```ron
"regia_render::post_volumes::PostVolume": (
    format_version: 3,
    enabled: true,
    shape: ConvexHull(planes: [
        (1.0, 0.0, 0.0, 4.0), (-1.0, 0.0, 0.0, 4.0),
        (0.0, 1.0, 0.0, 3.0), (0.0, -1.0, 0.0, 0.0),
        (0.0, 0.0, 1.0, 12.0), (0.0, 0.0, -1.0, 12.0),
        (0.7071, 0.7071, 0.0, 4.5),
    ]),
    priority: 2,
    blend_distance: 1.5,
    weight: 1.0,
    overrides: (
        exposure_compensation: Some(0.75),
        saturation: Some(0.9),
    ),
    lut: Some("looks/hall.cube"),
),
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `format_version` | `u32` | `3` | See Versioning. |
| `shape` | `Box(half_extents: (x, y, z))`, `Sphere(radius: r)`, `ConvexHull(planes: [(nx, ny, nz, d), ...])` or `Unbound` | `Box(half_extents: (1.0, 1.0, 1.0))` | `ConvexHull` is new in v3: the intersection of the half-spaces `n · x ≤ d` in local space, 1 to 16 planes, outward normals (any non-zero length; `d` is in units of the normal's length). More than 16 planes, none, a zero normal or a non-finite value makes the volume contribute nothing. Planes that do not meet (an empty hull) contribute nothing either. A hull may be unbounded (a half-space, a slab, a prism). |
| `lut` | `Option<String>` | `None` | New in v3: a `.cube` or PNG-strip LUT, relative to the project root, that the volume blends into the view's LUT by its weight and priority. Ignored unless the host turns volume LUTs on (`PostVolumeLuts::enabled`, `render.set_post_volume_luts`). Resolution (`regia_render::lut::load_named`, ADR-4830): `\` is accepted as a separator (a Windows-authored path); a `.cube` the asset manifest holds loads from the mounted paks or the cooked cache first (the only route a packaged Android client has), then a loose file under the project root (`<root>/<path>`, then `<root>/assets/<path>`). A `.png` strip only loads loose: a cooked `.png` is a texture, not PNG bytes. |

Every other field (`enabled`, `priority`, `blend_distance`, `weight`, `overrides`) is unchanged from
version 2.

**Distance and band of a hull.** The planes are moved to world space exactly (an affine map takes a
plane to a plane), so the signed distance and the blend band are world meters under any rotation, scale
or shear of the entity: inside, the distance to the nearest face plane; outside, the exact distance to
the nearest point of the hull (on a face, an edge or a vertex). The weight across the band is the same
Hermite smoothstep as the other shapes.

**Versioning** (`docs/specs/12-assets.md` § 4a): versions 0, 1 and 2 are read as 3 by
`PostVolume::migrate` (`post_volume_migration_sources()` is `[0, 1, 2]`). Version 3 only added an enum
variant and an optional field, so an older component (whose shape is a box, a sphere or unbound and
which has no `lut` field) reads with no value changed. A version above the engine's
`POST_VOLUME_FORMAT_VERSION` is skipped by the renderer with one warning, and `render.set_post_volume`
refuses to edit it (`newer_format`). The reflected field list is hashed by `cargo xtask check-formats`
(entry `post-volume`). `regia upgrade-project` does not rewrite components inside level files; the
load-time migration covers them, as for the primitive component. An engine older than this one reads a
version-3 component as newer and skips it (it does not misread a hull as a box).
