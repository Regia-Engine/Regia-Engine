# Scatter gameplay colliders

Seeded scatter ([terrain scatter](terrain-scatter.md)) is drawn by the `ScatterField`. Its
rocks are visual. A game whose simulation must stop a walking body at the large rocks, on
every peer, needs the same rocks as data. `regia_terrain` gives it three pieces (ADR-8029):

- a **world-anchored placement grid**: rocks inside a map do not move when the map grows by a
  backdrop ring, or when a low tier drops the ring;
- a **collider bake**, `bake_scenery_colliders`: the same deterministic placement on the CPU (no
  GPU, no cook), one sphere per instance that stands taller than a threshold;
- a **push-out query**, `SphereBlockers`: moves a vertical body out of the spheres that block it.

The agent method `terrain.scatter_colliders` runs the bake over the world's terrain.

## Map-edge and world-anchored grids

Each scatter class places its instances cell by cell. A cell's instances are a pure function
of the set seed, the class name and the cell's index. So the anchor of the cell grid decides
where every rock goes.

- **Map edge** (`PlacementOrigin::MapEdge`, the default). Cell `(0, 0)` starts at the map's
  −X/−Z corner. This is how every earlier release placed, bit for bit. If you grow the map, every
  cell is renumbered and every rock moves.
- **World** (`PlacementOrigin::World(WorldAnchor)`). Cells are counted from a fixed world point,
  `origin`, in both directions. Cells may have negative indices. Growing or shrinking the map
  around the anchor leaves the cells that both maps cover unchanged.

Under a world anchor, heights and normals are read relative to the anchor's sample, never
through the map's extent. Two maps give the same bits for a rock when all of these hold:

- they hold the same samples at the same world points;
- they have the same sample spacing;
- the anchor sits on a sample of each. `WorldAnchor::lattice_aligned(map)` returns that sample,
  or `None`. An unaligned anchor still places deterministically, but reads the map by its
  extent, so the bits may differ between map sizes.

Three inputs still follow the map placed on:

- **Density maps.** A class density map is stretched over the map it is given with.
- **Layer altitude range.** A layer band that reaches the map's lowest or highest ground keeps
  full weight there. A backdrop of another altitude moves that range. Pin it with
  `WorldAnchor::pinned_to(origin, &inner_map)` or `altitude_range: Some([lo, hi])`.
- **The edge strip.** Within one sample of a map's edge, the normal is one-sided. A rock there
  may differ from the same spot on a larger map.

A host opts in on both sides:

<!-- ignore-reason: a host fragment; the set, maps, mesh library and extent lookup come from the host's own loading code -->
```rust,ignore
let origin = PlacementOrigin::World(WorldAnchor::pinned_to([0.0, 0.0], &inner_map));
let field = ScatterField::new(&set, source, &library, key_base)?.with_placement_origin(origin);
let colliders = bake_scenery_colliders(&set, inner_inputs, &origin, extent_of, &settings)?;
```

`ScatterField::set_placement_origin` drops every resident tile. The next update places them
again. Switching an existing map to a world anchor moves its rocks once, so re-bake its
colliders when you switch.

Give the same origin to every other reader of the placement (ADR-8496), or it answers with the
map-edge rocks:

- **Forest queries** (`ScatterQuery`, which the forest flight, the corridor tool and the
  `forest.*` agent methods read): `ScatterQuery::with_placement_origin(origin)`. Its cell
  indices (`ScatterHit::cell`, `cell_range`, `cell_hits`) then count from the anchor and may be
  negative.
- **The HLOD scatter proxies** of the stream cook: set `StreamScatter::placement_origin` (the
  map's local frame, as the field). The level file does not record an origin, so a cook from
  the command line places from the map edge.

The bake places only the seeded instances: painted ones (`ScatterField::set_painted`) never become
colliders; a host that needs them adds their spheres itself.

A runtime height edit (`ScatterField::heights_changed`) re-sits the rocks of every tile under it,
negative tile indices included, onto the ground placement reads under the field's origin, and
drops the tiles' background placements in flight so they are placed again on the new ground.

## The bake

`bake_scenery_colliders(set, inputs, origin, extent_of, settings)`:

1. `extent_of(mesh)` gives each mesh's `MeshExtent`: its footprint `[dx, dz]` and height at unit
   scale. `MeshExtent::from_asset` reads it from a `ScatterMeshAsset`'s LOD 0;
   `MeshExtent::from_bounds` takes glTF accessor bounds.
2. Classes that can never reach the threshold are not placed. This moves nothing else: cell
   streams are keyed by class name.
3. For each instance: `rise = height · scale · (1 − sink) · clamp(up.y, 0.2, 1)`. Only
   instances with `rise > threshold_m` are kept.
4. Radius `r = radius_per_scale · scale · sqrt(dx · dz)`, and top `= ground y + rise`. The
   sphere is centered at `(x, top − r, z)`, written in integer millimetres (rounded half away
   from zero). The list is sorted by `(x, z, y, r)` and de-duplicated.

| Setting | Default | Meaning |
|---|---|---|
| `threshold_m` | 0.7 | Rise above the ground that makes a collider |
| `radius_per_scale` | 0.4 | Radius per unit of `scale · sqrt(dx · dz)` |
| `clip_half_extent` | none | Keep `|x| ≤ hx` and `|z| ≤ hz` (the playable area) |
| `mirror_x` | false | Answer `−x` (a simulation frame that mirrors X) |

`SceneryCollider::sphere()` converts to metres as `mm / 1000` in `f64`, narrowed to `f32`. This
gives the same bits as parsing the three-decimal text of the value.

## The push-out query

`SphereBlockers::from_colliders(&colliders, SphereBlockerSettings::default())` sorts the
spheres by their bit patterns and buckets them on an XZ grid in a `BTreeMap`. Two hosts given
the same spheres in any order push in the same order, with the same bits. The query uses
plain `f32` arithmetic and `sqrt` only.

`push_out(x, z, feet_y, &body)` moves the body's XZ out of every blocking sphere:

- A sphere blocks while its top is more than `body.step` above the feet, and its bottom is
  below `feet_y + body.height`. Rocks under the step are walk-through.
- A sphere never supports the body. The body does not stand on a rock.
- The body ends at least `r + body.radius + body.skin` from each rock's axis. The move is capped
  at `max_push_m` per call (default 0.5), so a body that starts inside a rock walks out over a
  few calls. A body at the exact center is pushed toward +X.

| Setting | Default | Meaning |
|---|---|---|
| `bucket_m` | 16 | Bucket edge, metres |
| `bucket_margin_m` | 1 | Extra reach a sphere is bucketed with. At least the largest body radius plus skin |
| `max_push_m` | 0.5 | Largest move per call, metres |

`BlockedBody` has no default. The game supplies its body's `radius`, `height`, `step` and `skin`.

## `terrain.scatter_colliders`

Read-only, heavy. Reads a `.scatter.ron` under the project root and places it over the world's
`Terrain` map. It uses the terrain's holes and layer stack, and no density maps. Then it bakes
the colliders.

```json
{ "path": "maps/arena.scatter.ron", "anchor": [0.0, 0.0],
  "extents": { "rocks/boulder.glb": { "footprint": [1.6, 1.2], "height": 1.4 } },
  "default_extent": { "footprint": [1.0, 1.0], "height": 1.5 } }
```

| Param | Kind | Meaning |
|---|---|---|
| `path` (required) | string | A `.scatter.ron` under the project root |
| `entity` | integer | Terrain entity. Omit it for the lowest-entity-id terrain |
| `anchor` | `[x, z]` | World-anchored grid from this point. Omit it for the map-edge grid |
| `pin_altitude` | boolean | With `anchor`: pin the layer altitude range to this terrain's |
| `extents` | object | Mesh path → `{ footprint: [dx, dz], height }` |
| `default_extent` | object | The extent of every mesh not in `extents` |
| `threshold_m` | number | 0 to 1000, default 0.7 |
| `radius_per_scale` | number | 0 to 100, default 0.4 |
| `clip_half_extent` | `[hx, hz]` | Not negative |
| `mirror_x` | boolean | Default false |
| `limit` | integer | Colliders listed, 0 to 4096 (default 64) |

The result:

```json
→ { "path": "maps/arena.scatter.ron", "entity": 4294967296, "origin": "world",
  "anchor": [0.0, 0.0], "aligned": [32, 32], "altitude_range": null, "threshold_m": 0.7,
  "count": 41, "digest": "0x3f0c9d1e8a2b4c55",
  "colliders": [[-30512, 4120, 512, 401]], "truncated": true }
```

- `origin` is `map_edge` or `world`.
- `aligned` is the anchor's sample `[col, row]` in this terrain, or null. Null means rocks
  are not bit-stable across map sizes.
- `colliders` are `[x_mm, z_mm, y_mm, r_mm]`, sorted. `limit` shortens only this list.
- `count` and `digest` cover all colliders. The digest is FNV-1a 64 over the four `i64` values
  of each collider, little-endian.

To check that a backdrop moves no collider, call the method on both maps with the same `anchor`
and a `clip_half_extent` one sample inside the smaller map. The digests must match.

Errors:

| Code | When | Fix |
|---|---|---|
| `F0002` | A parameter has the wrong shape or range, or `pin_altitude` is given without `anchor` | Fix the parameter |
| `F0003` | No project root, or no `Terrain` (or the named entity has none) | Run in an app with a project and a terrain |
| `scatter_rules_read` | The path is not a `.scatter.ron` or cannot be read | Name a `.scatter.ron` under the project root |
| `scatter_rules_invalid` | The set does not parse | Fix the file; `terrain.scatter_rules` shows it |
| `scatter_colliders_extent` | A mesh has no extent and there is no `default_extent` | List it in `extents` or pass `default_extent` |
| `scatter_colliders_place` | A class names a density map, which the method does not load | Bake from the host with `bake_scenery_colliders` and the maps |
