# glTF conventions (LOD, sockets, ORM, variants)

The cook path reads `docs/formats/gltf-conventions.md` and writes runtime payloads. No manual
remap after import. Author synthetic files the same way; do not depend on DO27 source assets.

## LOD

Name meshes `Stem_LOD0`, `Stem_LOD1`, `Stem_LOD2`, … any `_LODn` (LOD0 is the most detailed). The cook emits
one `regia_asset::LodGroup` per stem that has a chain. A lone `_LOD0` is not a group. A chain still
requires `_LOD0` plus at least one other level of the same stem; the levels stay sorted by index.

The primary mesh is the first `_LOD0` in the scene walk, or the first mesh whose name has no `_LODn`
tag. A file whose triangle meshes are all `_LODn` with `n > 0` still cooks: the lowest index present
is the primary (the first mesh of that index). That covers a file that is itself one LOD level, and
a clip file whose only mesh is the bind mesh at `_LOD2` or another level. A file that already has
`_LOD0` or an untagged mesh keeps that mesh as the primary. Generated LODs still apply only to
`_LOD0` or an untagged mesh. A file with no triangle mesh fails with `no mesh found in glTF file`.

## Sockets

An empty named `socket_<id>` becomes a `PrefabSocket` with the prefix stripped (`socket_turret`
→ `turret`). The local matrix is stored on `PrefabSocketSet` (`prefab/sockets`). The node's mesh,
if any, is not rendered.

## Collision nodes

A node named `collision_*` (`collision_<body>_<n>` hulls, or an older single `collision_hull`)
holds collision geometry. Its mesh is never rendered: the cook skips it, and so do the editor's
loaders (`load_gltf_model`, `editor.spawn_gltf_hierarchy`; ADR-1622). The rule is
`regia_asset::is_collision_node_name` (case sensitive).

Since ADR-2341 the cook also fits each proxy to a collider shape by its name: `collision_box_*`,
`collision_sphere_*`, `collision_capsule_*`, `collision_convex_*`, `collision_mesh_*` (a static
triangle mesh); any other `collision_*` name is a convex hull. The shapes are the glTF's
`collision` sub-asset; `assets.collision_report` lists them and `regia_physics::AuthoredCollision`
attaches them on spawn. See [Collision proxies](collision-proxies.md).

## Textures

| Map | Cook |
|---|---|
| Base color | sRGB (`TextureCookUsage::ColorSrgb`) |
| Occlusion + metallic–roughness | Packed linear ORM |

ORM channels are **R** occlusion, **G** roughness, **B** metallic, **A** 255. Missing occlusion
is 255; missing metallic–roughness is roughness 255, metallic 0. The cooked material's
`orm_map` points at `texture/orm/{material index}`, not at the raw metallic–roughness image.
Source images are still cooked. Do not hide occlusion in the metallic–roughness R channel.

Images smaller than 4×4 stay `CookedImage` RGBA8 (block compression needs 4×4). Larger maps
become KTX2.

## Variants

Materials named `<slot>_Rem` and `<slot>_A52` (both required) become a `MaterialVariantSet`.
Spawn it with `PrefabInstance::new(set, "Rem")` and switch at runtime:

```rust
use regia::core::id::Guid;
use regia_asset::{AssetId, MaterialVariant, MaterialVariantSet, PrefabInstance};

fn select_variant() -> Option<PrefabInstance> {
    // The cook path emits a `MaterialVariantSet` for a slot (`hull_Rem` / `hull_A52`).
    let set = MaterialVariantSet {
        slot: "hull".into(),
        variants: vec![
            MaterialVariant { name: "Rem".into(), material: AssetId(Guid::from_u128(1)) },
            MaterialVariant { name: "A52".into(), material: AssetId(Guid::from_u128(2)) },
        ],
    };
    let mut prefab = PrefabInstance::new(set, "Rem")?;
    prefab.select("A52"); // swaps PrefabInstance.material to the A52 cooked material
    Some(prefab)
}
```

`select` returns `false` and leaves the instance unchanged when the name is missing. The
cooked `CookedStandardMaterial.authored.base.base_color` of the selected asset is the color
that changes.
