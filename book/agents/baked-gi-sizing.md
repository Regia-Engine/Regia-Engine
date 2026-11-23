# Sizing a GI bake

Before a large map is baked ([Baking GI from meshes](baked-gi-bake.md)), check what its probe layout will cost:
how many probes, how big the `.fgiv` volume will be, whether the unsplit volume fits one portable binding, how many
rays the bake starts, and how much memory it needs. The `bake_sizing` tool answers these on a synthetic stand-in
for a large DO27 map, with no DO27 content. It also runs a timed bake of the same level on a GPU. Design: ADR-1099.
Numbers and the recommended layout for Bore-Twelve-sized maps: `docs/research/bake-sizing-bore-twelve.md`.

## Commands

```text
cargo run --release -p regia_pt --example bake_sizing -- plan  [--level bore12|small] [--layout NAME] [knobs]
cargo run --release -p regia_pt --example bake_sizing -- sweep [--level bore12|small] [--markdown]
cargo run --release -p regia_pt --example bake_sizing -- ron   [--level ..] [--layout ..] [knobs] --out FILE
cargo run --release -p regia_pt --example bake_sizing -- bake  [--level ..] [--layout ..] [knobs] [--out FILE.fgiv]
```

- `plan` builds the level's bake job (the same description path as `regia bake-gi`, with the meshes served from
  memory) and prints its sizes. No GPU.
- `sweep` prints `plan` for the three presets and for the recommended layout with one knob changed each.
  `--markdown` prints the same as one markdown table (the table of the research page) instead of JSON lines.
- `ron` writes the generated `.bake.ron` text. It is for reading: its mesh sources (`synthetic/*.mesh`) exist only in
  memory, so `regia bake-gi` cannot bake the file.
- `bake` bakes on the first GPU adapter and adds the measured numbers.

Levels: `bore12` (1280 m terrain grid at 1.25 m, 20 interiors including a 45 × 100 × 45 m sky-open shaft, 12
towers, 40 containers, 600 scatter rocks, a backdrop ring) and `small` (160 m, 4 interiors). Layouts: `defaults`
(the `.bake.ron` defaults), `doc-example` (bricks on `setpiece` only) and `recommended` (the default).

Knobs override the layout: `--terrain-spacing M` (0 = no terrain layer), `--auto-spacing M`,
`--interior-spacing M`, `--shaft-spacing M` (sky-open interiors), `--max-probes N`, `--layers a,b` (`all` = every
layer but `terrain`), `--moments N`, `--rays N`, `--samples N`, `--batch N`.

## Output

One JSON object per line on stdout; progress goes to stderr. `plan` and `sweep` print:

```json
{"ok": true, "level": "bore12", "layout_name": "recommended", "layout": {...}, "plan_seconds": 1.9,
 "sizing": {
   "probes": 393985, "terrain_probes": 128000, "interior_probes": 175385, "cluster_probes": 90600,
   "other_brick_probes": 0, "bricks": 65, "interior_bricks": 20, "interior_flagged": 19, "cluster_bricks": 45,
   "under_terrain": 23826, "auto_scale": 1.0, "placements": 673, "triangles": 59140,
   "largest_bricks": [{"label": "interior:hangar_a", "probes": 42517, "spacing": 1.0, "counts": [61, 17, 41]}],
   "volume": {"per_probe": 136, "payload": 53684368, "file_max": 53688112, "runtime": 53687480},
   "portable_limit": 134217728, "fits_portable": true,
   "rays": {"live_probes": 370159, "main": 1516171264, "relocation_max": 379042816},
   "gpu": {"batch_probes": 2048, "path_slots": 524288, "scene": 15400000, "paths": 100663296,
           "hits": 16777216, "accum": 1015808, "largest_binding": 100663296, "total": 134000000},
   "gpu_fits_portable": true, "host_peak_estimate": 522000000}}
```

(The values are illustrative: run the command for the real ones.)

- `auto_scale` is the mesh-cluster spacing multiplier (`plan.auto_bricks.scale` on a bake). Above 1, `max_probes`
  coarsened the cluster bricks. Interior bricks keep the requested spacing unless they alone leave fewer than 8
  probes per cluster; that factor is `plan.auto_bricks.interior_scale`, which this report does not copy. Look at
  `largest_bricks` to see which brick took the budget. On a map with a backdrop, `cluster:backdrop+N` at the top
  means the backdrop's layer is in `auto_bricks.layers`.
- `under_terrain` probes are stored in the volume but never traced.
- `volume.runtime` is the unsplit volume's storage buffer. `fits_portable` is true when that buffer is at most
  `portable_limit` (128 MiB). A false value does not refuse the bake: `regia bake-gi` writes a layout above that
  limit as a split volume of up to four parts, each at most 128 MiB, when a split fits (`plan.split_parts` on a
  bake or `--dry-run`; ADR-2618). `bake_volume_too_large` is only a layout no split fits.
- `gpu.largest_binding` above 128 MiB (`gpu_fits_portable: false`) means the bake fails on a device with a 128 MiB
  binding limit. Lower `--batch`.
- `host_peak_estimate` is the bake's probe data at its peak, from the code's type sizes.
- `memory` is the same answer `regia bake-gi --dry-run` prints: the bytes against every tier, what is over and what fixes
  it ([Split baked GI volumes](baked-gi-split.md#memory-before-a-bake), ADR-3006).
- `split_study` measures the cuts a split could make on this layout: the largest part of a 2 × 2 cut in X and Z and of four
  slabs in Y, each with bricks whole and with bricks cropped. It compares the options. The bake does `xz_parts`:
  a cut with whole bricks when one fits, else with the bricks that cross a cut cropped (ADR-3237). `split_parts` is
  each of its parts' bytes and `split_cropped_bricks` how many bricks each holds cropped (both empty when no split is
  needed or none fits). `--level tall` is a map stacked on a small footprint (ADR-3006).

`bake` adds `device`, `seconds`, `stage_seconds` (`relocate N`, `bake`), `filter_encode_seconds`,
`main_rays_per_second`, `projected_seconds_at_layout_rays` (each stage scaled back to the preset's rays and samples
when `--rays`/`--samples` lowered them), `report` (the bake report), `file_bytes`, `gpu_peak_allocated_bytes` (the
allocator's peak during the bake; `null` where the backend gives no report) and `host_peak_rss_bytes` (Linux and
Android; `null` elsewhere).

## Error codes

A failure prints `{"ok": false, "code", "message"}` and exits 1.

| Code | Meaning |
|---|---|
| `sizing_usage` | No or an unknown mode, or `ron` without `--out`. |
| `sizing_level_unknown` | `--level` is not `bore12` or `small`. |
| `sizing_layout_unknown` | `--layout` is not `defaults`, `doc-example` or `recommended`. |
| `sizing_bad_value` | A knob's value does not parse, or a spacing is not positive. |
| `sizing_build_failed` | The description does not build; the message carries the bake's own code (for example `bake_layout` when the bricks cannot fit `max_probes`). |
| `sizing_pack_failed` | The scene does not pack for the tracer. |
| `sizing_no_gpu` | `bake` found no GPU adapter, or the tracer did not start. |
| `sizing_bake_failed` | The bake failed (for example a buffer above the device's binding limit). |
| `sizing_write_failed` | `--out` could not be written. |

## Calibrating the bake time

The ray rate of the probe kernels decides the bake time. Measure it once per card with a fraction of the rays:

```bash
cargo run --release -p regia_pt --example bake_sizing -- bake --level bore12 --rays 64 --samples 4
```

Read `projected_seconds_at_layout_rays`, then confirm it once with the full bake (no `--rays`/`--samples`).
