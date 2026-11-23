# `.navlinks.ron`, format version 1

Authored off-mesh links for a navmesh: jumps, drops, climbs and teleports
(`regia_nav::link_file::NavLinkSet`, ADR-1821, [`docs/specs/92-navmesh-offmesh.md`](../specs/92-navmesh-offmesh.md)
§ 3.5). Game code loads it with `NavLinkSet::from_ron_str` and hands the links to `NavMesh::set_off_mesh_links` or
`NavTileCache::set_off_mesh_links`; agents send the text to `nav.set_links` (`book/agents/navigation-links.md`).

```ron
(
    format_version: 1,
    links: [
        (from: (0.0, 0.0, 0.0), to: (0.0, -2.0, 3.0), bidirectional: false, kind: Drop, user_id: 7),
        (from: (4.0, 0.0, 0.0), to: (4.0, 3.0, 0.0), kind: Climb, flags: 2, duration: 1.5),
        (from: (8.0, 0.0, 0.0), to: (40.0, 0.0, 0.0), kind: Custom(12), cost_scale: 4.0),
    ],
)
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `format_version` | `u32` | required | 1. 0 and any version above 1 are the error `nav_link_version`. |
| `links` | list | required | At most 65536 links. |
| `from`, `to` | `(f32, f32, f32)` | required | World-space ends, finite. |
| `bidirectional` | `bool` | `true` | `false`: usable from `from` to `to` only. |
| `kind` | `Jump`, `Drop`, `Climb`, `Teleport`, `Custom(u16)` | `Jump` | Picks the gameplay animation; a `Teleport` with no duration takes one crowd step. |
| `user_id` | `u64` | `0` | The game's id; enable switches act on every link with the id. |
| `radius` | `f32` | `2.0` | Endpoint snap radius in metres, `> 0`. |
| `cost_scale` | `f32` | `1.0` | Path cost = 3D length times this, `> 0`, then times the area cost of the link's `area`. |
| `flags` | `u16` | `1` | Filter bits checked against an agent's include and exclude masks. |
| `enabled` | `bool` | `true` | The runtime switch. |
| `duration` | `f32` | `0.0` | Crowd traversal seconds, `>= 0`; 0 = length over the agent's top speed. |
| `area` | `u8` | `0` | Area category: the filter's cost for this area multiplies the link cost, and an area the filter forbids blocks the link. |

Migration: v1 is the first version and has no migrators (`nav_links_migration_sources()` is empty). Links built in
code before v1 need nothing: the new `OffMeshLink` fields have serde defaults. `cargo xtask check-formats` hashes
`nav_links_schema_fingerprint()` under the name `navlinks`; `regia upgrade-project` lists `.navlinks.ron` files as
unmigrated v1.
