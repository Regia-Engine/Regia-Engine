# `.ecology.ron` (biome ecology profile, format v1)

The site a biome stands for and which of its entries is which species (ADR-2456, `regia_procgen::biome::ecology::EcologyProfile`,
agent page `book/agents/biomes.md` "Ecology"). It sits beside its biome: `biomes/birch.biome.ron` → `biomes/birch.ecology.ron`
(`ecology_path_for`). The `.biome.ron` format is not changed by it; a biome without a profile is validated by species names.
Frozen (`docs/specs/12-assets.md` § 4a): a shape change bumps `format_version` with a `VersionStep` migrator
(`ECOLOGY_MIGRATIONS`, applied to the parsed RON value) and `regia upgrade-project` support; `cargo xtask check-formats`
holds the field-list hash (`ecology`).

```ron
EcologyProfile(
    format_version: 1,                      // default 1; a newer version is refused (`ecology_invalid`)
    site: (
        moisture: (7.5, 9.5),               // required: soil moisture range, 1 very dry … 10 waterlogged
        reaction: (1.5, 4.0),               // required: soil reaction range, 1 strongly acid … 9 calcareous
        altitude_m: (0.0, 1200.0),          // default unbounded
        slope_deg: (0.0, 12.0),             // default (0, 90)
        soil: "peaty gley at a bog margin", // default ""; shown in messages
    ),
    species: [                              // default []: entries not listed are matched by name
        (entry: "canopy/downy_birch",       // `layer/entry` of the biome
         species: "betula_pubescens",       // an id of the built-in table or of `custom_species`
         role: Dominant,                    // Dominant | Associated | Understory
         share: Some(0.75)),                // default None; the layer's shares sum to 1 and match the entry weights
    ],
    competition: (                          // the rules `biome.ecology` applies; default (shade_rules: true, self_thinning: None)
        shade_rules: true,                  // shade-needing understory only under crowns, light-demanders out of deep shade
        self_thinning: Some((
            cell_m: 25.0,                   // default 25: world-anchored cell size
            max_stems_per_ha: 200.0,        // default 400: most stems per hectare at scale 1
            exponent: 1.605,                // default 1.605: size exponent of the stand density sum
        )),
    ),
    custom_species: [],                     // default []: `Species` rows added to (or replacing, same id) the table
)
```

A `custom_species` row has every field of `regia_procgen::biome::ecology::Species`: `id`, `common_name`, `group`,
`aliases` (default `[]`), `stratum` (`Canopy | Subcanopy | Shrub | Field | Ground`), `moisture`, `reaction`, `indicator`
(moisture, reaction), `light`, `shade_tolerance`, `casts_shade` (default 0), `altitude_m`, `max_slope_deg`,
`succession` (`Pioneer | Mid | Late`), `vernal` (default false).

`EcologyProfile::to_ron` writes the canonical form. Version history: v1 (ADR-2456) is the first.
