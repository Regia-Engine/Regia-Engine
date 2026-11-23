# `nav-link-anchor` component, format version 1

An off-mesh link authored as a scene component (`regia_nav::NavLinkAnchor`, ADR-2420), saved inside `.level.ron` and
`.prefab.ron` under the type path `regia_nav::link_file::NavLinkAnchor`. The link takes off at the entity and lands
at `to`, a point in the entity's own space, so moving or turning the entity moves the link.
`regia_nav::NavAuthoredLinksPlugin` puts every anchor's link on the navmesh (after the links of the runtime
`.navlinks.ron` files), each fixed step, when they changed.

| Field | Type | Default | Notes |
|---|---|---|---|
| `to` | `Vec3` | `(0, 0, -2)` | The landing, in the entity's local space. |
| `bidirectional` | `bool` | `true` | `false`: from the entity to `to` only. |
| `area` | `u8` | `0` | Area / cost category. |
| `user_id` | `u64` | `0` | The game's id; anchors are ordered by it, then by entity. |
| `kind` | `Jump`, `Drop`, `Climb`, `Teleport`, `Custom(u16)` | `Jump` | Picks the gameplay animation. |
| `radius` | `f32` | `2.0` | Endpoint snap radius, `> 0`. |
| `cost_scale` | `f32` | `1.0` | Path cost multiplier, `> 0`. |
| `flags` | `u16` | `1` | Filter bits. |
| `enabled` | `bool` | `true` | The runtime switch. |
| `duration` | `f32` | `0.0` | Traversal seconds (0 = length over speed). |
| `format_version` | `u32` | `1` | A newer version is refused by the scene loader. |

An invalid anchor (a non-finite point, a radius or cost scale not above 0, a negative or non-finite duration) stays off
the navmesh and is counted in `NavAuthoredLinksStatus::invalid_anchors`.

Migration note: version 1 is the first; there are no migrators (`nav_link_anchor_migration_sources()` is empty). The
shape fingerprint is `nav_link_anchor_schema_fingerprint()`, registered with `cargo xtask check-formats` as
`nav-link-anchor`.
