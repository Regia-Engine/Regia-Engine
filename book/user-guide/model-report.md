# Model map and node report

An export check for a glTF/GLB, without a GPU: what is in the model, what is bound, and which
budget it breaks. Built for Tincal's final-asset view ("normal map not bound", "texture 4096²,
3 materials on this node") and usable from any tool.

<!-- ignore-reason: uses `regia_editor`, which is desktop-only and not a dependency of the book's compile check -->
```rust,ignore
# use std::path::Path;
# let path = Path::new("model.glb");
use regia_editor::model_report::{ModelClass, ModelReport};

let report = ModelReport::from_file(path, ModelClass::Character).map_err(std::io::Error::other)?;
println!("{}", report.to_text());           // compact log rendering
let json = report.to_json();               // the agent method's `report`
for flag in &report.flags {
    eprintln!("{}", flag.message);          // "Body: triangles 61000 exceeds the character limit 40000"
}
```

## What it reports

- **Nodes:** the whole tree (`parent`, `children`, `depth`), with triangles, vertices, materials,
  skin and joint count per node; `is_socket`, `is_joint` and `in_scene` flags.
- **Skins, materials:** joints per skin and the meshes bound to it; which nodes use each material.
- **Maps:** one row per material and slot (`base_color`, `normal`, `orm`, `occlusion`, `emissive`)
  with `bound`, `size` and whether any mesh uses the material.
- **Totals:** nodes, triangles, vertices, materials in use, joints, distinct textures, estimated
  texture memory with mips (RGBA8 × 4/3 per distinct image) and the largest texture side.
- **Flags:** budget limits the model breaks, from the `ModelClass` (`prop`, `character`,
  `vehicle`, `building`, `hero`) defaults. Every field of `BudgetLimits` can be overridden; 0 turns a check
  off. `require_normal_maps` flags a material in use with no normal map.
- **Duplicate names** (glTF names are not unique; name lookups return the first) and **orphans**:
  nodes the default scene never reaches.

## The hero class, LODs and glass

`hero` is the budget for a close-up hull (ADR-511): `max_triangles` 350 000 (LOD0, 250-350k expected),
`max_materials` 4 (aim for 3), `max_glass_materials` 1, and LOD checks:

- A node whose name ends in `_LOD<n>` (any case) is level `n`, and so are its descendants. `lods[]` in
  the report lists each level's triangles; level 0 also counts every node that no `_LOD` node contains.
  With LODs the model's triangle total is LOD0 only (the others are alternatives, not additions).
- LOD1/2/3 are expected at 20 % / 5 % / 1.2 % of LOD0 (`lod1_permille` 200, `lod2_permille` 50,
  `lod3_permille` 12); a level more than `lod_ratio_tolerance_percent` (25) above that share is a
  `lod_ratio` flag.
- Hard ceilings: LOD2 under 15k (`max_lod2_triangles` 14 999) and LOD3 under 4k (`max_lod3_triangles`
  3 999), flag `lod_triangles`.
- A material is glass when it uses `KHR_materials_transmission` or its name contains `glass`; more than
  `max_glass_materials` in use is a `glass_materials` flag.

Light skimmers (Remora, Manta; 120-180k triangles at LOD0) use the same class: keep the other hero numbers
and, if a stricter cap is wanted, pass `limits: { "max_triangles": 180000 }`. Characters keep the
`character` class (4-6 materials, cap 8). The LOD and glass checks are off (0) in every other class;
override them through `limits` to use them there.

## Comparing with a spawned hierarchy

`ModelReport::from_spawn(world, &spawned, path, class)` (or `check_hierarchy(world, root)`) also
matches the scene's nodes to the entities below `root` by name. A node with no entity left is an
orphan ("no entity of that name below the model root"); so is an entity the file has no node for.
Duplicate names are matched one to one, `Name#Material` part children and `socket_*` nodes dropped
by `keep_sockets: false` are ignored.

## Notes

- Triangle counts come from accessor counts (lists, strips, fans); a mesh instanced by two nodes
  counts once per node. Only the node's own mesh counts, not its children.
- The class limits are engine defaults, not a standard. Pass your own `BudgetLimits` for a
  project's budget.
- Texture sizes need the images decoded (the glTF crate does that on load); memory use is the
  decoded image, then dropped.
