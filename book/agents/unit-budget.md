# Unit triangle budgets (agents)

DO27 has a frozen triangle table for each unit class. The bench that measures it is a test, not a protocol method:
`cargo test -p regia_render --test unit_budget --release -- --ignored --nocapture --test-threads=1` (book page
[Performance & benches](../user-guide/perf-benches.md#the-unit-budget-bench)). It prints one markdown row per case
and a fitted cost per 100k visible triangles for each path.

To check the same quantities in a running game, use methods that already exist:

1. `perf_overlay.toggle {"mode": "detailed"}`, then `play.step 30`.
2. `perf_overlay.get`. `stats.gpu_ms` and `stats.gpu_passes` are the frame's timestamp nodes, the same numbers
   the bench reads from `FrameRenderer::gpu_timings`. `stats.vram_allocated_bytes` is the allocator total.
3. `play.hitch_report` carries the same `gpu_passes` for the slow frames.
4. `perf_overlay.toggle {"mode": "default"}` restores the player's or host's mode.

```json
{"method": "perf_overlay.toggle", "params": {"mode": "detailed"}}
{"method": "perf_overlay.get"}
```

Reading the bench's rows:

- **visible tris** is what the colour view submits after LOD selection (shadow-only LOD draws left out). The
  visibility buffer cuts clusters further on the GPU; its row also gives the clusters drawn.
- **body LOD 0/1/2/3** counts hull bodies per level. A skinned mesh is never rewritten by `select_mesh_lods`, so
  skinned rows show every hull at LOD0.
- **geometry ms** is shadows + prepass + cull + forward: the passes that hull triangles drive. Screen-space and
  probe passes are in **GPU ms**, not here.

`perf_overlay.toggle` accepts optional `mode` (`off`, `compact`, `detailed`, or `default`; absent cycles
the modes) and `persist` (boolean, default false). It returns `F0002` for an invalid mode or non-boolean
`persist`, and `F0003` if the overlay plugin is unavailable. `perf_overlay.get` takes no
parameters and returns a snapshot even if the plugin is absent. Workflow: enable detailed mode, step a
fixed number of frames, read the overlay stats, then restore `default`.

Captured failing call (lavapipe; overlay plugin absent):

```console
$ regia agent call perf_overlay.toggle '{"mode":"invalid"}' --project samples/arena
error: method call failed: the performance overlay plugin is not installed (-32001): {"code":"F0003","hint":"add regia::DefaultPlugins (feature `ui`) or regia_framework::perf_overlay::PerfOverlayPlugin"}
```
