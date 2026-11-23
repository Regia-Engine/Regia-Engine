# `.contract.ron` — per-class delivery contract (format `delivery-contract`, v1)

Status: v1, must merge before the v0.6 format freeze (ADR-8401, `docs/specs/12-assets.md` § 4a). Reader:
`regia_asset::delivery_contract::DeliveryContract`.

One file per asset class. It stores the settings a delivery check uses: how large one unit is, which axes are up and forward, where the origin sits, the LOD chain with triangle budgets, and the kit limits (joints, required names, materials, textures, footprint, socket clearance). Class names and numeric caps are settings. The kit fields were added to version 1 before that version shipped in a tag (ADR-8450). Older files still load: every new field has a default, and there is no migrator.

```ron
(
    format_version: 1,
    class_name: "prop",
    scale: (meters_per_unit: 1.0, identity_tolerance: 0.001),
    axes: (up: pos_y, forward: pos_z),
    origin: (
        ground_offset_m: 0.0,
        ground_tolerance_m: 0.05,
        coarse_floor_rise_m: 0.25,
        center_fraction: 0.1,
    ),
    lod: (
        required: ["lod0"],
        optional: [],
        budget_lod: "lod0",
        lod0_min_triangles: 1,
        lod0_max_triangles: 100000,
        chain: ["lod0"],
        ratios: [],
        hard_caps: [],
        require_decreasing: true,
    ),
)
```

| Field | Type | Rule |
|---|---|---|
| `format_version` | `u32` | `1`. `0` is refused. A newer version is refused before the body is trusted (`contract_version_newer`). |
| `class_name` | string | 1 to 64 bytes: ASCII letters, digits, `_` or `-`. A label, not a built-in class table. |
| `scale.meters_per_unit` | `f64` | Finite and greater than 0. The declared size of one unit. Node scale is still judged against 1. |
| `scale.identity_tolerance` | `f64` | Finite and at least 0. A node scale component fails `unit_scale` when it is farther than this from 1. |
| `axes.up`, `axes.forward` | axis | `pos_x`, `neg_x`, `pos_y`, `neg_y`, `pos_z`, `neg_z`. They must not be parallel. |
| `origin.ground_offset_m` | `f64` | Finite. Expected minimum along the up axis, in metres. |
| `origin.ground_tolerance_m` | `f64` | Finite and at least 0. How far below the offset the minimum may sit, and how far above it a fine mesh may sit. |
| `origin.coarse_floor_rise_m` | `f64` | Finite and at least 0. How far above the offset a coarse LOD may sit. |
| `origin.center_fraction` | `f64` | Finite and greater than 0. Each horizontal center must lie within this fraction of the largest span. |
| `lod.required` | list of strings | LODs that must be present. Names are short tokens, unique across required and optional. |
| `lod.optional` | list of strings | Extra LODs that may be present. Default `[]`. |
| `lod.budget_lod` | string | The LOD whose triangle count must lie in `lod0_min_triangles..=lod0_max_triangles`. |
| `lod.chain` | list of strings | Order for the strictly decreasing check when `require_decreasing` is true. Missing counts are skipped. |
| `lod.ratios` | list | `{ name, of, nominal }`. A warning (`lod_ratio`) fires outside half to double `nominal`. Empty skips the check. |
| `lod.hard_caps` | list | `{ name, max_triangles_exclusive }`. The count must be strictly below the cap (`lod_cap`). |
| `lod.require_decreasing` | bool | When true, each present chain LOD must have fewer triangles than the previous present one (`lod_chain`). |
| `max_joint_influences` | `u32` | Default 4. Range 1 to 8. A second influence set counts as at least 5 (`joint_influences`). |
| `required_nodes` | list of `{ pattern, count }` | Names that must be present (`missing_node`). `count` defaults to 1. `pattern` is 1 to 64 bytes of ASCII letters, digits, `_`, `-` or `*`. |
| `required_sockets` | list of `{ pattern, count }` | Same, for sockets (`missing_socket`). |
| `required_clips` | list of `{ pattern, count }` | Same, for clips (`missing_clip`). |
| `max_materials` | `u32` | Default 4. A higher count is `material_count` and fails. |
| `material_warn_above` | `u32` | Default 3. A count above this and still within `max_materials` is a warning. `0` disables the warning. |
| `texture_max_edge` | `u32` | Default 2048. The longer edge must be at most this many pixels (`texture_edge`). At least 1. |
| `footprint` | `{ radius_m, tolerance }` or absent | LOD0 horizontal radius. `tolerance` defaults to 0.10. Absent skips the check (`footprint_radius`). |
| `socket_clearance` | list of `{ pattern, min_width_m, min_height_m }` | Each present socket matching `pattern` must be at least this wide and tall (`socket_opening`). An unknown size fails closed. |

`*` in a pattern matches any run of characters, including an empty one. No other metacharacter is special. Empty lists skip those checks.

When `budget_lod` is absent from the measured counts, the budget, the chain, the ratios and the caps are skipped. Presence and the allowed LOD set are still reported.

The observation is not stored in this file. `assets.check_contract` takes it beside the text. `authored_up` is explicit. Bounds along the forward axis are not evidence of the up axis (`nose_axis` is a warning only).

Unknown fields are refused (`contract_parse`, naming the field; ADR-8553), in the file and in every nested
section: `optionall: ["lod1"]` is an error, never a silent default for `optional`. `format_version` is read before
the full parse, so a newer file is `contract_version_newer` even when it also has fields this build does not know.
Forward compatibility comes from a version bump plus a migrator, not from ignoring keys. Canonical text: `DeliveryContract::to_ron_string` (pretty, trailing newline).

The kit rows refuse unknown fields too: `extra: 1` inside a `required_nodes`, `required_sockets` or
`required_clips` row, inside `footprint`, or inside a `socket_clearance` row is `contract_parse` naming `extra`.

## Versions and migration

v1 is the first version: no migrator (`delivery_contract_migration_sources()` is empty). The kit fields (ADR-8450) are folded into v1 before any tag: a v1 file without them still loads with the defaults above. The frozen corpus has two v1 fixtures: `delivery-contract/v1.ron` (no kit fields) and `delivery-contract/v1-kit.ron` (every kit field set). A later shape change bumps `DELIVERY_CONTRACT_FORMAT_VERSION`, adds a migration step and a `regia upgrade-project` migrator (the `delivery-contract` row, suffix `.contract.ron`, version read from `format_version`). `cargo xtask check-formats` hashes the serde field list (`delivery_contract_schema_fingerprint`).

This format is part of the one v0.6 format bump and must merge before the v0.6 freeze.
