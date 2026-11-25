# Materials

<!-- index: Material -->
<!-- index: Standard shader -->
<!-- index: Material graph -->
<!-- index: Shading model -->
<!-- index: Skin shading -->
<!-- index: Eye shading -->
<!-- index: Fabric sheen -->
<!-- index: Skinning -->
<!-- index: Dual quaternion -->
<!-- index: Subsurface scattering -->

Author a surface, cook it, and confirm it on the tier the game will ship. Parameter blocks and
format versions are in [Materials, textures and import](../../user-guide/materials.md). Graphs are
in [Material graphs](../../systems/material-graph.md). Character shading is in
[Shading models](../../user-guide/shading-models.md). The compute skin path is in
[Skinning](../../user-guide/skinning.md).

## Before you start

A `.material.ron` is a shader name plus parameters. The forward pass lights every standard
material with the same code: sun, local lights, image-based light, GI, clearcoat. A material
graph replaces the surface function (base color, normal, roughness, metallic, occlusion,
emissive, alpha) and keeps that lighting. A `MaterialShading` component picks skin, eye, or a
fabric sheen on top of the material. Hair cards are a runtime `LabMaterial` switch and are not a
`.material.ron` block yet.

Ordinary surfaces stay a `.material.ron`. Reach for a graph when the surface needs logic: dust on
upward faces, a height blend, a pulsing emissive.

## 1. Write a standard material

Put the file under `assets/`. Texture paths are project paths. The cooker emits KTX2 for larger
maps and RGBA8 for small ones.

```ron
(
    format_version: 1,
    name: "PaintedMetal",
    shader: Standard,
    base: (
        base_color: (0.55, 0.56, 0.58, 1.0),
        metallic: 1.0,
        roughness: 0.35,
        base_color_map: "textures/paint_albedo.png",
        normal_map: "textures/paint_normal.png",
        orm_map: "textures/paint_orm.png",
        emissive: (0.0, 0.0, 0.0),
    ),
)
```

Later format versions add blocks. A file at an older version loads with the newer defaults:

| Version | Block | Use it for |
|---|---|---|
| 2 | `hard_surface` | Vehicles and structures: multi-scatter and specular occlusion are on when the block is absent. |
| 3 | glass, IOR, emission strength | Clear and tinted panes. |
| 4 | `thin_film` | Soap, oil, coated lenses. |
| 5 | `weather` | Porosity, wetness, snow, and puddles. Absent means the default response: porosity from roughness and metallic, full wetness and snow, no puddles. |
| 6 | `subsurface` | Screen-space scatter for wax, marble, or a material skin profile. Every preset leaves `RenderProfile::subsurface_scattering` off, so the material draws without the blur until a game turns that flag on. The skin shading model is a different path and runs on every tier. |
| 7 | `transmission_roughness` | Frosted transmission. `base.roughness` still shapes the reflection. A glTF roughness does not set this field. |

Frozen samples live in `docs/formats/` (`material.example.ron`, `material-v2.example.ron`, and the
later version files). Copy the shape from there. Do not invent a field the schema does not list.
A shape change needs a format version, a migrator, and `regia upgrade-project`.

Preview before the mesh lands in a level:

```console
regia preview models/helmet.glb --project my_game
```

Cook for the platform you will run:

```console
regia cook --project my_game --platform linux
```

`--quality fast` leaves pak lz4 off. `--quality final` turns it on. Packaging runs the cook.

## 2. Stream the textures

Texture streaming is on in every preset. Standard-surface materials upload low mips first and
step up inside a per-frame budget. A `.meta` sidecar can opt a texture out or set a floor:

```ron
(
    stream: Some(false),
    stream_min_px: Some(512),
)
```

`stream: Some(false)` keeps the texture at the tier's cap, and the whole material follows,
because a material's maps stream together. `stream_min_px` raises the smallest size. A value
below the tier's own floor changes nothing. Material-graph materials stay fully resident.

VeryLow caps a budgeted stream at 384 MiB and turns eager upload off
(`QualityTier::very_low_columns`). A discrete GPU still grows that budget to its VRAM share each
frame. Mobile Low and Mobile High ask for the lite forward shader; it applies on tile GPUs, so a
Mobile profile on a desktop adapter keeps the full shader's pixels.

## 3. Add a graph when the surface needs logic

A `.matgraph.ron` compiles into the forward pass's `material_surface` function. The graph decides
the surface. Lighting stays the engine's. Limits: 15 parameters, 7 textures, 8 nested function
calls, two UV sets. The vertex offset does not bend normals.

Author it in the Material Graph panel or as RON. The editor writes an editor-only
`<graph>.matgraph.layout.ron` beside it for canvas positions. Agents use `editor.matgraph_*`.
The procedure and the node list are in [Material graphs](../../systems/material-graph.md).

## 4. Skin, eyes, cloth, and a skeleton

Put `MaterialShading` on the entity that draws the part, next to its `MeshRenderer`. Entities
that share one material `Arc` share the shading model: the first in draw order wins. Give a part
its own material when it needs its own model. Shading is not inherited from a parent.

| Model | What you get |
|---|---|
| `ShadingModel::Skin` | Pre-integrated subsurface scattering from the surface's own curvature, a blurred diffuse normal, transmission through thin parts, a second specular lobe. It runs on every tier. There is no screen-space pass. |
| `ShadingModel::Eye` | A cornea over a recessed iris, a limbal ring, a wet coat. |
| `FabricSheen` | A grazing sheen for velvet, cotton, or felt. It combines with the standard model and with skin. |

<!-- ignore-reason: a sketch of shading-model inserts; the entity is the caller's -->
```rust,ignore
use regia_render::shading_model::{FabricSheen, MaterialShading, SkinShading};

fn face(world: &mut regia_ecs::prelude::World, entity: regia_ecs::prelude::Entity) {
    world.entity_mut(entity).insert(MaterialShading::skin(SkinShading::default()));
    let _ = FabricSheen { color: [0.6, 0.55, 0.5], roughness: 0.5 };
}
```

The compiled example on the shading-models page inserts skin, eye, and sheen together.

**Skinning.** The default path skins in the vertex shader: four influences, linear blend, repeated
in the shadow, depth, motion, and color passes. The compute path (`SkinningConfig { compute: true }`)
skins once per frame. It adds eight influences (`skin_influences: 8` in the mesh `.meta`, cooked
mesh v3) and optional dual-quaternion skinning (`SkinningOptions::dual_quaternion()` on the
character root; children inherit the method). Eight influences are not inherited: put
`SkinInfluences8` on the mesh entity. The quality tier does not switch this path. You opt in.

Hair, brows, lashes, and fur cards use `LabMaterial::hair` at runtime. See
[Hair cards](../../agents/hair-cards.md).

## 5. Match the material to the light

A material that looks chalky under the default sun is usually a roughness or an exposure, not a
missing feature. Check, in this order:

1. Base color in linear RGB, metallic 0 or 1 for most surfaces, roughness where the highlight
   should sit.
2. The sun in lux and the camera exposure
   ([Light a level](light-a-level.md), [The default look](../../user-guide/default-look.md)).
3. The tier. VeryLow and a Performance upscale quality reconstruct a softer image. Compare on
   the tier you will ship.
4. Lightmaps and baked GI, which replace diffuse indirect light on the receivers they list.
   Metals still take their reflection from the probes and the sky.

Look-dev renders the two terrain stand-ins at Ultra, High, and Deck
([Look-dev harness](../../user-guide/lookdev.md)). That shot list does not include VeryLow or
Cinematic. Render those with `RenderProfilePreset` yourself when the comparison needs them.

## Finished check

1. `regia preview` shows the mesh with the material's maps, and `regia cook` finishes without a
   schema error.
2. `assets.cooked_pack` with `inspect: true` counts `mip_metadata_textures` and
   `unstreamed_textures` the way you authored the `.meta` files.
3. `material.shading` reports the skin, eye, or sheen model on the entity you set, and no
   conflict when two entities share the material.
4. A character on the compute path holds its volume through a 160° twist when dual quaternions
   are on, and a four-influence mesh looks the same as before when you leave the path off.
5. The same camera on High and on the shipping tier shows the material you expect. A graph
   material is fully resident. A standard material may show a lower mip until the streamer catches
   up.

## Where to read more

- [glTF conventions](../../agents/gltf-conventions.md) for LOD groups, sockets, and ORM packing.
- [Material graphs (agents)](../../agents/material-graph.md) and
  [the material graph editor](../../agents/material-graph-editor.md).
- [VFX and decals](../../systems/vfx.md) when the surface is a decal or a particle, not a material
  on a mesh.
