# Collision proxies

A prop's collision is often simpler than what it draws: a crate collides as a box, a lamp post as a
capsule, a rock as a few convex pieces. Author those shapes in the DCC tool as extra meshes on
nodes named `collision_<kind>_<name>`, export one glTF, and `regia cook` fits each one to a collider
shape (ADR-2341). The proxies are never drawn. In a level, an entity with
`regia_physics::AuthoredCollision` gets them as its `Collider` when it spawns, and
`assets.collision_report` shows what the cook made.

## Naming

| Node name | Collider | How the cook fits it |
|---|---|---|
| `collision_box_*` | oriented box | The vertices' box in the node's frame (node scale applied), turned with the node |
| `collision_sphere_*` | sphere | Centered on that box, radius to the farthest vertex |
| `collision_capsule_*` | capsule | Along the box's longest axis; radius to the vertex farthest from the axis |
| `collision_convex_*` | convex hull | The hull of the vertices, at most 255 points |
| `collision_mesh_*` | triangle mesh | The triangles as they are (static and kinematic bodies) |
| any other `collision_*` (`collision_hull`, `collision_turret_0`) | convex hull | As `collision_convex_*` |

The kind may also end the name (`collision_box`). Names are case-sensitive. Model a box proxy as any
mesh whose bounds are the box (a cube is enough), and rotate the node, not the vertices, for a
turned box: the fit uses the node's frame. Several proxies on one prop make one compound collider.

A file without `collision_*` nodes cooks exactly as before; nothing is added. What the cook could
not fit is a warning in the cook log, never a silent skip: a flat convex proxy becomes a box, a flat
box gets a 0.5 mm thickness, a `collision_mesh_*` node without triangles or a `collision_*` node
without a mesh gives no shape. Format: `docs/formats/collision-set.md`.

## Attaching on spawn

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_render::MeshSource;
use regia_physics::{AuthoredCollision, AuthoredCollisionStatus, RigidBody};

commands.spawn((
    MeshSource::new("assets/props/crate.glb", ""),
    RigidBody::static_body(),
    AuthoredCollision::new("assets/props/crate.glb"),
));
// A frame later (with the AssetServer resource present) the entity has a Collider built from the
// proxies, and AuthoredCollisionStatus::Attached { shapes, trimesh_as_hull }.
```

`AuthoredCollision { mesh }` is saved in levels and prefabs (`mesh` is the glTF path or asset id).
The `PhysicsPlugin` system `attach_authored_colliders` runs in `PreUpdate`, so the collider joins the
same frame's fixed step. The rules:

- It is opt-in: an entity without `AuthoredCollision` never gets a collider from proxies.
- An explicit `Collider` on the entity wins; the component is then ignored.
- The collider goes on the entity: it joins the entity's `RigidBody`, or the nearest ancestor body,
  or is a fixed collider when there is neither.
- A dynamic body cannot use a triangle mesh. Each `collision_mesh_*` shape becomes the convex
  hull of its vertices when that hull exists (at least four points that enclose a volume),
  counted in `trimesh_as_hull`, with a warning. A mesh that is too flat, or has fewer than four
  points, stays a triangle mesh: `trimesh_as_hull` stays 0, the attach does not warn, and the
  status is still `Attached`. The fixed step then skips that dynamic triangle mesh (it logs that
  the mesh was skipped) and inserts no rapier collider, so `ColliderHandle` stays absent while
  the `Collider` component remains.
- A glTF without proxies gives `AuthoredCollisionStatus::NoProxies` and no collider; an unreadable
  one gives `Failed { code, message }`.

In code without an entity: `regia_physics::authored_collision::load_authored_collision(&mut server,
"assets/props/crate.glb")` reads the set and `collider_from_set(&set, dynamic)` builds the collider.

## `assets.collision_report`

`assets.collision_report { mesh }` reads the glTF's cooked `collision` sub-asset through the running
asset server. `mesh` is the glTF's project-relative path (`assets/` may be left out) or its asset id.
It is read-only.

```json
{ "method": "assets.collision_report", "params": { "mesh": "assets/props/crate.glb" } }
{ "result": {
    "mesh": "assets/props/crate.glb",
    "id": "5f0c…", "collision_id": "a71e…",
    "has_collision": true, "shape_count": 2,
    "counts": { "box": 1, "sphere": 0, "capsule": 0, "convex": 1, "mesh": 0 },
    "fallbacks": 0,
    "shapes": [
      { "node": "collision_box_lid", "kind": "box", "requested": "box",
        "translation": [0.0, 0.5, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "half_extents": [0.5, 0.5, 0.5] },
      { "node": "collision_convex_body", "kind": "convex", "requested": "convex",
        "translation": [0.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "points": 8 }
    ] } }
```

Per shape: `node`, `kind` (what was cooked), `requested` (what the name asked for; they differ for a
fallback, counted in `fallbacks`), the frame `translation` and `rotation` (`[x, y, z, w]`) in model
space, and the sizes of its kind: `half_extents` (box), `radius` (sphere), `half_height`, `radius` and
`axis` (`x`, `y` or `z`; capsule), `points` (convex: the hull's point count), `vertices` and
`triangles` (mesh). A glTF without proxies answers `has_collision: false`, `shape_count: 0` and zero
counts.

Try it on any cooked project with the agent CLI:

```sh
regia agent call assets.collision_report '{"mesh": "assets/props/crate.glb"}'
```

## Errors

| Code | When |
|---|---|
| `F0002` with `[bad_mesh]` | `mesh` is missing or empty |
| `F0016` | No manifest entry for the glTF, or the app has no `AssetServer` |
| `F0013` with `[collision_set_corrupt]` | The cooked set does not decode |
| `F0013` with `[collision_set_invalid]` | A shape breaks a rule of the format (non-finite, size, index) |
| `F0013` with `[collision_set_version]` | A set version this build does not read: re-cook |
| `F0013` with `[collision_set_wrong_type]` | The file at the set's id is another asset type |
| `F0016` with `[collision_set_unreadable]` | The set exists but could not be read (IO) |

`AuthoredCollisionStatus::Failed` carries the same lowercase codes, plus `unknown_mesh` for a path with
no manifest entry.
