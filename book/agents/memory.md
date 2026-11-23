# Memory per subsystem (agent)

How an agent sees where a running game's memory goes, CPU heap and GPU side by side, for each engine subsystem, and how
it checks that a level can be loaded and unloaded many times without leaving memory behind (ADR-539).

## Heap tracking

The CPU numbers come from `regia_core::memory`. When the host installs the tagging allocator, every heap allocation
counts against the subsystem the allocating thread is working for, and is freed against the same one:

```rust
#[global_allocator]
static ALLOC: regia_core::memory::TaggingAllocator = regia_core::memory::TaggingAllocator::system();
```

It costs a 16-byte header (or the alignment, when that is larger) and a few atomic adds per allocation. Use it in
dev builds, tests and soaks, not in a shipped game. Without it the heap numbers are `null` and `heap.hint` says why.
`regia_app::Diagnostics::allocated_bytes` is filled from the same ledger.

The engine opens a scope at these entry points: level spawn and unload (`scene`), asset loads (`assets`), the
physics step (`physics`), the audio drain (`audio`) and the frame render (`render`). Game code can open its own:

```rust,wrap
use regia_core::memory::{self, Subsystem};

let _mem = memory::enter(Subsystem::Gameplay); // until `_mem` drops, this thread's allocations are gameplay's
```

Scopes nest, and the innermost one wins. Work outside every scope counts as `unscoped`. That includes host code,
start-up, the standard library, and tasks on worker pools, because a scope belongs to the thread that opened it.

GPU bytes come from the wgpu allocator report (Vulkan and D3D12; `null` on GL and Metal), grouped by label. Terrain
and scatter data count as `terrain`, VFX and particle buffers as `vfx`, UI and glyph textures as `ui`, and everything
else as `render`.

## `memory.report`

Read-only. `top` (default 10, max 200) sets how many of the largest GPU allocations to list.

```json
{"jsonrpc":"2.0","id":1,"method":"memory.report","params":{"top":3}}
```

```json
{
  "heap": { "tracked": true, "live_bytes": 412090368, "peak_bytes": 530579456, "hint": null },
  "gpu": { "available": true, "backend": "Vulkan", "allocated_bytes": 1610612736, "reserved_bytes": 2147483648 },
  "process": { "rss_bytes": 1932735283, "rss_peak_bytes": 2040109465 },
  "ecs": { "entities": 18342, "archetypes": 211, "components": 486 },
  "subsystems": [
    { "name": "unscoped", "heap_bytes": 96468992, "heap_peak_bytes": 120586240, "heap_allocations": 40211, "gpu_bytes": 0 },
    { "name": "scene", "heap_bytes": 25165824, "heap_peak_bytes": 31457280, "heap_allocations": 9120, "gpu_bytes": 0 },
    { "name": "assets", "heap_bytes": 201326592, "heap_peak_bytes": 268435456, "heap_allocations": 3310, "gpu_bytes": 0 },
    { "name": "render", "heap_bytes": 67108864, "heap_peak_bytes": 75497472, "heap_allocations": 12007, "gpu_bytes": 1258291200 },
    { "name": "terrain", "heap_bytes": 0, "heap_peak_bytes": 0, "heap_allocations": 0, "gpu_bytes": 335544320 }
  ],
  "owners": [
    { "name": "render.shader_modules", "subsystem": "render", "heap_bytes": 9437184, "heap_peak_bytes": 11534336, "heap_allocations": 52110 },
    { "name": "render.frame", "subsystem": "render", "heap_bytes": 4194304, "heap_peak_bytes": 6291456, "heap_allocations": 3012 }
  ],
  "top_gpu": [
    { "label": "material texture (streamed)", "subsystem": "render", "category": "meshes", "bytes": 402653184, "count": 311 }
  ],
  "since_mark": null
}
```

Every subsystem is listed in a fixed order: `unscoped`, `scene`, `assets`, `render`, `terrain`, `physics`, `audio`,
`animation`, `navigation`, `ui`, `vfx`, `telemetry`, `agent`, `gameplay`. The example above shows only the first five.

| Field | Meaning |
|---|---|
| `heap.tracked` | Whether the tagging allocator is installed. When it is `false`, every heap field is `null`. |
| `heap.live_bytes`, `heap.peak_bytes` | Heap bytes held now, and the highest value seen. Allocator headers are not counted. |
| `gpu` | The allocator report's totals; `null` before a render device exists. |
| `process` | Resident set now and at its peak. Linux and Android use `VmRSS` and `VmHWM`. Windows uses the working set and peak working set. macOS uses the task's resident size and `ru_maxrss` (ADR-5860). `null` on other targets and when the OS query fails. |
| `ecs` | Spawned entities, archetypes, and registered components. |
| `subsystems[].heap_bytes` | Live heap bytes the subsystem allocated and has not freed. |
| `subsystems[].heap_allocations` | Live allocations of the subsystem. |
| `subsystems[].gpu_bytes` | GPU bytes whose label maps to the subsystem; `null` without an allocator report. |
| `owners` | The named owners (above), largest live heap first: `name`, `subsystem`, `heap_bytes`, `heap_peak_bytes`, `heap_allocations`. Their bytes are also in their subsystem's row. Empty when the heap is not tracked or no owner was entered. |
| `top_gpu` | The largest GPU labels: bytes, allocation count, subsystem and the finer `seeing.memory_report` category. |
| `since_mark` | The change since the last `memory.mark` (below); `null` before one. |
| `assets_by_type` | Per asset type (`mesh`, `image`, `texture`, `material`): `type`, `loaded` (assets loaded now) and `bytes` (the heap blocks their decoded payloads hold; a texture from a mapped pak counts its payload length). Counted when asked, never per frame. With the tagging allocator they add up to the `assets` subsystem's growth over the loads within 64 KiB plus 2 %: the rest is the asset server's own maps (ADR-9740). `null` without an asset server. |

### Named owners inside a subsystem

A subsystem row says which part of the engine holds bytes; an owner row says what inside it does (ADR-3351). An owner
is a `static` declared next to the code that allocates, entered like a scope:

```rust,wrap
use regia_core::memory::{self, MemoryOwner, Subsystem};

static LEVEL_CACHE: MemoryOwner = MemoryOwner::new(Subsystem::Gameplay, "gameplay.level_cache");
let _mem = memory::enter_owner(&LEVEL_CACHE); // gameplay's bytes, and the owner's row
```

An owner's bytes count in its subsystem too, so the subsystem totals do not change. A plain `memory::enter` of another
subsystem inside an owner scope clears the owner (an asset load started by renderer code is the asset subsystem's);
one of the owner's own subsystem keeps it. The innermost owner wins. At most 63 owners get a row (the first ones
entered in the process); later ones count in their subsystem only.

The renderer names its start-up and idle holders (`regia_render::heap_owners`):

| Owner | What it holds |
|---|---|
| `render.plugin` | What `RenderPlugin::build` allocates: resources, schedules, type registrations. |
| `render.device` | The wgpu instance, adapter and device. |
| `render.shader_modules` | WGSL composition: the per-thread composer over the engine library, its cached imports, and the composed modules the shader modules keep. |
| `render.pipelines` | Render and compute pipeline objects and their backend compile. |
| `render.pipeline_cache` | The persistent pipeline cache's file payload, load and save. |
| `render.atmosphere_luts` | The sky and atmosphere tables (transmittance, multiple scattering, sky view, aerial perspective). |
| `render.blue_noise` | The spatiotemporal blue-noise texture (only with the knob on). |
| `render.color_luts` | Level LUTs, post-volume LUTs and the color-vision LUT. |
| `render.cloud_noise` | Cloud noise volumes and the curl field. |
| `render.extract` | The frame description extracted from the ECS each frame. |
| `render.upload_queue` | The background upload queue. |
| `render.frame` | The frame renderer's own state outside every owner above (texture views and windows). |

## `memory.mark`

Read-only, no parameters. Records the heap ledger, the entity count and the GPU total as a baseline, and replaces any
earlier mark. Later `memory.report` answers carry `since_mark`:

```json
{ "heap_bytes": 1048576, "largest": "assets",
  "subsystems": { "assets": { "heap_bytes": 1048576, "heap_allocations": 12 } },
  "gpu_bytes": 0, "entities": 0 }
```

`subsystems` lists only the subsystems that changed, `owners` only the named owners that changed. `largest` names the
subsystem that gained the most heap bytes.
`gpu_bytes` is `null` when either side had no allocator report.

## Checks an agent can run

- **A level unloads cleanly:** load the level once and unload it, so caches settle. Call `memory.mark`. Then load and
  unload it five times and call `memory.report`. `since_mark.entities` is 0. `since_mark.heap_bytes` stays within a
  few KiB and does not grow with each extra round. If it does grow, `since_mark.largest` names the subsystem holding
  the memory.
- **What fills the GPU, by owner:** compare `subsystems[].gpu_bytes` for `terrain` and `render`, then read `top_gpu`
  for the labels.
- **Heap not tracked:** when `heap.tracked` is `false`, read `heap.hint` and use `process.rss_bytes` for the total.
- **What inside the renderer holds the heap:** read `owners` (largest first). The `render` row minus the sum of the
  `render.*` owners is what the renderer allocated outside every owner; start-up work outside every scope is still
  `unscoped`.

## The soak test

`crates/regia_scene/tests/level_leak_soak.rs` runs 1000 load/unload cycles of a 64-entity level under the tagging
allocator. Each cycle parses the level from RON, opens it, runs a frame, unloads it, drops its data from the
`LevelLibrary` and runs another frame. The heap and the `scene` subsystem are sampled after every unload and judged
with `regia_core::memory::LeakCheck`. The first 100 cycles are skipped. Then the peak of the last 100 cycles may
exceed the peak of the first 100 measured cycles by at most 8 KiB. One 16-byte allocation kept per cycle would add
12.8 KB and fail. A control run of 300 more cycles leaks 64 bytes per cycle on purpose and must fail the same check.

## Telemetry

The 10 s `regia::frame_summary` record carries the same numbers in `memory.subsystems`:
`heap_tracked`, `heap_live_bytes`, `heap_peak_bytes`, and `by_subsystem: { <name>: { heap_bytes, gpu_bytes } }`.
GPU bytes there come from the last telemetry probe sample, so they can be up to 10 s old. See
[Telemetry](telemetry.md).
