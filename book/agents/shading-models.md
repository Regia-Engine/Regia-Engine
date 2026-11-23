# Shading models: skin, eye, fabric sheen

Character materials (Tincal B3, B4, B6). The model belongs to a mesh entity's material: every
entity that shares the same material `Arc` draws with the first one's model. User guide:
[Shading models](../user-guide/shading-models.md).

## `material.shading`

One mesh entity's model, or (no `entity`) every entity that has one.

```json
{ "method": "material.shading", "params": { "entity": 4294967301 } }
{ "method": "material.shading", "params": {} }
```

With `entity`: `{ entity, model: "standard" | "skin" | "eye", skin? | eye?, sheen: { color,
roughness } | null, material_key, shares_material_with: [entity] }`. Without: `{ entities: [...],
table: { materials, shaded_draws, conflicts } | null, renderer: { materials } | null }`. `table`
is the last extracted frame: `conflicts` counts draws whose entity disagrees with an earlier
entity on the same material (that entity's model is drawn, not theirs). `renderer.materials` is
how many materials the runtime renderer shades with a non-standard model.

## `material.set_shading`

Params: `entity` (required); `model` (`standard`, `skin`, `eye`; default: the one a `skin` or `eye`
object implies, else unchanged); `skin` and `eye` objects with any of the fields below (omitted
fields keep their value; switching model starts from that model's defaults); `sheen`
`{ color?, roughness? }` (linear `[r, g, b]` or `"#RRGGBB"`, or `null` to remove the lobe);
`remove: true` deletes the component.

```json
{ "method": "material.set_shading", "params": { "entity": 4294967301, "model": "skin" } }
{ "method": "material.set_shading", "params": { "entity": 4294967301, "skin": { "scatter_scale": 2.0, "transmission": 0.6 } } }
{ "method": "material.set_shading", "params": { "entity": 4294967302, "model": "eye", "eye": { "iris_radius": 0.15, "iris_depth": 0.03 } } }
{ "method": "material.set_shading", "params": { "entity": 4294967303, "sheen": { "color": "#C08090", "roughness": 0.4 } } }
{ "method": "material.set_shading", "params": { "entity": 4294967303, "remove": true } }
```

Returns `{ before, after }`.

| `skin` field | Default | Meaning |
|---|---|---|
| `scatter_scale` | 1 | Multiplies the measured skin profile's scattering distances (above 1 is softer). |
| `curvature_radius_mm` | 0 | Fixed curvature radius in mm (0 = estimated per pixel from the surface). |
| `normal_blur` | 0.5 | How far red (green half, blue a quarter) diffuse leans back to the geometric normal. |
| `transmission` | 1 | Weight of light passing through thin skin from behind. |
| `thickness_scale` | 1 | Thickness per mm of curvature radius for the transmission. |
| `secondary_roughness` | 0.75 | Second specular lobe's roughness. |
| `secondary_weight` | 0.15 | Second lobe's share (0 = one lobe). |
| `mm_per_unit` | 1000 | Millimetres per world unit. |

| `eye` field | Default | Meaning (UV units of the eyeball map) |
|---|---|---|
| `iris_center` | [0.5, 0.5] | Pupil centre. |
| `iris_radius` | 0.16 | Iris (limbus) radius. |
| `limbus_width`, `limbus_darkening` | 0.025, 0.6 | The dark ring at the iris edge. |
| `iris_depth` | 0.04 | Depth of the iris under the cornea at the pupil (0 = no refraction). |
| `cornea_ior`, `cornea_roughness` | 1.376, 0.05 | Refraction index and the wet cornea's specular roughness. |
| `socket_occlusion`, `occlusion_start`, `occlusion_end` | 0.35, 0.3, 0.5 | Darkening toward the eyeball's edge. |

Check it worked: `seeing.screenshot` before and after the edit and compare; the entity's material
must not be shared with an entity that should stay standard (`shares_material_with`), and
`material.shading {}` should show `conflicts: 0`.

Errors: `F0003` (`-32001`, not found) when `entity` is not a mesh entity; `F0002` (`-32602`,
invalid params) for an unknown model or malformed `skin`, `eye` or `sheen` value.

Captured on lavapipe (llvmpipe Vulkan): the ephemeral cuboid entity had no `MeshRenderer`, so the
handler returned `F0003` before it validated `model`.

```json
{"jsonrpc":"2.0","id":5,"method":"material.set_shading","params":{"entity":4294967094,"model":"Skin"}}
{"jsonrpc":"2.0","id":5,"error":{"code":-32001,"message":"entity 4294967094 has no MeshRenderer","data":{"code":"F0003","hint":"a shading model goes on a mesh entity: pick one from scene.query_entities"}}}
```

For light that spreads over a whole shadow edge, turn on [screen-space subsurface scattering](subsurface.md). A skin material then swaps its pre-integrated falloff for the blur.
