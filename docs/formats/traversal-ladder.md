# `traversal-ladder` component, format version 1

A ladder on an entity (`regia_framework::traversal::Ladder`, ADR-2971), saved inside `.level.ron` and `.prefab.ron`. The rungs go from
the entity's origin up +Y; a climber stands on the entity's local +Z side and faces local -Z. The entity's position and yaw count; tilt
and scale do not. See `docs/specs/traversal.md` § 4.

| Field | Type | Default | Notes |
|---|---|---|---|
| `half_width` | `f32` | `0.35` | Rung half width, `>= 0`. |
| `height` | `f32` | `3.0` | Base to top, where the upper floor is, `> 0`. |
| `climb_speed` | `f32` | `1.5` | m/s, `> 0`. |
| `standoff` | `f32` | `0.4` | Capsule center to the ladder's plane, `>= 0`; a smaller value than the capsule's radius plus its skin is raised to that. |
| `exit_forward` | `f32` | `0.6` | How far past the plane the top exit stands, `> 0`. |
| `user_id` | `u64` | `0` | The game's id, also the nav link's `user_id`; 0 = unnamed. |
| `nav_link` | `bool` | `true` | Publish a nav link (`TraversalNavPlugin`). |
| `enabled` | `bool` | `true` | The runtime switch. |
| `format_version` | `u32` | `1` | A newer version is refused by the scene loader. |

An invalid ladder (`Ladder::validate`, code `ladder_bad_params`: a non-finite or out-of-range number) attaches nobody and publishes no
link.

Migration note: version 1 is the first; there are no migrators (`traversal_ladder_migration_sources()` is empty). The shape fingerprint
is `traversal_ladder_schema_fingerprint()`, registered with `cargo xtask check-formats` as `traversal-ladder`.
