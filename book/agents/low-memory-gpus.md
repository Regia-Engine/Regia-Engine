# Low-memory GPUs (agent)

How an agent checks the out-of-memory handling of a running game: whether the integrated-GPU defaults apply, whether
texture streaming caught allocation failures, and what fills GPU memory. The engine side is in
[the host checklist](../user-guide/do27-host-checklist.md#low-memory-gpus-v0116).

## `render.memory_pressure`

Read-only, no parameters. Works without a renderer (the process-wide fields are filled; `stream` and
`integrated_limits` are `null`).

```json
{"jsonrpc":"2.0","id":1,"method":"render.memory_pressure","params":{}}
```

```json
{
  "signals": 2, "texture_budget_steps": 1, "oom_uncaught": 0, "oom_caught": 3,
  "mips_held": false, "mip_drops": 0, "device_lost": false,
  "integrated_limits": true, "new_scene_warm": false,
  "stream": { "resident_bytes": 201326592, "budget_bytes": 201326592, "held": false, "backing_off": true },
  "categories": { "meshes": 211812352, "textures": 201326592, "virtual_texture": 0, "terrain": 157286400,
                  "staging": 33554432, "atlases": 0, "shadows": 4194304, "render_targets": 62914560, "other": 1048576 },
  "pipeline_cache_bytes": 2346284
}
```

| Field | Meaning |
|---|---|
| `signals` | Memory-pressure signals accepted since start (probe, OS, out-of-memory). |
| `texture_budget_steps` | How many times the streaming budget was halved. The first out-of-memory error steps it. |
| `oom_uncaught` | Out-of-memory errors that reached wgpu's uncaptured handler. Should stay 0. |
| `oom_caught` | Streamed texture allocations that failed and were handled (retried smaller or dropped). |
| `mips_held` | Streamed textures are held one mip lower (an error came soon after a trim; lasts 60 s). |
| `mip_drops` | How many times every streamed set gave up a level. |
| `integrated_limits` | The integrated-GPU defaults apply (384 MiB budget at most, low mips first, no new-scene warm): a desktop integrated GPU under 15,000 MiB of RAM and 8 GiB of device-local heap. |
| `new_scene_warm` | Whether a large new scene on a scene-less window would be warmed on this device. |
| `stream.backing_off` | Streaming is paused after a failed allocation (5 s, doubling to 60 s). |
| `categories` | GPU bytes per bucket at the last telemetry probe sample (`null` before one). |
| `pipeline_cache_bytes` | Size of the persistent pipeline cache (RAM). |

## Checks an agent can run

`render.memory_pressure` is a query, not a low-memory-device test. It runs headless without a
renderer: process-wide counters are returned, `integrated_limits` and `stream` are `null` without a
renderer, and `new_scene_warm` is `null` without a `GameFrameWarm` resource. `categories` does not read the
renderer; it is `null` only before the first telemetry probe sample. The handler declares no error codes
and accepts no parameters. The map-change check below can run headless; its results describe
that run's counters and do not require a low-memory device.

- **The defaults on this machine:** `integrated_limits` is `true` only on a desktop integrated GPU that is not a Steam
  Deck and is not large (16 GB of RAM or more keeps the presets; `REGIA_INTEGRATED_LIMITS=1` forces the defaults,
  `0` turns them off; `true`/`yes`/`on` and `false`/`no`/`off` work too). Then `stream.budget_bytes` is at most 402653184 (384 MiB) and `new_scene_warm` is `false` unless the game was
  started with `REGIA_SCENE_WARM=1` (or `true`, `yes`, `on`).
- **Very Low carries the cap on any RAM size** (ADR-8878): the quality tier Very Low (`render.set_profile
  {"preset":"VeryLow"}`, `--render-profile VeryLow`, Auto on a weak GPU) writes the same 384 MiB budget and lazy
  first use into its profile, so `stream.budget_bytes` is at most 402653184 at Very Low even on a 16 GB integrated
  laptop where `integrated_limits` is `false`. A discrete card at Very Low still grows the budget to a quarter of its
  VRAM, as at Desktop Low. Pair it with `render.memory_budget {"tier":"integrated_low"}` (2.5 GiB RAM, 1.5 GiB GPU
  memory).
- **A map change survives:** read `render.memory_pressure` before and after loading the next map. `device_lost` and
  `oom_uncaught` stay `false` and 0. `categories.meshes` and `categories.textures` drop after the old map is
  released and before the next one is preloaded.
- **What filled memory:** `categories` uses the same buckets as the 10 s telemetry summary (`memory.categories`) and
  the device-lost crash bundle. `seeing.memory_report` lists every label.
- **Softer textures for a minute** after `mips_held` turns `true` are expected; they sharpen when it turns `false`.

Related: `render.problems` (`vram_pressure`, `gpu_errors`), `assets.texture_memory` (per texture), `seeing.memory_report`
(per label), [telemetry](telemetry.md).
