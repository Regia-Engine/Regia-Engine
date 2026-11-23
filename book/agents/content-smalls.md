# Contact sheet, install tier and grounded placement

Three host tools for reviewing frames, stating an install's residency cap, and checking
authored points against a heightfield (ADR-8036).

## `regia contact-sheet`

Build a labeled JPEG grid and a luma CSV. One of `--frames`, `--dirs` or `--pairs` is
required. The CSV is written beside `--out` with a `.csv` suffix.

```text
regia contact-sheet --frames a.png b.png --out sheet.jpg
regia contact-sheet --dirs frames/ --out sheet.jpg
regia contact-sheet --pairs before/ after/ --out sheet.jpg
```

| Flag | Meaning |
|---|---|
| `--frames FILE…` | Image files, in sheet order. |
| `--dirs DIR…` | Directories. Each directory is sorted by file name. Non-images are skipped. |
| `--pairs BEFORE AFTER` | Pair files that share a name. Columns are widened to an even count. |
| `--out FILE` | JPEG path. Required. |
| `--cols N` | Columns (default 4). A pair sheet uses `max(1, N / 2) * 2`. |
| `--max-width N` | Scale the sheet down with nearest-neighbor when it is wider than this (default 3840). The sheet is never scaled up. |
| `--quality N` | JPEG quality, 1 to 100 (default 90). |
| `--label TEXT` | Repeatable. One label marks every cell. With pairs, one label per pair is shared by both sides. Otherwise pass one label per cell. |
| `--roi x,y,w,h` | Metric window as fractions of the image. The drawn cell stays the whole image. |
| `--no-metrics` | Leave luma out of the caption and the CSV. |

Each cell keeps its aspect, sits centered on black, and is never enlarged to fill the cell.
The label strip is 28 pixels. The gap is 4 pixels. Luma is BT.601 (`0.299 R + 0.587 G + 0.114 B`)
on the 8-bit image. `p99` is the value at index `ceil(0.99 * count) - 1` of the sorted lumas.
`clip` is the fraction of pixels whose luma is at least 250. PNG and JPEG are decoded with the
workspace `image` crate. PPM (`P3`, `P6`) and PGM (`P2`, `P5`) at maxval 255 are read here.

Exit codes:

| Exit | Code | When |
|---|---|---|
| 0 | | The JPEG and the CSV were written. |
| 2 | `contact_sheet_usage` | The arguments cannot build a sheet (no source, `cols` below 1, quality outside 1..=100, a bad `--roi`, the wrong number of labels). |
| 2 | `contact_sheet_io` | A file cannot be read or written. A missing image's message contains `cannot read image`. |
| 2 | `contact_sheet_format` | The bytes are not a PNG, JPEG, PPM or PGM this command reads. The message contains `unsupported image format`. |

## `content.check_placement`

Read permission. Empty params check nothing and return `issues: []` and `checked: 0`.

Parameters, all optional:

| Name | Kind | Default |
|---|---|---|
| `objects` | array | none. Each item is an object with string `id`, number `x`, number `z`, and optional number `ground_y` and boolean `spawn_pad`. |
| `ground_y` | number | `0`. Height of the flat field, metres. |
| `half_extent` | number | `1000000`. Half the map size on x and z, metres. |
| `ground_tolerance_m` | number | `0.05`. A stamped height may differ from the field by this many metres. |
| `edge_tolerance_m` | number | `0.05`. A point may sit this many metres past the half-extent. |
| `spawn_ring_m` | number | `9`. Spawn-ring radius, metres. `0` skips the ring. |
| `spawn_rise_over_run` | number | `0.2679492` (tan of 15 degrees at f32 precision, stored so the check does not call a tangent). Maximum `|height − centre| / radius` on the ring. |

```json
content.check_placement { "objects": [{ "id": "crate", "x": 0, "z": 0, "ground_y": 1.0 }], "ground_y": 0, "half_extent": 100 } → { "issues": [{ "code": "on_heightfield", "id": "crate", "detail": "stamped y is off the heightfield" }], "checked": 1 }
```

Issue `code` values:

| Code | When |
|---|---|
| `on_heightfield` | `ground_y` is set and differs from the field by more than `ground_tolerance_m`, or either height is not finite. The detail contains `stamped y`. A point with no `ground_y` is not a float failure. |
| `inside_map` | `x` or `z` is not finite, or the point sits outside the half-extent plus `edge_tolerance_m`. |
| `unique_id` | A later point reuses an `id`. |
| `spawn_flat` | `spawn_pad` is true, the ring radius is positive, and the ring rises at least `radius * spawn_rise_over_run` from its centre. |

A bad item (not an object, or missing `id`, `x` or `z`, or a non-finite number) is
`placement_bad_input`. A parameter of the wrong JSON type is `F0002`.

## `content.install_tier`

Read permission. `tier` is optional. Absent, `{}` or `null` returns the built-in `low` tier.
The ids are `minimal`, `low`, `high` and `ultra`.

| `tier` | `texture_max_px` | `texture_budget_mib` | `terrain_layer_max_px` | `preset_max` |
|---|---|---|---|---|
| `minimal` | 512 | 768 | 512 | `Low` |
| `low` | 1024 | 1024 | 1024 | `Medium` |
| `high` | 2048 | 2048 | 2048 | `High` |
| `ultra` | 0 (uncapped) | 4096 | null (no mip skip) | `Ultra` |

`preset_max` names an existing graphics preset (`Low`, `Medium`, `High`, `Ultra`, `Clean`,
`Deck`). This call does not change that preset.

```json
content.install_tier { "tier": "low" } → { "tier": "low", "version": 1, "texture_max_px": 1024, "texture_budget_mib": 1024, "terrain_layer_max_px": 1024, "preset_max": "Medium", "uncapped": false, "known": ["minimal", "low", "high", "ultra"] }
```

An id that is not a built-in is `install_tier_unknown`. The hint is `use minimal, low, high or ultra`.
A `tier` that is not a string is `F0002`.

`regia package --server` writes the `low` tier as `tier.ron` (version 1, not a frozen content
format) and two systemd units under `deploy/`, each starting a binary the package ships (KI-NT-043,
ADR-8891):

| File | Role |
|---|---|
| `deploy/regia-server.service` | The packaged dedicated-server binary. `ExecStart` is the install path. `REGIA_SERVER_PORT` defaults to 7777. |
| `deploy/regia-hub.service` | The packaged `regia-lobby-hub`: `ExecStart=/opt/regia/regia-lobby-hub --config /opt/regia/hub.toml --key-file /opt/regia/hub.key`. The port (7810), caps and limits are in `hub.toml`; the key path is a setting, not a shipped secret (the key is created on first run). |

No relay unit is written (v0.6.0 wrote a `regia-relay.service` template): Regia ships no standalone
relay binary yet.

The units use `User=regia`, `Group=regia`, `WorkingDirectory=/opt/regia`, `Restart=on-failure`,
`NoNewPrivileges=true`, and `WantedBy=multi-user.target`. UDP ports above 1024 need no extra
capability. `server.toml` is unchanged.
