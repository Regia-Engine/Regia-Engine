# Thin film (iridescence)

A thin dielectric film on a surface reflects from both of its faces, and the two reflections
interfere. The reflected color then shifts with the view angle and the film's thickness: soap
bubbles, oil on water, beetle shells, bird feathers. glTF calls this `KHR_materials_iridescence`.

## What it does

The film replaces the Fresnel term of the base specular. The direct lobes (sun and local lights)
take the film's reflectance at `l·h`. The environment, the multi-scatter energy and the fitted area
lights take an F0 that gives the film's reflectance at `n·v`. Diffuse light under the film loses
what the film reflects. Clearcoat, anisotropy and sheen stack on top as they do on a plain material.

| Field | Meaning | Default |
|---|---|---|
| `factor` | Share of the surface the film covers, 0..=1. 0 is no film. | 0 |
| `ior` | Index of refraction of the film, at least 1. | 1.3 |
| `thickness_min` | Thickness in nanometers where the thickness map reads 0. | 100 |
| `thickness_max` | Thickness in nanometers where the thickness map reads 1, and everywhere without a map. | 400 |
| `factor_map` | Coverage, read from R and multiplied into `factor`. | none |
| `thickness_map` | Thickness, read from G. | none |

Useful thicknesses run from about 100 to 800 nm. A film under 30 nm fades out, so 0 nm is exactly the
plain material. Past a few micrometers the colors average away to a neutral reflectance.

## What it costs

A material without the film costs nothing: it draws through the standard pipelines and its material
block is byte for byte the same as before the feature. A filmed material draws through the
`THIN_FILM` variant of the forward shader, from the CPU color loop, with its own group 3. One
filmed material leaves the GPU-driven batches and the visibility buffer; its depth prepass and
shadows stay where they were.

Frame warms build the variant. A filmed material first drawn mid-play keeps the plain Fresnel until
the variant is built, so a first use never stalls a frame (`fallback` below counts those draws).

## Surfaces that do not take the film

The shipped forward pass draws the film only for a standard surface that binds nothing else in
group 3. A transmissive surface (`transmission > 0`), a masked instance and a lightmap receiver
keep the base Fresnel. ADR-2732 (proposed) describes routes that would bind both features; those
routes are not drawn, and `ThinFilmStats::transmissive`, `masked` and `lightmapped` stay 0.

## What it does not do

- A material graph draws without the film: its code owns the surface. `material.thin_film` names
  this `unsupported: "material_graph"` and `active` is false.
- A transmissive material (`transmission > 0`) draws without the film. `material.thin_film` names
  this `unsupported: "transmissive"` and `active` is false.
- Per-instance glass (a damaged or own-canopy instance) draws without the film: its group 3 holds a
  damage page.
- An x-ray override draws without the film.
- A masked instance and a lightmap receiver draw without the film.
- On a Mobile lite tier, an opaque or masked filmed material draws through the lite shader without
  the film. A blended, premultiplied or additive filmed material keeps the film on that tier, unless
  one of the exclusions above also applies. A transmissive material does not keep it.

The frame counts each of those skips in `renderer.unsupported`. `material.thin_film` only names
`material_graph` and `transmissive`. A filmed material that is neither still answers `active: true`
and `unsupported: null`, even when the frame later skips it for a mask, a lightmap, an x-ray
override or an opaque lite-tier draw.

The film's two maps use the material's first UV set. glTF `texCoord` and `KHR_texture_transform` on
them are kept in the extension table, not applied.

## Names

| Where | Name |
|---|---|
| Material fields | `LabMaterial::thin_film: ThinFilmFactors`, `LabMaterial::thin_film_map` |
| Authored block | `thin_film` in `.material.ron` v4 ([`ThinFilmParams`](../user-guide/materials.md)) |
| Packed map | `regia_render::pack_thin_film_map`: coverage in R, thickness in G |
| CPU reference | `regia_render::thin_film::{reflectance, fresnel, schlick_f0, weight, spectral_reflectance}` |
| Renderer counts | `FrameRenderer::thin_film_stats` |
| Shader axis | `THIN_FILM` (`FORWARD_AXES`) |
| glTF import | `KHR_materials_iridescence` ([glTF extensions](../user-guide/gltf-extensions.md)) |
| Agent | `material.thin_film`, `material.set_thin_film`; `material.shading` lists `thin_film` too |

## Agent methods

`material.thin_film { entity }` returns
`{ entity, enabled, active, unsupported, factor, ior, thickness_min, thickness_max, has_map, renderer }`.
`enabled` says the factor is above 0. `active` is true when the factor is above 0, the material has
no surface graph and `transmission` is 0. When `active` is false, `unsupported` is `material_graph`
or `transmissive`.
`renderer` is `{ materials, drawn, fallback, unsupported }` for the last frame, or `null` without a
renderer. It does not include `transmissive`, `masked` or `lightmapped`.

`material.set_thin_film { entity, factor?, ior?, thickness_min?, thickness_max? }` edits the film and
returns `{ entity, before, after }`. An omitted field keeps its value, and `factor: 0` turns the film
off. The entity gets its own copy of the material, so entities that shared it keep theirs. The map
stays as it is.

Errors: `F0002` for a value out of range or not finite as `f32`, `F0003` when the entity has no
`MeshRenderer`. A rejected edit keeps the entity's existing material.

## Checking a film

The closed form is held to a brute-force spectral integral
(`regia_render::thin_film::spectral_reflectance`). A transmissive filmed pane draws the same pixels
as the same pane with the film off, and the frame counts that draw in `unsupported`
(`a_transmissive_film_is_counted_and_draws_without_the_film`).
