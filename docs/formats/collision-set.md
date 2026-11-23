# Collision set (`regia_asset::CookedCollisionSet`), format version 1

The authored collision of an imported glTF (ADR-2341): one shape per `collision_<kind>_*` proxy
node, fitted by the glTF importer (`regia_asset_pipeline::collision_proxies`). It is the payload of
the glTF's `collision` sub-asset (`AssetId::sub_asset(gltf, "collision")`), a cooked file (`FCKD`
header, type tag `regia_asset::CookedCollisionSet`) in the dev cache (`.regia/cooked/<os>/<id>.fckd`)
and in paks. The schema hash is `collision-set` in [`format-hashes.ron`](format-hashes.ron).

**Only a file with proxies has one.** A glTF without `collision_*` nodes writes no `collision`
sub-asset and no `.meta` entry for it, so its cook is byte for byte what it was before this format
existed (`regia_asset_pipeline/tests/gltf_no_proxy_identity.rs` pins it). Proxy nodes are never
drawn (DO-6, unchanged): they are not parts of the `model` sub-asset and get no cooked mesh.

## Proxy names

The node name picks the shape (reserved prefixes are case-sensitive; the node's mesh, every
primitive merged, gives the vertices):

| Name | Shape | Fit |
|---|---|---|
| `collision_box_*` | oriented box | The vertices' box in the node's frame, with the node's scale applied; posed at the box center with the node's rotation. |
| `collision_sphere_*` | sphere | Centered on that box; radius to the farthest vertex. |
| `collision_capsule_*` | capsule | Along the box's longest axis (a tie keeps Y, then X, then Z); radius to the vertex farthest from that axis; half height the box's half length minus the radius, at least 0. |
| `collision_convex_*` | convex hull | Quickhull of the vertices in model space, at most 255 vertices (farthest first). |
| `collision_mesh_*` | triangle mesh | The triangles in model space (static and kinematic bodies). |
| any other `collision_*` (`collision_hull`, `collision_<body>_<n>`) | convex hull | As `collision_convex_*`: the hand-made hull pieces the prefix meant before kinds existed. |

`collision_box` (no suffix) counts as `collision_box_*`, and so on for every kind. Fallbacks are
warnings in the cook log, never silent: a convex proxy whose vertices are flat (or fewer than four)
cooks as a box (`requested` stays `Convex`); a box, sphere or capsule thinner than 0.5 mm is
thickened to it; a mesh proxy without a triangle list, a proxy whose vertices or transform are not
finite, or a `collision_*` node without a mesh gives no shape. A node transform with shear fits the
primitives in its nearest rotation-and-scale frame.

## Space

Shapes are in **model space**: the space of the file's cooked meshes, with node transforms and the
importer's forward-axis turn baked (`Forward::Convert` for `regia cook`). A box, sphere or capsule
stores its frame (`translation`, `rotation`) and its sizes in that frame; a convex hull and a
triangle mesh store model-space points with an identity frame.

## Layout

The payload is [postcard](https://docs.rs/postcard), fields in the order below (integers as
varints, `Vec` as a length plus items, `f32` little endian, enums as a varint variant index).

### `CookedCollisionSet`

| # | Field | Encoding |
|---|---|---|
| 1 | `format_version` | `u32` = 1 |
| 2 | `shapes` | `[CookedCollisionShape]`, at most 4096, in node visit order (document order, depth first) |

### `CookedCollisionShape`

| # | Field | Encoding |
|---|---|---|
| 1 | `node` | `String`, the proxy's node name, at most 1024 bytes |
| 2 | `requested` | `CollisionProxyKind`: `Box` 0, `Sphere` 1, `Capsule` 2, `Convex` 3, `Mesh` 4 (what the name asked for) |
| 3 | `translation` | `[f32; 3]`, the shape frame's origin in the model, meters |
| 4 | `rotation` | `[f32; 4]`, unit quaternion `x, y, z, w` |
| 5 | `geometry` | `CookedCollisionGeometry` |

### `CookedCollisionGeometry`

| Variant | Index | Body |
|---|---|---|
| `Box` | 0 | `half_extents: [f32; 3]`, each above 0 |
| `Sphere` | 1 | `radius: f32`, above 0 |
| `Capsule` | 2 | `half_height: f32` (0 or more), `radius: f32` (above 0), `axis: CollisionAxis` (`X` 0, `Y` 1, `Z` 2) |
| `Convex` | 3 | `points: [[f32; 3]]`, 4 to 4096 |
| `TriMesh` | 4 | `vertices: [[f32; 3]]`, `indices: [[u32; 3]]` (each in range; both non-empty, at most 2²² each) |

## Validation

`CookedCollisionSet::from_postcard` reads the version first (any value but 1 is
`collision_set_version`), then decodes (`collision_set_corrupt` on a damaged payload) and checks
every rule above: finite numbers, positive sizes, a unit rotation (length within 1e-3 of 1), the
point and size limits and index ranges (`collision_set_invalid`, naming the shape).
`regia_asset::load_collision_set_bytes` also checks the `FCKD` type tag
(`collision_set_wrong_type`). The codes are what `assets.collision_report` answers in brackets.

## Using it

- **Collider.** `regia_physics::authored_collision::collider_from_set(&set, dynamic)` returns a
  `Collider`: one shape as the shape with its frame as `offset`, several as a `Compound` at an
  identity offset. With `dynamic`, a triangle mesh becomes the convex hull of its vertices when
  that hull exists (at least four points that enclose a volume). A mesh that is too flat, or has
  fewer than four points, stays a `TriMesh`. The fixed step then skips a dynamic triangle mesh
  and inserts no rapier collider, so the entity can be `Attached` with `trimesh_as_hull` 0 and
  still have no simulated collider.
- **On spawn.** An entity with `regia_physics::AuthoredCollision { mesh }` (a glTF path or id) gets
  that collider the first frame the `AssetServer` can read the set, and an
  `AuthoredCollisionStatus` (`Attached`, `NoProxies` or `Failed`). An explicit `Collider` wins.
- **Agent.** `assets.collision_report { mesh }` lists the shapes
  ([Collision proxies](../../book/agents/collision-proxies.md)).

## Migration

Version 1 is the first version: there is nothing to migrate. `regia upgrade-project` knows the
format (`collision-set`, `.fckd`, cooked files of other types skipped) and reports such files as
current. A cache cooked before this format has no `collision` sub-assets; the next `regia cook`
re-imports every glTF (the cooker's build stamp is in the input key) and a glTF with proxies gets a
new cache key (its settings hash takes the collision-set version), so its outputs are rewritten.
Files without proxies keep their keys.
