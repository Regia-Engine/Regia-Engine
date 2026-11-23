# `traversal-settings` component, format version 1

A character's traversal tuning (`regia_framework::traversal::TraversalSettings`, ADR-2971), saved inside `.level.ron` and
`.prefab.ron`. Its presence gives the character swimming, ladders, mantles and vaults (`docs/specs/traversal.md`). Every field has a
default, so a file names only what it changes. Distances are meters, speeds m/s, rates 1/s, angles radians.

| Field | Type | Default | Notes |
|---|---|---|---|
| `swim` | `SwimSettings` | see below | |
| `ladder` | `LadderSettings` | see below | |
| `mantle` | `MantleSettings` | see below | |
| `montage_slot` | `String` | `"traversal"` | Named in `TraversalEvent::Started`; not empty. |
| `format_version` | `u32` | `1` | A newer version is refused by the scene loader. |

`SwimSettings`: `enabled` true, `chest_offset` 0.5, `speed` 3.0, `sprint_multiplier` 1.5, `dive_speed` 2.0, `rise_speed` 1.5,
`float_rate` 2.0, `drag` 6.0 (`> 0`), `max_dive_depth` 6.0, `surface_band` 0.25, `exit_margin` 0.1, `leave_margin` 0.6,
`jump_velocity` 4.0.

`LadderSettings`: `enabled` true, `attach_margin` 0.15, `attach_reach` 0.5, `intent` 0.5 (`0..=1`), `enter_time` 0.2, `exit_speed` 2.0
(`> 0`), `jump_off_speed` 3.0, `jump_off_up` 3.0, `cooldown` 0.5.

`MantleSettings`: `enabled` true, `auto_in_air` true, `min_height` 0.5, `max_height` 2.0 (at least `min_height`), `reach` 0.6 (`> 0`),
`edge_inset` 0.03 (`> 0`), `max_slope` 0.6, `min_depth` 0.6 (`> 0`), `stand_inset` 0.05, `clearance` 0.05 (at least 0.04), `speed` 3.0
(`> 0`), `vault_enabled` true, `vault_max_height` 1.2, `vault_max_thickness` 0.5, `vault_max_drop` 1.5, `vault_speed` 4.0 (`> 0`).

All numbers must be finite and at least 0 unless noted (`chest_offset` may be negative). At run time a bad value stops the moves and
sets `last_refusal` to `traversal_bad_settings`; `TraversalSettings::validate` names the field.

Migration note: version 1 is the first; there are no migrators (`traversal_settings_migration_sources()` is empty). The shape
fingerprint is `traversal_settings_schema_fingerprint()`, registered with `cargo xtask check-formats` as `traversal-settings`.
