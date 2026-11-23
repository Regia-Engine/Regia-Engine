# `<map>.detnav` (deterministic navmesh, `NVM2`, version 2)

Owner: `regia_nav::det::navmesh` (`NavMesh::to_bytes` / `NavMesh::from_bytes`, ADR-8174). The cooked navigation mesh a
lockstep authority arms for a map: written once at cook time by `NavMesh::bake` + `to_bytes`, loaded by the authority
with `from_bytes`, never baked at match time. The layout is Direct Order 27's cooked navmesh copied whole (tag
regia-absorb-2), so its files load unchanged; the format enters the engine at its version 2 (no Regia version 1 file
exists, `ENTERED_AT` in the `cargo xtask check-formats` tests).

All integers little-endian. Every `f32` is written as its canonical bits: `-0.0` is written as `+0.0`, every NaN as
`0x7fc0_0000`.

| Field | Type | Meaning |
|---|---|---|
| `magic` | `[u8; 4]` | `NVM2` (`NAVMESH_MAGIC`) |
| `version` | `u32` | 2 (`NAVMESH_VERSION`). Any other value is refused |
| `bake_id` | `u64` | FNV-1a 64 of the version and every bake input (`NavMesh::bake_id`) |
| `extent_m` | `f32` | Full map width (m) |
| `arena` | `u8` | 1 for an indoor arena bake, else 0 |
| `poly_count` | `u32` | 1..=1,000,000 |
| per polygon: `kind` | `u8` | 0 ground, 1 tunnel, 2 room, 3 stair, 4 door; anything else is refused |
| `level` | `i32` | Storey index (informational) |
| `ceil` | `f32` | Ceiling (world Y); `f32::MAX` on open ground; must be finite |
| `width` | `f32` | Navigable width (m); `f32::MAX` on open ground; must be finite |
| `vert_count` | `u8` | 3..=16 |
| per vertex: `x`, `z`, `floor` | `f32` x 3 | World XZ (counter-clockwise) and the floor height there; must be finite |
| `link_count` | `u32` | At most `poly_count` |
| per link: `nbr` | `u32` | The neighbor polygon, `< poly_count` |
| `a`, `b` | `[f32; 2]` x 2 | Portal endpoints (world XZ) |
| `width`, `step`, `clearance` | `f32` x 3 | Portal width, floor step toward `nbr`, headroom; must be finite |

The file ends after the last polygon: trailing bytes are refused. A file whose vertices span a spatial index of more than
`MAX_LOADED_BUCKETS` (2^22) buckets of 8 m, or as many bucket entries, is refused (a Regia addition: every mesh within
about a 16 km square loads as before). Loading sorts and deduplicates each polygon's links by neighbor, so a loaded mesh
writes exactly the bytes a bake of the same parts writes.

**Versioning.** Registered as `nav-det-navmesh` in `cargo xtask check-formats` (hashed from `NAVMESH_LAYOUT`) and in
`regia upgrade-project` (`.detnav`, the magic and the `u32` after it). The fuzz target is `nav.det_navmesh`. A change to
the bake rules or the layout bumps `NAVMESH_VERSION` and ships a migrator; the golden
`crates/regia_nav_det/goldens/navmesh_bake.detgold` pins the bytes of three bakes on every architecture.
