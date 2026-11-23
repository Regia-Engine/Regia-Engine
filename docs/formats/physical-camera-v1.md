# `PhysicalCamera` component, format version 1

A camera's lens stored inside `.level.ron` / `.prefab.ron` files like any reflected component
(`regia_render::lens::PhysicalCamera`, a pinned type path; registry name `physical-camera`). Design:
[`docs/specs/83-prefab-profiler-volumes.md`](../specs/83-prefab-profiler-volumes.md) § 3.1 and
[`docs/specs/30-post-volumes-and-physical-camera.md`](../specs/30-post-volumes-and-physical-camera.md)
(ADR-1464, ADR-2323).

```ron
"regia_render::lens::PhysicalCamera": (
    format_version: 1,
    shutter_angle: 0.25,
    aperture_f_stops: 2.8,
    exposure: Physical,
    iso: 100.0,
    focal_length: Some(0.035),
),
```

Every field of `PhysicalCamera` is stored (shutter, aperture, focus, sensor, circle-of-confusion limit,
exposure mode `Inherit` / `Manual(ev100: ..)` / `Physical`, ISO, shutter time, focal length); a field left
out takes the default lens's value.

**Versioning** (`docs/specs/12-assets.md` § 4a): `format_version` 0 (left out) reads as 1; a version above
`PHYSICAL_CAMERA_FORMAT_VERSION` is ignored by the renderer with one warning (the camera renders as if it
had no lens). The reflected field list is hashed by `cargo xtask check-formats` (entry
`physical-camera`); `physical_camera_migration_sources()` is `[0]`. `regia upgrade-project` lists the
lenses inside each scene file and `--write` rewrites them at version 1. Fuzz target: `physical-camera`.
