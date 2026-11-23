# Destruction chunk drawing

`regia_framework::destruction_draw` draws [destruction level 2](destruction.md) structures in pieces
once they break, with the chunk meshes the fracture cook wrote. Before the first break the game's own
kit mesh is the intact look; after it, each chunk that still stands draws under the structure's root
entity and each piece of debris draws under its debris body. It needs the `client` feature of
`regia_framework` (on by default).

The plugin is opt-in: an app that does not add `DestructionDrawPlugin` draws nothing new.

| Method | Permission | What it does |
|---|---|---|
| `physics.destruction_draw` | play | Report what the draw path holds; set the debris LOD and whether intact meshes hide. |

## Setting it up

```rust,wrap
# use regia::prelude::*;
# use std::sync::Arc;
# use regia_asset::{AssetId, AssetServer};
# use regia_physics::{PhysicsPlugin, PhysicsWorld};
# use regia_physics::destruction::{DestructibleAsset, DestructiblePose, Destruction, DestructionPlugin};
# use regia_framework::destruction_draw::{
#     DestructionDrawPlugin, DestructionIntactMesh, DestructionMeshes, load_destruction_meshes,
# };
# use regia_render::{CpuMaterial, CpuMesh, MeshRenderer};
# use regia_scene::Transform;
# let app: &mut App = todo!();
# let mut commands: Commands = todo!();
# let server: &mut AssetServer = todo!();
# let destruction: &mut Destruction = todo!();
# let physics: &mut PhysicsWorld = todo!();
# let (destructible_id, pose, seed): (AssetId, DestructiblePose, u64) = todo!();
# let (kit_mesh, kit_material, cut_material): (Arc<CpuMesh>, Arc<CpuMaterial>, Arc<CpuMaterial>) = todo!();
# let kit_transform: Transform = todo!();
app.add_plugins((PhysicsPlugin, DestructionPlugin::default(), DestructionDrawPlugin));

// Once per destructible asset, keyed by the asset's `name`:
let asset = DestructibleAsset::load(server, destructible_id)?;
let set = load_destruction_meshes(server, &asset, kit_material.clone(), cut_material)?;
app.world_mut().resource_mut::<DestructionMeshes>().register(asset.name.clone(), set);

// Place it as usual, and mark the kit mesh that shows it whole:
let id = destruction.define(physics, Arc::new(asset), pose, seed)?;
commands.spawn((DestructionIntactMesh(id), MeshRenderer::new(kit_mesh, kit_material), kit_transform));
```

`load_destruction_meshes` reads each chunk's `lods` (`<source>#fracture/chunk_<n>`, LOD 0 first)
through the asset server and splits every mesh by material slot: slot 0 is the original surface
(drawn with the first material), the other slot the cut faces (drawn with the second). A game that
already holds the cooked meshes calls `mesh_set_from_cooked`; one that builds its own meshes fills a
`DestructionMeshSet` with `DestructionLodMeshes::from_cpu_parts`.

## What it draws

- **Nothing** while an instance is whole, or when its asset has no registered set.
- **On the first break**, one entity per part (surface, cut faces) of every attached chunk, LOD 0, as
  a child of the instance's root. The marked intact mesh is hidden: its `MeshRenderer` moves into
  `DestructionIntactHidden` and comes back when the instance is whole again (a snapshot restore) or
  removed.
- **Each chunk that detaches** loses its attached draws. If it got a debris body, the body gets the
  chunk at the debris LOD as children placed at `-centroid`. Small chunks, and chunks that fell
  before a snapshot, have no body and simply vanish.
- **When an instance goes** (removed, or replaced by a snapshot) its draws go with their parents.

Chunk meshes are in the asset's space. The root's `Transform` is the instance pose, so attached
draws sit at the identity. A debris body sits at the chunk's centroid with the asset's axes, so the
offset puts the mesh exactly where the body is. Every draw entity carries `DestructionChunkDraw`
(instance, chunk, LOD, cut or surface, debris or attached).

The systems run in `Update`, after the frame's fixed ticks, in instance and chunk order. A parent
that is not spawned yet is retried on the next frame.

## `physics.destruction_draw`

```json
physics.destruction_draw { "debris_lod": 1 }
→ { "changed": true, "destruction": true,
    "settings": { "debris_lod": 1, "hide_intact": true },
    "mesh_sets": ["sample_wall"],
    "stats": { "instances_drawn": 1, "attached_draws": 70, "debris_draws": 4, "intact_hidden": 1,
               "missing_sets": 1, "mismatched_sets": 0, "missing": ["sample_bridge"] } }
```

Optional parameters: `debris_lod` (0 to 255, clamped to each chunk's last LOD; a change redraws every
broken instance) and `hide_intact`. `destruction` says whether the app has `DestructionPlugin`.
`missing` names the assets placed in the world that have no mesh set: register one to see them
break. `mismatched_sets` counts instances whose set has another chunk count than the asset; they are
not drawn.

To check your own work: break something with `physics.destruction_damage`, then expect
`stats.instances_drawn` 1, `attached_draws` one or two per attached chunk (surface, and cut faces
when the chunk has any), `debris_draws` one or two per entry of `physics.destruction_state`'s
`debris_chunks`, and `intact_hidden` 1; take a `seeing.screenshot` to judge the pieces.

Errors:

| Code or reason | When |
|---|---|
| `F0003` | No `DestructionDrawPlugin`. |
| `F0002` | `debris_lod` is not an integer 0 to 255, or `hide_intact` not a boolean. |

Loading a mesh set fails with `DestructionDrawError`, whose `code()` is one of:

| Code | When |
|---|---|
| `destruction_mesh_bad_path` | A `lods` entry is empty or has an empty side of `#`. |
| `destruction_mesh_missing` | The manifest, the paks and the cache do not hold the mesh. |
| `destruction_mesh_decode` | The file is not a cooked mesh, or a submesh range is bad. |
| `destruction_mesh_count` | `mesh_set_from_cooked` got another number of chunks than the asset has. |

## Limits

- Sets are keyed by asset name: two different assets with the same name share one.
- A GPU-ready mesh packed before layout v2 (ADR-2788) has no submesh ranges, so its cut faces draw with
  the surface material until the pak is cooked again (`regia cook --ship`); `assets.cooked_pack` with
  `inspect: true` counts such meshes in `meshes_without_submeshes`.
- Attached chunks always draw LOD 0; distance LOD for attached chunks is the renderer's job.
- The intact mesh is hidden by taking its `MeshRenderer` off; other components on that entity stay.
- A chunk set-level `meshes` file (`.fracture.ron`) is not read at run time: recook so each chunk names
  its `lods`.
