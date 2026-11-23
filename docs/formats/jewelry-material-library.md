# Material library (`library.json`, `jewelry.material_library/1`)

Schema: [`jewelry-material-library.schema.json`](jewelry-material-library.schema.json). Shared rules and
refusal reasons: [jewelry-contracts.md](jewelry-contracts.md). ADR-2480, ADR-2535. Registry name:
`jewelry-material-library`. Format version 1 (the major in `schema`); no migrator yet.

Every material a viewer can show, as simple look data. Path:
`$JEWELRY_OUT/materials/<library_version>/library.json`. Produced by the kit builder, seeded from published
measurements; the values live in the library, never in engine code. Regia maps each entry to a cheap look
(below): one baked environment map, no ray tracing, no bounce loops, no spectral solve at runtime.

## Fields

- **Top level:** `schema`, `library_version` (≥ 1), `working_space` (`linear_rec709`, `acescg`), `materials[]`.
- **Every entry:** `material_id`; `kind` (conductor, anodized, gem_dielectric, gem_special, finish);
  `display_name` (free text, marked); `catalogue_facets[]` (the option labels it maps from, `<group>=<label>`);
  `source { citation, dataset?, retrieved? }`; `status` (measured, estimate); `calibration { method: photo_fit |
  none, target_lab?, achieved_de00?, date? }`; and exactly one block named after its kind. Required fields first,
  then the optional ones:
  - `conductor`: `f0_rgb_linear`. Optional: `roughness`, `f82_tint_rgb`, `plating` (none, rhodium, black_rhodium),
    `nk { wavelength_nm[], n[], k[] }`, `nk_dataset`. RGB values may exceed 1 (a gold's F0 red in linear sRGB).
  - `anodized`: `substrate_material_id` (a conductor) and at least one of `color_rgb_linear` and `fresnel_lut
    { cos_theta: "uniform_0_1", entries: 8–64 × [r, g, b], inputs_hash? }` (entry `i` at cos θ = i / (n − 1)).
    Optional: `roughness`, `catalogue_name`, `oxide { model: dataset | sellmeier | cauchy, dataset?, coefficients?,
    k? }`, `thickness_nm`, `thickness_jitter_nm`, `voltage_hint_v`.
  - `gem_dielectric`: `n_d` and at least one of `tint_rgb_linear`, `attenuation_rgb_per_mm` and
    `absorption_spectrum { wavelength_nm[], per_mm[] }`. Optional: `abbe_vd`, `sparkle` (0–1), `surface` (polished,
    frosted), `surface_roughness`, `sellmeier { b[3], c_um2[3] }`, `birefringence`, `scatter`, `inclusions` (none,
    light, natural), `ior_family`.
  - `gem_special`: `model` (opal, pearl, phenomenal), `body_rgb_linear`. Optional: `sphere_spacing_nm`,
    `patch_scale_mm`, `orient_strength`, `adularescence`, `chatoyancy`.
  - `finish`: `finish_kind` (polish, brushed, sandblast, hammered, textured), `roughness`. Optional: `anisotropy
    { strength, direction_source }`, `detail_normal_ref`, `feature_scale_mm`.

## How the viewer maps an entry

| Entry | The viewer uses | Accepted and ignored |
|---|---|---|
| `conductor` | base color = `f0_rgb_linear`; roughness = the picked finish's `roughness`, else `roughness`, else 0.1 | `f82_tint_rgb`, `plating`, `nk`, `nk_dataset` |
| `anodized` | `fresnel_lut` as a small lookup by cos θ, else `color_rgb_linear` as a plain base color; roughness as for a conductor | `substrate_material_id` beyond the check, `oxide`, `thickness_nm`, `thickness_jitter_nm`, `voltage_hint_v` |
| `gem_dielectric` | fake refraction from `n_d`; tint below; sparkle = `sparkle`, else `clamp((60 − abbe_vd) / 40, 0.1, 1)`, else 0.3; the frosted shader when `surface` is `frosted`, else the cut's shader | `surface_roughness` beyond the frosted shader, `sellmeier`, `birefringence`, `scatter`, `inclusions`, `ior_family` |
| `gem_special` | `model` selects a simple shader (opal, pearl, phenomenal); `body_rgb_linear` is its color; `patch_scale_mm` is the opal's patch size | the rest |
| `finish` | `roughness` overrides the metal's; `anisotropy.strength` where the shader has it; `detail_normal_ref` when the package carries the texture | `feature_scale_mm` |

**Gem tint**, in order: `tint_rgb_linear` as given; else `exp(−attenuation_rgb_per_mm × 2 d)`, where `d` is the
stone's depth in mm (table to culet, or its `size_mm` depth); else the same with the attenuation taken as the
mean of `absorption_spectrum.per_mm` over 600–700, 500–600 and 400–500 nm (a cook-time reduction, nothing
spectral reaches the shader).

**Stone shader:** `gem_special.model` when the entry is special; else `cabochon` when the cut's `cut_type` is
cabochon; else `frosted` when `surface` is frosted; else the faceted stone.

A gem instance is a recipe gem slot, a part-kit gem cut and an entry chosen by the option.

## Checks beyond the schema

`material_id` is unique (`contract_duplicate_id`); no option label is claimed twice
(`contract_duplicate_facet`); an anodize substrate exists (`contract_unknown_reference`) and is a conductor
(`contract_wrong_kind`); a gem whose color fields are all `null` is refused (`contract_missing_field`); spectral
arrays have one value per wavelength and wavelengths increase (`contract_bad_spectrum`). The schema itself
refuses a kind without its block or with another kind's block.

## Fixtures

`crates/regia_configurator/tests/fixtures/jewelry-material-library/library-basic.json`: synthetic values only. Two
conductors, an anodized entry with a 16-entry table (a smooth test curve, not a physical film) and one with a plain
color, two clear gems (published n_d and Abbe numbers), a polish finish, a conductor with spectral n,k arrays, a gem
with Sellmeier terms and an absorption spectrum, a frosted stone, a play-of-color special stone and a brushed finish.
