# Texture-set census (bindless step 2)

The renderer binds one material group (group 2) per standard material today: the constants and
seven textures (base color, normal, ORM, emissive, detail, detail normal, aux) plus the sampler.
Bindless step 2 (ADR-317, refined by ADR-356) moves the constants out of that group and shares one
group between every material that samples the same seven texture views. How much that saves
depends on the content, so this census measures it on a running scene before step 2 ships.

The census only counts. It binds, draws and reorders nothing, so a frame is byte-identical with
it on or off (`census_on_and_off_draw_identical_pixels`).

## What counts as a texture set

A texture set is the seven views a material group binds, in bind order. Two materials share a set
when they bind the same GPU views:

- Untextured slots bind the renderer's fallback views, so two untextured tints share a set.
- Images shared by identity (`LabMaterial::images`, the shared texture cache) are one view, so
  materials that reference the same shared image share it.
- Two separate uploads of the same image are two views and two sets. The census reports what step
  2 would actually get, not what deduplicating identical images would add.
- A streamed material is keyed by the views it binds now (its group follows residency swaps).
- A material-graph material keeps a group of its own until step 5. It counts under
  `graph_materials` and never shares.

## Read

`render.texture_sets` (no parameters):

```json
{}
→ { "census": true,
    "resident": { "standard_materials": 3, "graph_materials": 0, "texture_sets": 2,
                  "shared_materials": 2, "largest_set": 2 },
    "pass": { "frames": 2, "opaque_draws": 4, "material_runs": 4, "texture_set_runs": 3,
              "materials": 3, "texture_sets": 2, "bind_ratio": 1.333 } }
```

- `resident` is counted when you call, whatever the switch says. `texture_sets` is the number of
  material groups step 2 would keep for the resident standard materials (against
  `standard_materials` today). `shared_materials` counts the materials whose set at least one other
  material binds, and `largest_set` is the most materials on one set.
- `pass` is the last main-view frame drawn while the census was on, and is all zero while it is
  off. The list is the frame's forward opaque draws (visible opaque and alpha-test draws, in the
  order they were collected). Visibility-buffer draws are not in it, because the resolve shades
  them (bindless step 4).
  - `material_runs`: material-group binds in that order today (one per change of material between
    consecutive draws).
  - `texture_set_runs`: the binds with groups shared by texture set and the order unchanged.
  - `texture_sets`: the floor if step 2 also sorted the draws by set.
  - `bind_ratio`: `material_runs / texture_set_runs` (1.0 when nothing would be saved).

The real pass counts can differ from `material_runs`. The draw loops group some draws by pipeline
(graph, alpha-test, double-sided) and the GPU-driven path draws instanced batches. Treat the pass
numbers as the step-2 estimate. The step-1 draw counters (ADR-316) are the measurement once both
are on the same branch.

## Switch the pass census

`render.set_texture_set_census { enabled }` turns the per-frame count on (`true`) or off (`false`,
the default). Any change of the switch restarts `frames` and the pass counts.

```json
{ "enabled": true }
→ { "before": { "census": false, … }, "after": { "census": true, … } }
```

While it is on, the cost is one pass over the opaque list and one hash lookup per draw, under the
`texture_set_census` tracing span. Hosts call `FrameRenderer::set_texture_set_census` and
`FrameRenderer::texture_set_stats` directly.

## Check it

On lookdev, vfx_lab or a DO27 map, turn the census on, let two frames draw, and read
`render.texture_sets`:

- `resident.texture_sets` close to `resident.standard_materials`: the content brings its own
  textures per material, and step 2 saves little until images are shared or packed.
- `pass.bind_ratio` well above 1, or `pass.texture_sets` far below `pass.materials`: step 2 pays.
  Record the numbers in the step-2 lane's report (ADR-356 step 2b acceptance).

## At bindless level 3

With the texture table (`REGIA_BINDLESS_LEVEL=3`, ADR-863, see
[renderer-excellence](renderer-excellence.md) § Bindless level 3) every standard material binds the
same material group, so the opaque pass's `material_batches` in `render.get_frame_stats` is 1
whatever the census says. The census still counts by texture set: it measures what groups shared by
texture set would save on devices that cannot carry the table (tier `material_ssbo`), which is the
open question of ADR-356 step 2b.
