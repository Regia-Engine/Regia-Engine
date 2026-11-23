# Load memory: the staging flush

A level's first frame uploads every new material texture and mesh. wgpu copies each
`write_texture` / `write_buffer` into a staging buffer of its own and keeps it until the next
submit, and the renderer submits once per frame. Before ADR-1346 the whole level's textures sat in
staging at once, on top of the CPU copies the materials hold: the DO27 map pak (4.9 GiB of
textures) peaked at about 9 GiB of RSS.

The staging flush bounds that. The renderer counts the bytes written on a thread since its last
submit; at the cap it submits the pending writes on their own, and the frame path waits for its
previous flush before it writes more. A load then holds about two caps of staging (one the GPU
copies while the CPU fills the next) instead of the level.

The frame draws the same pixels: wgpu applies every pending write before the next submit's
command buffers, so a write moved into an earlier submission changes what no pass reads
(`a_frame_flushed_at_every_write_draws_the_same_pixels`). A steady frame writes far less than the
cap and never flushes.

## Read

`seeing.memory_report` carries the flush and the process peak RSS next to the GPU categories:

```json
seeing.memory_report { "top": 3 }
→ { "available": true, "backend": "Vulkan", "rss_bytes": 2491..., "peak_rss_bytes": 5372...,
    "staging": { "cap_bytes": 134217728, "flushes": 38, "flushed_bytes": 5100273664,
                 "waits": 37, "peak_pending_bytes": 139460608 },
    "categories": { ... }, "labels": [ ... ] }
```

- `peak_rss_bytes`: the process's peak resident set. Linux and Android report `VmHWM`; Windows
  reports the peak working set, macOS `ru_maxrss` (ADR-5860). `null` on other targets and when the OS query fails.
  It never goes down, so compare two settings in two processes.
- `staging.cap_bytes`: the cap in force (`0` = the flush is off).
- `staging.flushes`, `staging.flushed_bytes`: flushes since start and the bytes they moved.
- `staging.waits`: flushes that waited for the flush before them. Set-up writes through handles
  that do not know their device flush without waiting, so `waits` can be lower than `flushes`.
- `staging.peak_pending_bytes`: the most bytes ever written on one thread between two submits:
  the largest staging wgpu held at once for one thread. With the flush on it stays at most the cap
  plus the largest single write; with it off it is about the largest first frame's upload.

## Set the cap

- Default 128 MiB.
- `REGIA_STAGING_FLUSH_MIB=<MiB>` sets it at start; `0` or `off` turns the flush off (the
  behavior before ADR-1346, for A/B runs).
- A host calls `regia_render::set_staging_flush_cap(Some(bytes))` before a level load (for
  example lower on a handheld or a phone), and `set_staging_flush_cap(None)` to go back to the
  environment or the default.

Each flush is one empty submit and, on the frame path, one wait for the previous flush, which is
normally already done. A smaller cap means more of them.

## Measure

- `pak_load_bench load --paks DIR --frame 1280x720` (a shipped map from paks) and
  `load_rss_bench [--materials 64] [--side 2048]` (a synthetic scene of distinct BC7/BC5 chains,
  no content needed) print the peak RSS before and after the first frame. Run each with
  `REGIA_STAGING_FLUSH_MIB=0` and without, in separate processes.
- `docs/evidence/perf/load-rss-20261021/ab.sh` runs both, alternating, under the GPU lock.

Related: `seeing.memory_report` on the [seeing](seeing.md) page (GPU memory by category),
`render.memory_budget` (load sets against a tier budget), and
`docs/evidence/perf/memory-20261007.md` (the DO27 memory budgets).

## Host memory budget (ADR-7891)

The renderer draws an uploaded mesh from its GPU buffers; the CPU copy is only needed again by a
CPU reader, after device loss, or when the GPU copy is evicted. A host that keeps its meshes in a
`regia_render::host_memory::HostMeshStore` gets a cap on those copies:

- `insert_pak(reader, asset_id)` loads a cooked mesh and keeps the pak source; `insert_owned(..)`
  keeps a procedural mesh (never dropped, counted as `unsourced_bytes`).
- Each frame: `prepare(&renderer, &quality, draws)` re-reads the dropped copies the frame needs,
  `frame_mesh(..)` builds each `FrameMesh` (a dropped copy is a *resident draw*: empty slices, drawn
  from the GPU buffers), and `after_frame(&renderer)` drops copies least recently drawn first until
  `cpu_mesh_bytes` fits the budget.
- `pin(id)` / `unpin(id)` mark a CPU consumer (a collider build, picking, CPU skinning or morphs, a
  mask decal or scalar overlay); a pinned mesh is never dropped. `cpu_geometry(id)` serves a late
  reader (re-read from the pak when dropped).
- Only meshes the renderer can draw without bytes are dropped: none while the visibility buffer,
  DDGI probes, the traced sun, ReSTIR GI or lightmap receivers are on, and never one whose material
  is an emissive mesh light, a surface graph or transmissive. Those frames count over-budget ticks.
- Texture payloads are counted (`set_cpu_texture_bytes`) against `cpu_texture_bytes`, not dropped yet.

The budget is `RenderProfile::host_memory` (per preset: Mobile Low 128/64 MiB; Mobile High, Deck and
Desktop Low 512/256 MiB; Desktop High 1536/512 MiB; Desktop Ultra and Cinematic 3072/1024 MiB, mesh/texture),
unless the process-wide override is set.

| Method | Params | Answers |
|---|---|---|
| `render.set_host_budget` | `cpu_mesh_bytes` (integer, optional), `cpu_texture_bytes` (integer, optional), `clear` (boolean, optional) | the `host` block below; a side left out keeps its value, `clear: true` removes the override |
| `render.memory_budget` | as before | adds `host: { budget: { cpu_mesh_bytes, cpu_texture_bytes }, override, cpu_mesh_bytes, cpu_texture_bytes, dropped_bytes, drops, reread_bytes, rereads, over_budget_ticks, pinned_bytes, unsourced_bytes, mapped_touched_bytes }` (`null` budget side = unlimited) |

Errors: `host_budget_bad_params` (INVALID_PARAMS): no byte value and no `clear`, a byte value that is
not a whole number of 0 or more, a `clear` that is not a boolean, or `clear` with byte values.

```json
{"jsonrpc":"2.0","id":7,"method":"render.set_host_budget","params":{"cpu_mesh_bytes":67108864}}
{"jsonrpc":"2.0","id":8,"method":"render.memory_budget","params":{"tier":"deck"}}
```

`tier` is `minimum` (the default), `deck`, `desktop_high` or `integrated_low` (the integrated-GPU floor: 2.5 GiB RAM,
1.5 GiB GPU memory; the budget a host running Very Low admits against, ADR-8878).

`mapped_touched_bytes` is the size of the pak entries read through the pak's memory mapping: those
file pages count in the process RSS while the pak is mapped (clean, reclaimable), so compare RSS with
`cpu_mesh_bytes + mapped_touched_bytes`. The same counters are in `regia_render::memory_report`'s
`MemoryReport::host` and in the telemetry window record's `memory.host`.

Tests: `cargo test -p regia_render --test host_mesh_budget` (512 MiB drawn under 64 MiB, resident
draws pixel-equal, re-read after the GPU copies go, a pinned mesh kept) and `--test host_mesh_rss`
(process RSS growth within the derived bound, Linux).
