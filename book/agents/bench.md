# Public benchmark scenes

`regia bench` runs the public benchmark scene set headless and prints frame stats, so a number can be
reproduced by anyone with the scenes and a card. The set and its licences are in
`docs/benchmark-scenes.md`.

It is a developer tool, like the [replay soak](replay-soak.md): there is no running app to call, so it
has no agent-protocol method. An agent runs the command with `--json` and reads one JSON object.

## Run it

```sh
regia bench --list                                # the set, licences, what is on disk
scripts/fetch-bench-scenes.sh                     # same as regia bench --fetch: the publishable set
regia bench                                       # every publishable scene on disk, 1080p
regia bench synthetic_atrium flight_helmet --frames 600 --size 2560x1440
regia bench --json > bench.json
regia bench --restricted sponza_khronos           # local only: never publish this row
regia bench synthetic_grass_1m template_npc_200 --json   # the generated scenes, with counters
regia bench synthetic_grass_1m template_npc_200 --small --frames 2 --warmup 1 --size 96x54 --json
```

| Flag | Default | Meaning |
|---|---|---|
| `SCENE...` | every publishable scene on disk | Scene ids. Naming one that is not on disk is refused. |
| `--list` | off | Print the set and exit. |
| `--fetch` | off | Download the named scenes, check them and write `LICENSES.txt`, then exit. |
| `--assets DIR` | `.regia/bench-scenes` | Where fetched scenes live. |
| `--restricted` | off | Allow scenes marked local only. |
| `--gltf FILE` | none | Also measure this glTF or GLB, reported as `file:<stem>` and marked not publishable. Alone it measures just that file. |
| `--png-dir DIR` | none | Write each scene's last frame as `<id>.png`, to see what was measured. |
| `--frames N` | 300 | Measured frames per scene. |
| `--warmup N` | 60 | Frames before the measured ones. |
| `--size WxH` | `1920x1080` | Image size. |
| `--held` | off | Hold the camera still instead of one orbit over the measured frames. |
| `--small` | off | Build the generated scenes with counters at their small size: 10,000 grass blades and 20 NPCs. Their rows say `"size": "small"`. For tests on a software rasterizer; never publish a small row as the scene's number. |
| `--json` | off | Print one JSON object on stdout instead of the table. |
| `--on LIST` | none | Turn opt-in renderer features on for their cost: `radiance-cache`, `blue-noise`, `oit`, `sss`, `shadow-occlusion`, `lens-effects`, and the material probes `thin-film` (a film on every non-transmissive material), `transmission` (read the glTF transmission, IOR and volume extensions, which the default load ignores) and `rough-transmission` (that, spread by the material's roughness) (ADR-2309; [Card costs of opt-in renderer features](feature-costs.md)). |
| `--preset NAME` | `desktop-high` | Render tier: `desktop-high`, `desktop-ultra`, `desktop-low` or `deck`. |

Pick the card with the usual variables (`REGIA_GPU_UUID`, `REGIA_GPU_STRICT=1`,
[Performance & benches](../user-guide/perf-benches.md)).

## What it measures

Each scene spawns into an ECS world and renders through `extract_frame` and a persistent
`FrameRenderer` at Desktop High (or `--preset`) into an offscreen target, with two frames in flight. The camera makes
one orbit over the measured frames, starting from the warm-up pose, so culling and shadows do real work
(`--held` measures a still view instead). Skinned models draw in their bind pose.

It does not time a window's present, and the synthetic scenes have no textures. The glTF path loads
images as RGBA8 with no block compression, so its VRAM and sampling cost differ from a cooked game's.

## The report

`--json` prints:

- `schema` (1), `ok`, `engine_version`, `adapter` (`name`, `driver`, `backend`, `device_type`), `size`,
  `warmup`, `frames`, `held`, `preset` and `on` (the features turned on).
- `scenes`: one object per scene with `id`, `licence`, `publishable`, `meshes`, `materials`, `triangles`,
  `load_s`, `frames`, `fps_p50`, and:
  - `wall_ms`, `cpu_encode_ms`, `cpu_submit_ms`: each `p50`, `p95`, `p99`, `max`, `mean`.
  - `gpu_frame_ms`: the same percentiles, `null` on an adapter without timestamp queries.
  - `gpu_passes_p50_ms`: median per pass (`shadows`, `clusters`, `gi`, `prepass`, `cull`, `ssao`, `forward`,
    `ssgi`, `ssr`, `post`), empty without timestamp queries.
  - `draws`: `drawn`, `culled`, `instanced`, `per_object` on the last frame.
  - `last_frame`: `mean_luma` (0 to 255) and `lit_pct`, the percent of pixels over near-black, read back after
    the timing. A frame near zero means the numbers measured nothing: look at it with `--png-dir`.
  - `gpu_memory`: `allocated_bytes` and `reserved_bytes` of the renderer after the last frame (the fields of
    `seeing.memory_report`), `null` on a backend without an allocator report.
  - `probed_materials`, only with a material probe on: `thin_film` and `transmission` (how many distinct
    materials the probes changed) and `rough`. A zero means the feature had nothing to draw on that scene.

### Generated scenes with counters (ADR-2695)

`synthetic_grass_1m` and `template_npc_200` are built from code (no fetched content, fixed seeds and
paths) and add a `counters` object to their row, and `size` (`full` or `small`):

- `synthetic_grass_1m`: an 80 m terrain patch whose grass field is 100 cells x 625 clumps x 16 blades =
  1,000,000 blades, at the ultra grass tier whatever the preset, with the wind on and its clock set from
  the frame index. The camera circles inside the patch at eye height. `counters` has `kind` `grass`,
  `field_blades` (the plan's product), `census` (one frame from above that takes every cell: `patches`,
  `candidates`, and `blades`, the renderer's candidate clumps times the blades per clump, which equals
  `field_blades`), `last_frame` (`drawing`, `quality`, `patches`, `camera_patches`, `candidates`,
  `walked_blades` for the measured view, `max_instances`), `wind` (`strength`, `sway_m`, `clock_s`,
  `frames_prepared`) and `frames`. Blades are counted where the renderer reports them: as clumps it walks.
  The renderer does not report how many clumps survive the density law and the masks (the drawn count).
- `template_npc_200`: a plaza with 200 agents walking two opposing flows at mixed speeds, turning round at
  the goal, one fixed 30 Hz step per frame (warm-up included). `counters` has `kind` `npc`, `agents`,
  `skinned_draws` and `skinned_instanced` (the renderer's counters of the last frame), `animation`
  (`steps`, and the totals of `updates`, `skipped`, `poses_evaluated`, `pose_shares`, plus the
  `last_step` with the agents per LOD tier and locked feet) and `path` (`turned_at_goal`, and the
  `nav_digest` and `crowd_digest` of the end state).

Two runs of the same command give the same `counters`: nothing in them reads a clock.

`--list --json` prints `schema` and `scenes` with `id`, `label`, `licence`, `credit`, `publishable`,
`synthetic`, `on_disk` and `bytes`. `--fetch --json` prints `ok`, `assets` and `fetched`.

A published number also names the scene id, the card (exclusive, under its lock), the load average and the
build, and goes into `docs/perf-log.md`. Never compare numbers from a shared card.

## Errors

With `--json` an error prints `{"ok": false, "code", "message"}`. The exit is 2 for a refused request and 1
for a failed run.

| Code | Exit | Meaning |
|---|---|---|
| `bench_unknown_scene` | 2 | No scene with that id. |
| `bench_scene_missing` | 2 | The scene is not on disk: run `--fetch`. |
| `bench_scene_restricted` | 2 | The scene is local only: pass `--restricted`. |
| `bench_unknown_feature` | 2 | An `--on` name that is not a feature. |
| `bench_bad_args` | 2 | `--frames` is zero, or `--preset` names no tier. |
| `bench_manifest_invalid` | 1 | The embedded manifest does not validate (a bug in this build). |
| `bench_fetch_failed` | 1 | A download or a write failed. |
| `bench_checksum_mismatch` | 1 | A download does not match its pinned size and SHA-256. It was removed. |
| `bench_load_failed` | 1 | The scene does not parse or draws nothing. |
| `bench_render_failed` | 1 | A frame failed. |
| `bench_no_gpu` | 1 | No device, or `REGIA_GPU_STRICT=1` and the requested card was not found. |
