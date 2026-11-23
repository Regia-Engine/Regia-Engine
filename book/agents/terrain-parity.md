# Terrain parity

A terrain has three agreements a screenshot does not show:

- the drawn LOD levels meet without cracks;
- the physics collider is the ground that is drawn;
- the heights being drawn are the heights that will be saved, which is what a reload and the
  game's simulation read.

`terrain.parity` checks all three in one read-only call.

## Method

`terrain.parity { entity?, eye? }`

- `entity`: the terrain (default: the first terrain in the world).
- `eye`: `[x, z]` in meters. The LOD selection is made and checked from this point. The default
  is the `TerrainView` camera, else the map center. The call checks the colour-pass selection
  (minimum level 0) from that eye. A shadow cascade draws its own mesh with a raised minimum
  level, and this call does not check that mesh.

The result:

| Field | Meaning |
|---|---|
| `ok` | `true` when `lod.crack_free` and `save.round_trips` are both true. |
| `lod.nodes` | Selected CDLOD nodes. |
| `lod.same_level_edges`, `lod.level_change_edges` | Shared edges checked. |
| `lod.max_level_jump` | Largest level difference across an edge. More than 1 is a crack. |
| `lod.max_gap_m`, `lod.worst_at` | Largest vertical gap between two drawn edges, and where it is (world `[x, z]`). |
| `lod.coarse_edge_morphs`, `lod.fine_edge_unmorphed` | Border vertices that break the crack-free rule (should be 0). |
| `lod.crack_free` | No gap above 1 mm and no level jump above 1. |
| `lod.morph_fraction` | `configured` and `effective`: the effective value is capped so the morph cannot open a seam. |
| `collision.nx`, `collision.nz`, `collision.scale` | The collider's layout: `nx` samples along Z, `nz` along X. |
| `collision.cut_samples` | Samples removed by terrain holes. |
| `collision.max_bilinear_delta_m` | The most the collider's triangles can differ from the simulation's bilinear height, anywhere on the map. |
| `save.quantum_m` | One 16-bit height step. |
| `save.max_drift_m`, `save.worst_sample` | Largest difference between the live and the saved height, and its `[column, row]`. |
| `save.clamped_samples` | Samples sculpted outside the map's height range. The save clamps them. |
| `save.round_trips` | Nothing is clamped and every sample is within half a step of its saved value. |

The call takes tens of milliseconds on a 4K map. Do not call it every frame.

## How to use it

- **After sculpting**, `save.clamped_samples > 0` means the brush went above the top or below the
  bottom of the heightfield's range. The saved map (and DO27's simulation) will be flat there.
  Lower the edit, or widen the map's height scale.
- **When you see a crack** in a screenshot, call `terrain.parity` with `eye` set to the camera
  position from that shot. `lod.worst_at` points at the worst edge. If `lod.crack_free` is true,
  the crack is not a LOD seam. Check the holes (`terrain.holes`) and the cliff relief instead.
- **Collision**: build the physics collider with `TerrainMap::collision_heightfield` (Rust). A
  collider filled from the map's own height array is mirrored about the X = Z diagonal.
  `collision.max_bilinear_delta_m` is the remaining difference between physics and a bilinear
  simulation such as DO27's sampler.

## Errors

Errors use the standard protocol codes ([protocol](protocol.md)):

- `F0002` (invalid params): `entity` is not an entity id, or `eye` is not `[x, z]` with finite
  numbers.
- `F0003` (not found): there is no terrain in the world, or the entity has no `Terrain`.
