# Delivery contract

`assets.check_contract` judges one measured asset against a `.contract.ron` file (format `delivery-contract`, version 1, ADR-8401, ADR-8450). The file is the settings for one asset class: unit scale, up and forward axes, the ground origin, the LOD chain with triangle budgets, joint influences, required nodes, sockets and clips, material and texture caps, a footprint radius and a socket opening. Class names and numeric caps are settings in the file.

`regia assets contract-check --contract FILE --report DIR --asset FILE` writes `report.md` and `report.json` in `DIR`, one row per failure. The row names the asset, the node, socket or clip, and what the contract expected against what was found. Warnings are omitted from that report. The command exits 0 when every finding is a warning and exits non-zero when any finding is an error. A file that cannot be read fails before a report is written.

Read-only. Permission: `read`.

## Call

`contract_ron` (string, required) is the `.contract.ron` text, at most 1,048,576 bytes. `observation` (object, required) is what was measured:

| Field | Required | Meaning |
|---|---|---|
| `node_scales` | yes | Each node's scale as three numbers. A component farther than `identity_tolerance` from 1 is `unit_scale`. An empty list skips the scale check. |
| `authored_up` | yes | The up axis the asset was authored with (`pos_x`, `neg_x`, `pos_y`, `neg_y`, `pos_z`, `neg_z`). |
| `authored_forward` | no | The forward axis, when the asset names one. Omit it when the asset does not. |
| `bounds_min`, `bounds_max` | yes | Axis-aligned bounds in source X, Y, Z. Send them even when `bounds_present` is false; they must still be finite. |
| `bounds_present` | yes | False when there are no mesh bounds. The ground check is then a warning. |
| `coarse_lod` | yes | A coarse LOD may sit higher above the ground offset. |
| `lod_triangles` | yes | Triangle counts keyed by the contract's LOD names. `{}` is allowed. |
| `asset_name` | no | File name copied onto each finding. Default empty. |
| `nodes`, `sockets`, `clips` | no | Names the required-name checks count. Default empty. |
| `material_count`, `material_names` | no | How many materials, and their names. Default 0 and empty. |
| `textures` | no | Texture rows. A missing edge is `texture_unreadable`. Default empty. |
| `skins` | no | One row per skinned primitive. Default empty. |
| `footprint_radius_m` | no | LOD0 horizontal radius in metres. Omit it to skip the footprint check. |

A missing required field is `observation_invalid`, and the message names the field. The optional fields default as shown, so a call that only measured scales and axes still sends the seven required fields (with empty `node_scales` or `lod_triangles` where nothing was measured) and gets judged on those.

The rows inside `sockets`, `textures` and `skins` have required keys too:

| Row | Required keys | Optional keys |
|---|---|---|
| `sockets` | `name` | `width_m`, `height_m` (metres; absent is an unknown size), `source` |
| `textures` | `name` | `width`, `height` (pixels; absent is an unreadable edge) |
| `skins` | `mesh`, `primitive`, `influences` | `second_set`, `weight_sum`, `weight_out_of_range`, `joint_out_of_range`, `unpaired` (default false or absent) |

A skin row's `mesh` is the mesh name, `primitive` the primitive index, and `influences` the most non-zero influences on one vertex. `second_set` is a `JOINTS_1` / `WEIGHTS_1` pair, `weight_sum` the sum of the first vertex that does not sum to 1, and `unpaired` joints without weights (or weights without joints). A row without one of its required keys is `observation_invalid` naming the key.

A successful result is `{ passed, class_name, error_count, warning_count, findings }`. Each finding is `{ code, severity, message, asset, subject_kind, subject, expected, found }`. `severity` is `error` or `warning`. `passed` is true when there is no error. Warnings do not fail the asset. `subject_kind` is `node`, `socket`, `clip`, `material`, `texture`, `skin` or `footprint` when the row is about one of those, and empty for an axis or LOD row.

`wrong_up_axis` is a finding: the authored up axis is not the contract's up axis. It is not a transport error. `wrong_forward_axis` is the same kind of finding for a forward axis the asset did name. A missing forward axis is not that finding. `nose_axis` is only a warning that no geometry extends along the contract forward axis. It does not establish the up axis.

Other finding codes: `unit_scale`, `origin_ground`, `origin_center`, `lod_presence`, `lod_count`, `lod0_budget`, `lod_chain`, `lod_ratio` (warning), `lod_cap`. When the budget LOD is absent, the budget, the decreasing chain, the ratios and the caps are skipped. Presence and the LOD set are still reported.

Kit findings, from the same call:

| Code | When |
|---|---|
| `joint_influences` | A vertex has more influences than `max_joint_influences` (default 4). A second set (`JOINTS_1` / `WEIGHTS_1`) counts as at least 5. Joints without weights is the same code. |
| `joint_weights` | A weight is outside 0 to 1, or a vertex's weights do not sum to 1 within 0.01. The message includes the sum. |
| `joint_index` | A joint index is past the skin's joint list. |
| `missing_node` | A `required_nodes` pattern matched fewer names than its count. The subject is the pattern. |
| `missing_socket` | Same, for `required_sockets`. |
| `missing_clip` | Same, for `required_clips`. |
| `material_count` | More materials than `max_materials` (default 4) is an error. More than `material_warn_above` (default 3) and still within the cap is a warning. |
| `texture_edge` | The longer edge is past `texture_max_edge` (default 2048). |
| `texture_unreadable` | The edge could not be read. A warning. |
| `footprint_radius` | The LOD0 horizontal radius is farther from `footprint.radius_m` than `footprint.tolerance` (default 0.10). The message names the radius rounded up to the next tenth of a metre. A measured 0 fails a non-zero radius. |
| `socket_opening` | A socket that matches `socket_clearance` is narrower or shorter than the minimum, or its size is unknown (the check fails closed). A pattern with no socket is `missing_socket`, not this code. |

`required_nodes`, `required_sockets` and `required_clips` are `{ pattern, count }`. `pattern` is a name or a glob whose only metacharacter is `*`. `count` defaults to 1. Empty lists skip those checks. `socket_clearance` is `{ pattern, min_width_m, min_height_m }` and applies only to sockets that are present. `footprint` is `{ radius_m, tolerance }` and is skipped when omitted. The observation's own defaults are in the tables under Call.

## Errors

| Code | When |
|---|---|
| `bad_contract` | `contract_ron` is missing or empty. |
| `contract_parse` | The text is not valid RON for this format, or has a field the format does not define (named in the message), including one inside a `required_*` row, `footprint` or a `socket_clearance` row. |
| `contract_version_newer` | `format_version` is newer than 1. |
| `contract_invalid` | The settings disagree: parallel axes, `format_version` 0, a bad class or LOD name, a LOD used by the chain, a ratio or a cap that the file does not list, `max_joint_influences` outside 1 to 8, `texture_max_edge` of 0, a bad name pattern or a count of 0, or a negative or non-finite footprint or clearance size. |
| `observation_invalid` | `observation` is missing, the wrong shape, non-finite, or has inverted bounds. |

`F0002` still covers an unknown parameter or a value of the wrong JSON type. That code is universal and is not repeated on this method.
