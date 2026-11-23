# Frame work: counted, device-independent frame cost

What a frame asked the GPU to do, counted where the renderer records it (ADR-2711). GPU time needs
a card to measure and differs per card; these counts are the same on every adapter (lavapipe
included) for one scene, preset and size, so a cost regression shows on any machine.

## Reading it

`render.frame_work` takes no parameters. The first call turns the per-frame report on: before
that, a window frame reads no counter at all. Until a window frame has finished after the first
call, `seq` is `0` and `frame` is `null`; wait with `wait_until` on `seq > 0`.

| Field | Meaning |
|---|---|
| `format_version` | `1`. |
| `reporting` | Always `true` after the call. |
| `started_now` | `true` when this call turned the report on. |
| `seq` | Published window frames since the report was turned on (1-based). |
| `frame` | The newest window frame's counts (below), or `null`. |
| `targets.count`, `targets.bytes` | The registered render targets (the resource inspector's list) and their bytes at their current sizes. |

`frame` holds:

| Count | What is counted |
|---|---|
| `frames` | Frames the counts cover (1). |
| `render_passes`, `compute_passes` | Passes begun through the counted encoder. |
| `dispatches`, `workgroups` | Direct compute dispatches and their workgroups (`x × y × z`, summed). |
| `indirect_dispatches` | Indirect dispatches (their group counts live on the GPU). |
| `draws`, `instances` | Direct draw calls and their instances. |
| `triangles` | Triangle-list equivalents of the direct draws (`vertices / 3 × instances`). |
| `indirect_draws` | Indirect draw calls; a multi-draw counts once. |
| `pipeline_sets`, `bind_group_sets` | `set_pipeline` and `set_bind_group` calls, render and compute. |
| `submits` | Queue submits, staging flushes included. |
| `pipelines_created`, `bind_groups_created` | Creations on the frame path (the hitch counters). |
| `buffers_created`, `buffer_bytes`, `textures_created`, `texture_bytes` | Allocations through the tracked device and their bytes. |
| `upload_bytes` | Bytes written with `write_buffer` / `write_texture`. |
| `heap_allocations` | Heap allocations, or `null` when no tagging allocator is installed. |

The method has no error codes of its own; it answers in an app without a renderer too (with
`frame` `null`).

## What is not counted

Calls another crate records on its own encoder (UI, VFX, terrain's shared draw), and raw `wgpu`
calls a renderer module still makes outside the counted wrappers. The count is a ratchet of the
renderer's default path, not a GPU profiler: two frames with equal counts can still take
different time on a card.

## The ratchet

`crates/regia_render/tests/frame_work_ratchet.rs` draws four fixed scenes (a forward box, a
10,000-object field, a terrain lab heightfield, a closed GI room) at every shipped preset at
160 × 90 on lavapipe and compares the counts with `crates/regia_render/tests/data/frame_work.ron`.
A count more than 10 % over its row fails, naming the row and the count; a count more than 10 %
under it prints a note. After a justified change, re-record:

```text
REGIA_FRAME_WORK_RECORD=1 cargo test -p regia_render --test frame_work_ratchet -- --nocapture
```

and give the reason in the commit message. The same run counts the VRAM rows (the forward box at
each memory budget tier's resolution against the tier's VRAM budget). See
`docs/06-testing-and-ci.md` § "Frame work ratchet" for when to re-record.

## Example

```json
{"jsonrpc":"2.0","id":1,"method":"render.frame_work","params":{}}
```

returns, after a frame,

```json
{"format_version":1,"reporting":true,"started_now":false,"seq":12,
 "frame":{"frames":1,"render_passes":18,"compute_passes":6,"dispatches":9,"workgroups":412,
          "draws":64,"indirect_draws":2,"triangles":20480,"...":"..."},
 "targets":{"count":21,"bytes":31457280}}
```
