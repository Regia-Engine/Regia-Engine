# Rough transmission

A transmissive surface can spread what it shows behind it: frosted glass, a scuffed visor, ice. The
surface keeps the light a clear pane lets through. Only the direction of that light spreads.

| Where | Name |
|---|---|
| Material field | `LabMaterial::transmission_roughness`, `0..=1`. 0 is clear glass. |
| Authored field | `transmission_roughness` in `.material.ron` v7 ([materials](../user-guide/materials.md)). |
| Shared model | `shaders/rough_transmission.wgsl`, mirrored by `regia_render::rough_transmission`. |
| Tracer field | `PtMaterial::transmission_roughness`. |

## What it does

The transmitted direction follows a GGX reflection lobe mirrored through the sheet, with the
roughness scaled by `0.65 × ior − 0.35`. The reference path tracer samples that lobe per path. The
forward pass averages 16 directions of it per pixel over the refraction copy of the opaque frame.
Each direction lands where it meets the plane of the background seen straight through the pixel. A
per-pixel rotation, advanced per frame, turns the remaining error into grain that the temporal
resolve removes.

## What it costs

A material without it costs nothing: it has no pipeline variant, and the material block is byte for
byte the same (the value rides in `hair_a.x` of a material without hair). A clear thin pane keeps
skipping the refraction pass. A rough pane joins that pass and takes 16 more texture reads and 16 more
depth reads per pixel it covers.

## What it does not do

- It needs a frame that refracts, `FrameQuality::screen_space_refraction` (Desktop High and Ultra).
  Elsewhere the pane draws clear.
- A hair material or a material graph keeps clear transmission (`unsupported` names which).
- A thin film on the pane keeps working: the film sets the weight, the lobe the direction ([thin film](thin-film.md)).
- The forward pass sees only what is on screen and in the opaque copy. Taps that land off screen or
  on something in front of the pane are dropped, so the blur narrows at those edges, and transparent
  surfaces behind the pane do not blur.
- It treats the background as one plane per pixel, so blur across a large depth step is approximate.
- `base.roughness` does not set it, and a glTF file's roughness does not import into it unless the
  material asks: glTF has no separate field for this, and importing it for every file would change
  every existing transmissive asset. A material's `extras.regia_transmission_roughness` opts in, `true`
  for its `roughnessFactor` or a number for that roughness ([glTF extensions](../user-guide/gltf-extensions.md), ADR-2732).

## Agent methods

`material.rough_transmission { entity }` returns:

<!-- not-a-request -->
```json
{
  "entity": 4294967301,
  "roughness": 0.4,
  "active": true,
  "unsupported": null,
  "ior": 1.5,
  "spread_radians": 0.09,
  "forward_taps": 16
}
```

`active` is false when the roughness is 0 or cannot draw. `unsupported` is then `no_transmission`,
`hair`, `material_graph` or `null`. `spread_radians` is the lobe's root-mean-square angle at normal
incidence, as the forward pass's tap pattern samples it.

`material.set_rough_transmission { entity, roughness }` edits the value and returns
`{ entity, before, after }`. The entity gets its own copy of the material.

Errors: `F0002` when `roughness` is not a number in `0..=1`; `F0003` when the entity has no
`MeshRenderer`.

## Checking it

`crates/regia_pt/tests/rough_transmission_parity.rs` compares the forward pass with the path tracer
on a frosted plate in front of a checkered emissive wall, across roughness, IOR and tilt.
`ROUGH_TRANSMISSION_PLATES=<dir>` writes both images of every case.
