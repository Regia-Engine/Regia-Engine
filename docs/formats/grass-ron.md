# `.grass.ron` (grass field, format v1)

A map's grass classes (ADR-1400, `regia_terrain::grass`, agent page `book/agents/terrain-grass.md`). Frozen
(`docs/specs/12-assets.md` § 4a): a shape change needs a version bump, a migrator and `regia upgrade-project`
support; `cargo xtask check-formats` holds the field-list hash.

```ron
GrassSet(
    version: 1,
    seed: 7,
    classes: [
        GrassClass(
            name: "dead_grass",
            density: 10.0,               // clumps per m² where every rule passes fully (0..=64)
            density_map: None,           // Some("maps/valley.grass.png"): grayscale over the whole map
            layers: [                    // empty: every layer at full density
                GrassLayerRule(layer: 1, density: 1.0, green: 0.15, height: 1.0),
                GrassLayerRule(layer: 4, density: 0.6, green: 0.6, height: 0.6),
            ],
            noise_scale: 9.0, noise_amount: 0.45,
            slope_min: 0.0, slope_max: 0.75,           // radians
            height_min: -10000.0, height_max: 10000.0,  // metres
            avoid_holes: true,
            exclusions: [ScatterExclusion(points: [(-40.0, 0.0), (60.0, 12.0)], radius: 3.0)],
            blades: 12, blade_height: (0.25, 0.55), blade_width: 0.011, clump_radius: 0.17,
            lean: 0.45, bend: 0.6,
            base_color: (0.24, 0.18, 0.10), tip_color: (0.58, 0.47, 0.30),
            green_color: (0.05, 0.10, 0.025), green_tip_color: (0.15, 0.23, 0.06),
            green_fraction: 0.12, green_height: 0.55, color_jitter: 0.18,
            roughness: 0.62, translucency: 0.4, base_occlusion: 0.5,
            range: 300.0, cast_shadows: true,
            sway: 0.0, sway_speed: 0.4,
        ),
    ],
)
```

Every `GrassClass` field is optional and defaults to `GrassClass::winter_dead()`; `seed` defaults to 0. Colours are
linear albedo. Validation (`GrassSet::validate`): at most 8 classes, unique non-empty names, every number in its
documented range (`GrassClass` rustdoc), layer indices inside the terrain's layer stack when it is known.

| Field | Meaning |
|---|---|
| `density`, `density_map`, `layers`, `noise_*`, `slope_*`, `height_*`, `avoid_holes`, `exclusions` | Where the class grows: baked once into the class mask (`GrassMask::build`: density, green fraction, height, tint) |
| `blades`, `blade_height`, `blade_width`, `clump_radius`, `lean`, `bend` | The clump: blades per clump (1..=16), height range, root width, root spread, outward lean, tip droop |
| `*_color`, `green_fraction`, `green_height`, `color_jitter`, `roughness`, `translucency`, `base_occlusion` | The look: stems root → tip, green shoots (fraction and relative height), per-clump jitter, back-lit transmission, root darkening |
| `range`, `cast_shadows`, `sway`, `sway_speed` | Farthest draw distance (the quality tier caps it), sun shadows inside the tier's shadow distance, tip sway (m) and frequency (Hz; 0 m = still air) |

Determinism: the class's random stream is keyed by `seed` and the class name, so reordering classes moves nothing.
