# Start-up work and idle memory (agent)

Two costs the per-frame counters do not see: the work an app does before its first presented frame, and the heap it
keeps holding while nothing happens. `app.startup_work` counts the first, `memory.idle_ledger` the second (ADR-2987).
Both are counts and bytes, not times, so a software adapter (lavapipe) reads what a card would, and the start-up
ratchet test can hold them to a committed table.

## `app.startup_work`

Read-only, no parameters. The agent plugin takes one snapshot of the engine's counters in `Last` of the first frame
that presented a window (the `first_present` boot mark, splash included). A headless host has no window, so its
first rendered texture camera ends the start-up window instead (`first_texture_view`). An app that does neither in
600 frames, such as a server, is captured then (`frame_limit`). Before that frame the answer has
`captured: false` and a `hint`.

```json
{"jsonrpc":"2.0","id":1,"method":"app.startup_work","params":{}}
```

```json
{
  "captured": true,
  "trigger": "first_present",
  "frame": 2,
  "ms_since_process_start": 913.4,
  "heap_tracked": true,
  "figures": {
    "pipelines_created": 44,
    "gpu_live_shader_modules": 39,
    "gpu_live_bind_group_layouts": 61,
    "gpu_live_render_pipelines": 30,
    "gpu_live_compute_pipelines": 14,
    "vfs_opens": 3,
    "vfs_bytes_read": 18211,
    "asset_loads": 0,
    "threads_live": 41,
    "heap_allocations": 412900,
    "heap_peak_bytes": 96468992
  }
}
```

The example lists a few of the figures. The full set:

| Figure | What it counts, from process start to the snapshot |
|---|---|
| `pipelines_created` | Render and compute pipelines created through the engine's creation sites (each passes `pipeline_cache::cache_for`). |
| `gpu_live_shader_modules`, `gpu_live_bind_group_layouts`, `gpu_live_pipeline_layouts`, `gpu_live_render_pipelines`, `gpu_live_compute_pipelines`, `gpu_live_bind_groups`, `gpu_live_buffers`, `gpu_live_textures`, `gpu_live_texture_views`, `gpu_live_samplers` | wgpu objects whose handles are still held at the snapshot, from wgpu's own registry report. wgpu shares equal bind group layouts, so that figure counts distinct layouts. `null` without a render device. |
| `tracked_buffers`, `tracked_buffer_bytes`, `tracked_textures`, `tracked_texture_bytes`, `tracked_bind_groups`, `tracked_upload_bytes`, `tracked_submits` | GPU creations, uploads and submits through the renderer's tracked device on every thread (the frame-path counters of [`play.hitch_report`](hitches.md)). |
| `vfs_opens`, `vfs_failed_opens`, `vfs_bytes_read`, `vfs_writes`, `vfs_bytes_written` | File work through the platform VFS (`assets://`, `config://`, `user://`, `cache://`). |
| `asset_loads`, `asset_failed_loads`, `asset_files_read`, `asset_bytes_read` | The asset server's typed loads and raw cooked reads, and the pak entries and cache files it read for them. The asset server does not read through the VFS, so these are separate. |
| `threads_live` | Threads of the process at the snapshot (Linux and Android; `null` elsewhere). |
| `heap_allocations`, `heap_allocated_bytes` | Heap allocations made and bytes allocated. |
| `heap_peak_bytes`, `heap_live_bytes` | The highest heap in use, and the heap in use at the snapshot. |

The heap figures need the tagging allocator (`regia_core::memory::TaggingAllocator` as the host's
`#[global_allocator]`, see [Memory per subsystem](memory.md)). Without it they are `null` and `heap_tracked` is false.
`ms_since_process_start` is for reading only: nothing compares it.

## `memory.idle_ledger`

Read-only. `frames` (default 120, 1 to 100000) sets how many frames the app runs before the answer. The call
answers after those frames, so send it when the app is idle and leave the app alone until the reply arrives.
`memory.report` is the snapshot that answers at once. This method waits and also reports the change over the
window.

```json
{"jsonrpc":"2.0","id":2,"method":"memory.idle_ledger","params":{"frames":120}}
-> {
  "frames": 120,
  "heap_tracked": true,
  "hint": null,
  "live_bytes": 61210624,
  "growth_bytes": 2048,
  "allocations": 9840,
  "allocations_per_frame": 82.0,
  "allocated_bytes_per_frame": 30411.7,
  "largest": "unscoped",
  "subsystems": [
    { "name": "unscoped", "live_bytes": 40112128, "live_allocations": 50211, "growth_bytes": 1024, "allocations": 9100 },
    { "name": "scene", "live_bytes": 65536, "live_allocations": 102, "growth_bytes": 0, "allocations": 0 }
  ],
  "largest_owner": "render.shader_modules",
  "owners": [
    { "name": "render.shader_modules", "subsystem": "render", "live_bytes": 9437184, "live_allocations": 52110, "growth_bytes": 0, "allocations": 0 },
    { "name": "render.frame", "subsystem": "render", "live_bytes": 4194304, "live_allocations": 3012, "growth_bytes": 0, "allocations": 330 }
  ]
}
```

Every subsystem is listed, in the order of `memory.report`; the example shows two.

| Field | Meaning |
|---|---|
| `subsystems[].live_bytes` | Heap bytes the subsystem holds at the end of the window. |
| `subsystems[].growth_bytes` | Change of `live_bytes` over the window. Steady idle is about 0. |
| `subsystems[].allocations` | Allocations the subsystem made during the window, freed or not. |
| `largest` | The subsystem holding the most bytes at the end. |
| `allocations_per_frame` | All allocations in the window divided by `frames`. |
| `owners` | The named heap owners inside the subsystems (ADR-3351; [Memory per subsystem](memory.md#named-owners-inside-a-subsystem) lists the renderer's), largest `live_bytes` first, with the same four figures. Their bytes are also in their subsystem's row. Empty when the heap is not tracked. |
| `largest_owner` | The owner holding the most bytes at the end; `null` when none holds any. |

Errors:

| Code | Meaning |
|---|---|
| `F0002` | `frames` is not an integer from 1 to 100000 (the protocol's general parameter error). |
| `idle_ledger_running` | Another `memory.idle_ledger` call is still waiting for its frames. Wait for its reply. |
| `idle_ledger_not_installed` | The app has no `regia_agent::startup_idle::StartupWorkPlugin`, so nothing counts its frames. `AgentPlugin` adds it. |

## Checks an agent can run

- **A plugin keeps a scratch buffer:** start the app, wait for `app.startup_work` to say `captured: true`, then call
  `memory.idle_ledger { "frames": 120 }`. A subsystem with a large `live_bytes` that the game does not need while
  idle is the place to look. `growth_bytes` above a few KiB on an idle app is a leak or a cache still filling.
  For the renderer, `owners` names the holder (`render.shader_modules`, `render.pipelines`, `render.frame`, ...).
- **A cold start compiles more:** compare `pipelines_created` and `gpu_live_render_pipelines` between two builds.
  The start-up ratchet does this for the shipped plugin sets.

## The ratchet

`crates/regia/tests/startup_idle_ratchet.rs` runs four plugin sets, each in a fresh child process on the machine's
adapter: `hello_window`, `template_first_person`, `template_character` and `default_plus_optional`. Each set has a
320×180 texture camera over a floor quad. The test records the start-up figures (counted from the app's start, not
the process's) and the heap each subsystem holds through the last 60 of 120 idle frames (`idle_heap_live_<subsystem>`,
the smallest sample, so a worker's transient allocation does not count), and the same floor for each renderer owner
(`idle_heap_owner_render.<owner>`, ADR-3351; 0 for an owner that never allocated). It compares them with
`crates/regia/tests/data/startup_idle.ron` (`format_version` 1). Every case runs twice: counts must be equal, heap
figures (which other threads' timing shifts slightly) within 5 %, and the larger is compared. A figure more than
10 % above its row fails and names the row. A figure more than 10 % under
prints a note asking to lower the row. To re-record after a justified change, put the reason in the commit message
and run:

```sh
REGIA_STARTUP_IDLE_RECORD=1 cargo test -p regia --test startup_idle_ratchet
```

Only the CPU reference adapter compares. On a card the test prints the figures and passes. `heap_peak_bytes`,
`threads_live`, `tracked_submits` and `idle_heap_transient_bytes` are printed but never compared, because they
depend on thread timing. `docs/06-testing-and-ci.md` in the repository has the full rules.

Related: [Memory per subsystem](memory.md) (`memory.report`, `memory.mark`), [Instant load](instant-load.md)
(`render.boot_timeline`: the times of the same start-up).
