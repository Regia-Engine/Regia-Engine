# HLOD scatter voxel proxies

A forest placed as terrain scatter reaches the horizon of a streamed world through its HLOD proxies (ADR-3253, spec 78
§§ 11.4 to 11.7). The stream cook places the scatter exactly as the runtime does, voxelizes the trees under each HLOD
node into a voxel proxy, and the runtime draws that proxy where the HLOD cut draws the node, with no scatter instance
data resident. Far proxies can also cast shadows into the far sun cascades.

## Cooking a level with scatter

Scatter is not part of a level file: a game spawns its terrain and scatter itself. The host hands them to the stream
cook the way it hands its terrain to the nav cook:

<!-- ignore-reason: illustration of host-side cook wiring, not a compiling example (StreamAssets fields abbreviated) -->
```rust,ignore
use regia_asset_pipeline::stream_cook::{StreamScatter, WithScatter, cook_streamed_level};

let mut forest = StreamScatter::from_cooked(&cooked_scatter_ron, terrain_map)?;
forest.origin = [512.0, 0.0, 512.0]; // where the terrain's center sits in the world
let assets = WithScatter { assets: &cache_assets, scatter: vec![forest], resolution: 32 };
cook_streamed_level(level, &scene, &registry, &assets, Some(&cache_dir), "maps/forest.level.ron")?;
```

- Every streaming cell with trees becomes a leaf of the HLOD tree, even with no entity in it (no cell blob).
- Each node with trees under it gets a voxel proxy (`hlod/<node>/voxels`, the `voxel-far` format) at `resolution`
  voxels along the node's longest axis (default 32), so parents have coarser voxels.
- `StreamScatter::coverage` (an alpha texture and cutoff) voxelizes leaf cards through their alpha test, so cut-away
  card area does not fill voxels.
- The cook writes the **HLOD voxel table** (`.wvoxels`) beside the world index. A level without scatter writes none,
  and its cooked files are byte for byte what they were.
- The report (`StreamReport`) counts `voxel_proxies`, `voxel_bytes` and `scatter_instances`.

Cook warnings: `stream_hlod_scatter_mesh` (a scatter mesh with no cooked triangles; its instances are skipped),
`stream_hlod_scatter_failed` (a set that does not place, such as a missing density map),
`stream_hlod_voxel_failed` (a node whose triangles do not voxelize).

Tuning: give the scatter classes a `fade_end` at least as far as the leaf switch distance (about 0.9 of
`load_radius_m`), or trees between the two draw in neither.

## At run time

`open_streamed_level` reads the table; a host that opens from its own bytes calls
`WorldStreaming::set_hlod_voxels(level, table)`. Each kept voxel node gets an `HlodProxyVoxels` entity beside its
`HlodProxyMesh` entity; the renderer draws the finest voxel level as boxes colored per voxel. A node draws once both
its mesh and its voxels are loaded (`HlodProxyResidency`, `HlodVoxelResidency`).

Near the viewer, a scatter-only cell draws `Full`: nothing from HLOD, the scatter runtime's own trees. Far away its
voxel proxy draws.

## Proxy shadows

Off by default. Turn them on with the resource:

<!-- ignore-reason: settings construction abbreviated; the page shows the shape, the schema text holds the fields -->
```rust,ignore
app.insert_resource(regia_render::HlodProxyShadows { enabled: true, ..Default::default() });
```

The active render tier picks how many sun cascades a proxy may draw into: Desktop High, Desktop Ultra and Cinematic 2,
Desktop Low and the Deck 1, the mobile tiers 0. A proxy casts as a bounded caster (`ShadowCast::Local`, its radius as
its shadow's reach), so it only reaches the cascades and pages at its distance: the near cascades, where full-detail
cells cast their own shadows, never see a far proxy.

## Proxy budget

`hlod_budget_bytes` (spec 78 § 11.4) never evicts a proxy that is on screen or that the cut asks for; it bounds the
prefetched parents and fallbacks. 0 means no prefetch: a switch loads its proxy on demand and, for those frames, draws
the nearest loaded ancestor. The cell you stand in is never covered by a root proxy for want of budget.

## `world.hlod_proxy`

Reads the cooked world index and, beside it, the voxel table. Per node:

```json
{"jsonrpc":"2.0","id":1,"method":"world.hlod_proxy","params":{"level":"<guid>","node":3}}
{"jsonrpc":"2.0","id":1,"result":{"nodes":[{ "node": 3, "level": 0, "cell": "2_2", "mesh": null, "color": null, "proxy_bytes": 41230, "atlas": null,
  "kinds": ["voxel"],
  "voxel": { "proxy": "…", "instances": 52, "voxels": 1830, "voxel_size_m": 3.4, "bytes": 40112 },
  "casts_shadow": true }]}}
```

Top level: `voxel_proxies` (nodes with a voxel proxy), `shadow_cascades` (cascades a proxy casts into here, 0 when
off) and `shadows: { enabled, tier, cascades }` from the live `HlodProxyShadows` and render profile.

`kinds` lists `mesh` (a merged, simplified mesh proxy), `atlas` (a baked atlas on it, ADR-2483) and `voxel` (the voxel
proxy of the scatter). `casts_shadow` is true when proxy shadows are on for this tier and the node draws anything.

### Error codes

| Code | Meaning | What to do |
|---|---|---|
| `hlod_index_unreadable` | The cooked world index is missing or is not one. | Cook the level; pass `level` or `path`. |
| `hlod_no_proxies` | The level cooked without HLOD. | Give `LevelStreaming` an `hlod` block and cook again. |
| `hlod_node_unknown` | `node` is past the tree. | Call without `node` to list the nodes. |
| `hlod_voxels_unreadable` | The voxel table beside the index does not read. | Cook again; a table from a newer build needs that build. |

## Checking a forest

1. Cook, then `world.hlod_proxy {"level": "<guid>"}`: every scatter cell's leaf has `voxel` with `instances > 0`.
2. Fly the camera far out (`seeing.screenshot`): the forest stays on the horizon past the scatter and impostor range.
3. With `HlodProxyShadows` on, a far hill of trees shadows the ground beyond it; `casts_shadow` is true.

Bounds measured by the tests (lavapipe, fixed camera): a cell's voxel proxy covers every pixel its trees cover, no
proxy pixel lies more than a voxel diagonal outside them, and the covered area is at most the trees' grown by one
voxel edge (1.72 times the trees' for sparse crowns at 450 m: the proxy reads fuller than the trees far away). The
proxy shadow bound (within 4 screen pixels of the full-detail caster's shadow) has a test that has not run yet.
