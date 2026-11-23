# Shading models: skin, eye, fabric sheen

Character shading for realistic faces and garments (Tincal engine items B3, B4 and B6). A
`MaterialShading` picks how a material lights, on top of its usual factors and maps:

| Model | What it adds | Cost |
|---|---|---|
| `ShadingModel::Skin` | Light scatters under the skin: the terminator turns soft and warm where the surface curves tightly, the diffuse normal blurs (red most), thin parts (ears, nostrils) glow red when lit from behind, and a second, rougher specular lobe sits under the main one. | A curvature estimate and a 6-term kernel per pixel, a few ALU per light. |
| `ShadingModel::Eye` | The iris is seen through the cornea (refracted, so it shifts with the view as a real recessed iris does), a dark limbal ring, a wet, glossy cornea over the whole eye, and darkening toward the eyeball's edge under the lids. | One extra base-colour sample, the coat lobe. |
| `sheen: Some(FabricSheen)` | A sheen lobe (velvet, cotton, felt): a soft highlight that peaks at grazing angles, with the layer under it dimmed to keep energy. Works with the standard and skin models. | A few ALU per light. |

No new pipeline is built for these materials: they draw with the standard forward pipelines,
which branch on the material's model word. Materials without a model draw exactly as before.

## Using it

```rust,wrap
use regia_render::shading_model::{EyeShading, FabricSheen, MaterialShading, SkinShading};
# use regia_ecs::prelude::World;
# let mut world = World::new();
# let face = world.spawn_empty().id();
# let left_eye = world.spawn_empty().id();
# let jacket = world.spawn_empty().id();
# let cheek = world.spawn_empty().id();

// On the entity that draws the face, the eyes and the jacket (next to its MeshRenderer).
world.entity_mut(face).insert(MaterialShading::skin(SkinShading::default()));
world.entity_mut(left_eye).insert(MaterialShading::eye(EyeShading::default()));
world.entity_mut(jacket).insert(MaterialShading::sheen(FabricSheen { color: [0.6, 0.55, 0.5], roughness: 0.5 }));

// Skin with a peach-fuzz sheen.
world.entity_mut(cheek).insert(MaterialShading {
    sheen: Some(FabricSheen { color: [0.15, 0.12, 0.1], roughness: 0.8 }),
    ..MaterialShading::skin(SkinShading::default())
});
```

**The model belongs to the material.** Entities that share one material `Arc` share its shading:
if they disagree, the first in draw order wins (the agent's `material.shading` reports the
conflicts). Give a part its own material when it needs its own model. Shading is not inherited
from a parent entity: a glTF model's skin, eye and cloth parts are separate entities, each with
its own material.

## Skin

Pre-integrated subsurface scattering (Penner & Borshukov 2011) with the measured skin diffusion
profile of d'Eon & Luebke 2007, evaluated in closed form per pixel from the surface's own
curvature. There is no screen-space pass and no lookup texture, so it runs on every tier.

| Field | Default | Effect |
|---|---|---|
| `scatter_scale` | 1.0 | The measured profile scatters about a millimetre: on a head-sized surface the effect is a soft, warm terminator. Raise it for softer, more translucent (or stylized) skin. |
| `curvature_radius_mm` | 0 | 0 estimates the curvature per pixel; a value fixes it (the visibility-buffer path, which has no derivatives, uses it or 30 mm). |
| `normal_blur` | 0.5 | Diffuse light reads the normal map blurred per channel: red leans this far back to the geometric normal, green half, blue a quarter. This is most of the soft look on a normal-mapped face. |
| `transmission` | 1.0 | Back-light through thin skin. The thickness is the curvature radius times `thickness_scale`, so tight parts (ear rims, nostrils, fingers) glow and broad ones (cheeks, the torso) stay dark. |
| `thickness_scale` | 1.0 | Thickness per mm of curvature radius. |
| `secondary_roughness`, `secondary_weight` | 0.75, 0.15 | The second specular lobe (direct light). |
| `mm_per_unit` | 1000 | World units are metres. |

Limits: the shadow map cuts the scattered light at the terminator where the surface shadows
itself (there is no pre-integrated shadow penumbra yet), and the transmitted light is not
shadowed (another object between the light and a thin ear does not block it). Wrinkle maps,
region tint masks and the screen-space separable SSS of B3 are not in this version; tint masks
come from mask recolor (B7).

## Eye

Map the eyeball so the pupil is at `iris_center` in UV (the usual planar front mapping) and give
the mesh tangents. The iris is drawn under a refracting cornea (index 1.376, Snell's law) onto a
recessed iris plane that is `iris_depth` deep at the pupil and meets the cornea at the limbus.

| Field | Default | Effect |
|---|---|---|
| `iris_center`, `iris_radius` | [0.5, 0.5], 0.16 | Where the iris is in the base colour map (UV). |
| `limbus_width`, `limbus_darkening` | 0.025, 0.6 | The dark ring around the iris. |
| `iris_depth` | 0.04 | 0 turns refraction off. |
| `cornea_ior` | 1.376 | Higher bends more (less parallax). |
| `cornea_roughness` | 0.05 | The wet film's highlight. It uses the clearcoat lobe, so a coat authored on the material is replaced. |
| `socket_occlusion`, `occlusion_start`, `occlusion_end` | 0.35, 0.3, 0.5 | Darkening from UV radius `start` to `end`, for the lids' shadow; use 0 with a separate occlusion shell mesh. |

The refraction needs the standard surface: a material-graph eye gets the cornea and occlusion
but not the refracted iris.

## Fabric sheen

The Charlie sheen distribution (Estevez & Kulla 2017) with Ashikhmin's visibility, and the
`KHR_materials_sheen` albedo scaling of the layer under it. `color` is linear (black is no
sheen), `roughness` 0.07 to 1. Image lighting adds the sheen lit by the irradiance. The glTF importer reads `KHR_materials_sheen` into its extension side table
(`GltfSheen::fabric_sheen` gives the matching `FabricSheen`), but it does not attach a
`MaterialShading` for you, and nothing exports it: set the component in code or through the agent.

## Hosts that render an `ExtractedFrame` themselves

`RenderPlugin` extracts the components into `ExtractedMaterialShading` every frame. A host with
its own render loop passes it on: `renderer.set_material_shading(shading.entries())`, next to
`set_render_overrides`. A host that builds `FrameMesh` lists itself uses its own material keys:
`renderer.set_material_shading([(material_key, MaterialShading::skin(SkinShading::default()))])`.

## Agents

`material.shading` and `material.set_shading` ([agent page](../agents/shading-models.md)).
