# `LensEffects` component, format version 1

A camera's lens effects (ghost flare, lens dirt, chromatic aberration, diffraction starburst) stored
inside `.level.ron` / `.prefab.ron` files like any reflected component
(`regia_render::lens_effects::LensEffects`, a pinned type path; registry name `lens-effects`). Design:
[`docs/specs/30-post-volumes-and-physical-camera.md`](../specs/30-post-volumes-and-physical-camera.md)
and ADR-1863 (the pass), ADR-3196 (this format).

```ron
"regia_render::lens_effects::LensEffects": (
    format_version: 1,
    flare_intensity: 1.0,
    flare_threshold: 4.0,
    coating: 1.0,
    dirt_intensity: 0.3,
    chromatic_aberration: 0.002,
    starburst: 1.0,
    after_dof: false,
),
```

Every field of `LensEffects` is stored; a field left out takes the off value (`LensEffects::OFF`:
every strength 0, threshold 4, coating 1, `after_dof` false), so a component written by hand with only
the fields it needs reads unchanged. `starburst` is the diffraction starburst strength (0 off, up to 10);
`after_dof` runs the pass after the camera's depth of field and motion blur instead of before them.
The component only takes effect where the quality preset allows the pass (`RenderProfile::lens_effects`).

**Versioning** (`docs/specs/12-assets.md` § 4a): `format_version` 0 (left out) reads as 1; a version above
`LENS_EFFECTS_FORMAT_VERSION` is ignored by the renderer with one warning (the camera renders without lens
effects). The reflected field list is hashed by `cargo xtask check-formats` (entry `lens-effects`);
`lens_effects_migration_sources()` is `[0]`. `regia upgrade-project` lists the components inside each scene
file and `--write` rewrites them at version 1. Fuzz target: `lens-effects`.
