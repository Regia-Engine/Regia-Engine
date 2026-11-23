# `traversal-jump-point` component, format version 1

An authored jump on an entity (`regia_framework::traversal::TraversalJumpPoint`, ADR-2971), saved inside `.level.ron` and `.prefab.ron`.
It takes off at the entity and lands at `to`. `TraversalNavPlugin` publishes it as a nav link of kind `Jump`, so AI reaches it through
the navmesh. See `docs/specs/traversal.md` § 7.

| Field | Type | Default | Notes |
|---|---|---|---|
| `to` | `Vec3` | `(0, 0, -3)` | The landing on the floor, in the entity's local space. |
| `bidirectional` | `bool` | `false` | Usable both ways. |
| `user_id` | `u64` | `0` | The game's id, also the nav link's `user_id`; 0 = unnamed. |
| `arc_height` | `f32` | `1.0` | How far over the higher end the path passes, `>= 0`. |
| `enabled` | `bool` | `true` | The runtime switch: a disabled point publishes no link. |
| `format_version` | `u32` | `1` | A newer version is refused by the scene loader. |

An invalid point (`TraversalJumpPoint::validate`, code `jump_point_bad_params`) publishes no link.

Migration note: version 1 is the first; there are no migrators (`traversal_jump_point_migration_sources()` is empty). The shape
fingerprint is `traversal_jump_point_schema_fingerprint()`, registered with `cargo xtask check-formats` as `traversal-jump-point`.
