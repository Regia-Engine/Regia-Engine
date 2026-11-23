# glTF as a named hierarchy and the wireframe view

For tools that must address the parts of a model by name (Tincal, a turret editor, a socket
placer) rather than place a baked prop.

## `spawn_gltf_hierarchy`

<!-- ignore-reason: uses `regia_editor`, which is desktop-only and not a dependency of the book's compile check -->
```rust,ignore
# use std::path::Path;
# use regia_ecs::prelude::World;
# let path = Path::new("model.glb");
# let mut world = World::new();
use regia_editor::gltf_hierarchy::{GltfSpawnOptions, spawn_gltf_hierarchy};

let spawned = spawn_gltf_hierarchy(&mut world, path, &GltfSpawnOptions::default())
    .map_err(std::io::Error::other)?;
let muzzle = spawned.entity("socket_muzzle");        // name -> entity
let some_entity = world.spawn_empty().id();
let node = spawned.node_of(some_entity);             // entity -> node (name, parent, triangles, materials)
println!("{:?}", spawned.report);                    // counts, duplicate names, warnings
```

- One entity per scene node: `Name`, `ChildOf`, the node's local `Transform`. A root entity named
  after the file stem (`root_name`) holds the top-level nodes.
- Meshes are in node-local space. A node with several materials gets named child entities
  (`Node#Material`) that map back to the node in `node_of`.
- Empty nodes, including `socket_*`, are empty entities (`keep_sockets: false` drops them).
- `collision_*` nodes (collision hulls) are spawned as entities with `GltfCollisionNode` but are
  not drawn, as in the cook (ADR-1622). `show_collision: true` draws them for debugging. The
  merged-mesh loader (`load_gltf_model`) leaves them out as well.
- A `KHR_lights_punctual` node carries the engine's `PointLight`, `SpotLight` or `DirectionalLight`
  (`lights: false` leaves them out). Point intensity goes from candela to lumens (× 4π). A light with
  no `range` ends where it falls to 1 lux. A directional light's direction is set once at spawn, so it
  does not follow a moved root. The renderer draws one directional light, so a model's joins only a
  scene that has none; the report warns when it was left out.
- A skin becomes a `Skeleton` and a `SkinnedMesh` on each skinned mesh (spec 17 § 2). Its
  `joints` are the joint nodes' own entities, so moving a joint entity moves the bone. The
  bind-pose `SkinJoints` is set at spawn; `SkinnedMeshPlugin` rewrites it from the posed joints
  every frame ([Morph targets and skinned meshes](morph-targets.md)).
- A mesh with morph targets gets `MeshMorphTargets` (per material part), `MorphWeights` at the
  file's default weights and `MorphTargetNames` (`extras.targetNames`), so it draws deformed.
- No axis conversion: skinned and rigid vertices share the file's frame.
- `editor_only` tags every entity with `GltfEditorOnly` for saves and exports to skip.

Limits: default scene only, `JOINTS_0`/`WEIGHTS_0`, triangle lists. Skins read the GLB's embedded
buffer; a `.gltf` with external buffers gets bind poses computed from the joint transforms.

## Wireframe

`PaneRequest::debug_view = DebugViewMode::Wireframe` overlays every mesh's edges (green) on the
lit editor pane. A game host inserts the resource instead:

```rust,wrap
# use regia_ecs::prelude::World;
# let mut world = World::new();
world.insert_resource(regia_render::DebugView {
    mode: regia_render::DebugViewMode::Wireframe,
    ..Default::default()
});
```

The lines are drawn by the gizmo pass after post, so the colour is exact and the forward and
GPU-driven paths behave the same. Every other `DebugViewMode` value is implemented too (unlit, normals,
base color, roughness, metallic, AO, overdraw, light complexity, shadow cascades, LOD coloring, motion
vectors, mip level; see [Render lights](../agents/render-lights.md)).

## UV checker, texel density and material ID

These swap each mesh for an unlit debug copy, so the colors are exact on every path:

<!-- ignore-reason: uses `regia_editor`, which is desktop-only and not a dependency of the book's compile check -->
```rust,ignore
use regia_render::DebugViewMode;
# use regia_editor::render::PaneRequest;
# use regia_editor::viewport::EditorCamera;
# use regia_editor::views::ViewKind;
# let mut request = PaneRequest {
#     pane: 0, kind: ViewKind::Perspective, camera: EditorCamera::default(),
#     width: 1, height: 1, ids: false, warm_frames: 0,
#     debug_view: Default::default(), reference: None,
# };
// One checker cell = 25 cm of surface.
request.debug_view = DebugViewMode::UvChecker { tile_m: 0.25 };
// Blue under 512 px/m, green within 10 percent, red over.
request.debug_view = DebugViewMode::TexelDensity { target_px_per_m: 512.0, tolerance: 0.1 };
// One stable color per material name; the legend is in `PaneInfo::debug_legend`.
request.debug_view = DebugViewMode::MaterialId;
```

Name materials with the `MaterialLabel` component (`spawn_gltf_hierarchy` does). To get the
legend as a callback, insert `regia_render::DebugLegendSink(Arc::new(|legend| ...))`; it
receives the `LegendEntry` rows (`name`, `color`) each frame drawn in material ID mode. In a game window set the
`DebugView` resource's `mode` instead. A material with no base-color texture counts as 1024
pixels square in texel density.
