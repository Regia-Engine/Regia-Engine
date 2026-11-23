# Showcase performance table

`cargo xtask showcase-table` measures the showcase scene on one card, per quality tier and per camera view,
and writes `docs/perf/showcase-table.md` and `showcase-table.json`. Every number comes from the run stamped in
the file. A row that was not measured says `not run`; nothing is estimated or carried over from another run.

## What it does

For each tier the command starts the prebuilt `showcase` host headless (`--render-profile <preset>`,
`--size WxH`), connects to its agent endpoint, checks `render.get_capabilities` (adapter) and
`render.get_profile` (the preset really applied), waits for `render.pipeline_warm` to go idle, and then, for
each view of the scene's `perf_views.json`:

1. `camera.set_path` with the view's path and `play: true`;
2. `render.out_start { frames, warmup_frames, fps: 60, timings: true, hud: false }` at the table's size;
3. polls `render.out_progress` until the job ends, then reads `render_ms` from `<name>_render_out.json`.

`render_ms` is the wall time of each delivered frame's render and readback (the new `timings` option of
[render-out](render-out.md)). PNG encoding runs on the writer threads and is not in it. The table reports
per view: measured frames, minimum fps (the slowest frame), average fps (frames over total time), maximum fps,
p50 and p99 frame time (nearest rank). The header records date, commit, scene, resolution, adapter, backend,
driver (when `nvidia-smi` can name it), `REGIA_GPU_INDEX` and `REGIA_GPU_UUID`.

## Run it

```sh
# Build once (release), then run on one card. The card is named, never guessed; card 6 is refused.
cargo build --release -p showcase
REGIA_GPU_INDEX=2 REGIA_GPU_UUID=GPU-... \
  cargo xtask showcase-table --scene /path/to/forest_scene --tiers all --size 2560x1440

# Software adapter (lavapipe): no card and no lock. The rows are marked CPU fallback numbers.
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo xtask showcase-table --cpu --size 640x360 --frames 12 --warmup 4 --tiers high,mobile-low-lite

# Write the planned table (every row `not run`) without a card or a binary.
cargo xtask showcase-table --dry-run
```

| Flag | Default | Meaning |
|---|---|---|
| `--scene DIR` | `samples/showcase/example_scene` | The scene the host loads. A forest scene in the cook layout works the same. |
| `--views FILE` | `<scene>/perf_views.json` | The views (below). |
| `--bin FILE` | `target/release/showcase` | The prebuilt host (honors `CARGO_TARGET_DIR`). Nothing is built. |
| `--out DIR` | `docs/perf` | Where `showcase-table.md`, `.json` and `logs/` go. |
| `--tiers LIST` | `all` | `ultra`, `high`, `low`, `deck`, `mobile-high`, `mobile-low`, `mobile-high-lite`, `mobile-low-lite`. |
| `--size WxH` | `2560x1440` | Output size. |
| `--frames N` | 240 | Measured frames per view. |
| `--warmup N` | 30 | Warm-up frames per view (rendered, not measured). |
| `--cpu` | off | A software adapter run. Fails if the host lands on a hardware adapter. |
| `--timeout-seconds N` | 1800 | Ceiling for one tier's host. |
| `--dry-run` | off | Write the planned table without launching anything. |
| `--two-phase on\|off` | preset | Sends `render.set_occlusion { two_phase }` to each tier's host before it measures ([occlusion](occlusion.md)); every measured row's note names the override. Used to measure the switch on tiers whose preset leaves it off (ADR-3286). |

The commit in the header is `git rev-parse`; set `SHOWCASE_TABLE_COMMIT` when the tree has no `.git`.
A copied `xtask` on a machine without the build tree starts the host in its own working directory (ADR-3286); give
`--scene` and `--bin` as absolute paths there.
A slow first frame (a cold pipeline compile) can leave the host silent for a while; the command retries calls the host
answers with `F0006` and cancels a failed view's job so the next view can start.

A GPU run holds `flock /tmp/regia-gpu-<n>.lock flock /tmp/ferrite-gpu-<n>.lock` for each tier's host (skipped when `REGIA_GPU_LOCK_HELD=<n>`
says the caller holds it) and fails if the host lands on a software adapter, so a wrong Vulkan ICD cannot pass as
a card. To compare cards or builds, interleave runs on one card; two cards of one model differ by a few percent.

## The views file

```json
{ "version": 1, "views": [
  { "name": "canopy", "description": "free text, ignored",
    "path": { "interp": "CatmullRom", "keys": [ { "time": 0, "position": [0, 12, 0] }, { "time": 8, "position": [40, 12, 30] } ] },
    "look_at": { "Point": [0, 8, 60] }, "fov": [ { "time": 0, "value": 45 } ] } ] }
```

Each view (minus `name` and `description`) is the params of one `camera.set_path` call. A path must cover the
warm-up plus the measured frames at 60 Hz (`(warmup + frames) / 60` seconds); a shorter one is refused with
`view_too_short` and no row is measured. Names are 1 to 48 letters, digits, `_` or `-`.

## Tiers

| Tier | Preset | Forward lite |
|---|---|---|
| `ultra` | `DesktopUltra` | off |
| `high` | `DesktopHigh` | off |
| `low` | `DesktopLow` | off |
| `deck` | `Deck` | off |
| `mobile-high` | `MobileHigh` | what the profile asks (tile GPUs only) |
| `mobile-low` | `MobileLow` | what the profile asks (tile GPUs only) |
| `mobile-high-lite` | `MobileHigh` | forced `high` (`REGIA_FORWARD_LITE`) |
| `mobile-low-lite` | `MobileLow` | forced `low` |

The Mobile profiles only take the lite forward shader on tile-GPU vendors and Android (`adapter_takes_lite`).
On a desktop card or lavapipe they draw the full shader, so the `-lite` rows force it: they measure the lite
shader's cost on that card, not a phone's frame rate. The written table lists, for every tier, the lowest GPU
class the specs name and says plainly that those are design targets, not measurements.

## Does the visibility-buffer path need mesh shaders?

No. The cluster cull is a compute pass, the visibility pass is a hardware-raster draw into an `R32Uint`
target, and the material resolve is a fragment pass; the software rasterizer for tiny clusters uses two
32-bit atomics per pixel. The only hard requirement is 11 storage buffers per fragment stage
(`RESOLVE_STORAGE_BUFFERS`); below that the meshes stay on the forward path
(`fallback_visbuffer_storage` in `render.problems`). `MULTI_DRAW_INDIRECT_COUNT`, `INDIRECT_FIRST_INSTANCE`
and `TIMESTAMP_QUERY` are optional. The path is off in every preset, so the tier rows do not use it.

## Agent method

An agent can run one view without the xtask: `camera.set_path`, then `render.out_start` with `timings: true`,
then read `render_ms` from the manifest. Both methods already exist; `timings` is the only addition.
The xtask is that recipe with the pinning, the checks and the report around it.

## Errors

The xtask exits 0 when every row was measured, 1 when any row is `not run` after a launch (the row's note says
why), and fails before launching on a bad argument. Notes and errors use lowercase codes where they come from
the protocol: `view_too_short` (path shorter than the run), and the render-out job's own error text.
`render.out_start` answers `invalid_params` for a `timings` that is not a boolean.

## What is not in the table

Mac and Android rows are not part of it until a device pass has measured them. They read `not run`.
