# Hitches (`play.hitch_report`, the DO27 soak)

`specs/27` § 2.4 makes zero hitches a hard gate: after a 2 s warm-up, no pipeline or bind group is
created on the frame path, no frame blocks on the GPU, no frame is over 50 ms, at most 0.1 % of
frames are over 2 × p50, p99/p50 ≤ 1.3, p99.9/p50 ≤ 2.0, and the longest frame is ≤ 2.5 × p50.

## Measuring a running app

```json
{"jsonrpc":"2.0","id":1,"method":"play.hitch_report","params":{"seconds":60,"warmup_s":2,"drive":"chase","timeout_ms":62000}}
```

The reply arrives after `warmup_s + seconds`. Omit `timeout_ms` and the connection waits exactly 30000 ms, then answers `F0006` while the measurement continues; a second call is `F0017`. A present `timeout_ms` waits that many milliseconds (at most 24 h) plus a 5 s grace. Set it to at least `(warmup_s + seconds) × 1000` (62000 here; the grace is extra). Each frame's interval is `RealTime::raw_delta()` (not clamped to 250 ms, so a 4.7 s stall reads 4700 ms; what a
player sees); CPU and GPU times come from the `RuntimeRenderer`. The report turns its GPU
timestamps on for the run and off after; `gpu` is `null` only on a device without timestamp
queries. `gpu_passes` is the last frame's GPU time per pass (`shadows_ms`, `gi_ms` (GI Depth's probe
update and screen lookup; 0 under Balanced), `prepass_ms`, `forward_ms`, `ssao_ms`,
`contact_shadows_ms`, `ssgi_ms`, `ssr_ms`, `taau_ms`, `scene_ms`, `post_ms`, `frame_ms`, ...), which shows
where a GPU-bound frame goes (on a phone, see [render profile](render-profile.md)).

`gpu_passes.nodes` lists the same frame's GPU-timed passes in graph order, each named after the frame-graph node
that records it (`[{ node, pass, ms }]`, ADR-5386): the names are the `render.graph` `dispatch.recorded` names, so a
slow pass points at a node of the [render graph](render-graph.md). A pass is listed only when its closing timestamp
was written. `gi probes` there is the probe update alone; the irradiance screen lookup is its own entry
(`gi lookup`, node `ssao`, which records it). `gpu_passes.stamps_outside_nodes` lists every timestamp written outside
its node's record site (`[{ query, owner, written_in }]`); it is empty on a correct frame, and anything there means
a pass time mixes in another node's work. `gpu_passes` is `null` without timestamp queries.

Device calls are the renderer's process-wide counters:

| Field | Meaning |
|---|---|
| `p50_ms`, `p99_ms`, `p999_ms`, `*_over_p50` | Frame-interval percentiles (nearest rank) and their ratio to the median |
| `over_2x_p50`, `over_50ms`, `longest_ms` | The hitch counts the gate reads |
| `device_calls` | `pipelines`, `bind_groups`, `blocking_polls`, `buffers`, `textures` (+ bytes), `upload_bytes`, `map_asyncs`, `submits`, `inline_sky_bakes` after warm-up |
| `bound` | What limited the measured frames, each frame's GPU time against its interval: `verdict` (`cpu_bound`, `gpu_bound`, `balanced`, `mixed` when no class has a majority, `unknown` without GPU timing), the frame count per class and `gpu_bound_pct`/`cpu_bound_pct`. GPU-bound: a lower render scale helps; CPU-bound: the GPU waits, so look at main-thread work, a frame cap or vsync. Same thresholds as `telemetry.live` |
| `creates`, `creates_dropped` | Every frame-path create after the warm-up, by `kind` (`pipeline`, `bind_group`, `buffer`, `texture`) and `label`, with `count`, `first_frame` and `last_frame` (measured frames from 0). `creates_dropped` counts creates the renderer's 32-entry create ring overwrote before a frame read it (more than 32 in one frame): counted in `device_calls`, not named |
| `blocking_poll_sites`, `blocking_poll_sites_unnamed` | Where the measured window's blocking polls (`device_calls.blocking_polls`) were called: one row per `TrackedDevice::poll` call site, `site` (`file:line` of the call) and `count`, most first. The renderer keeps the first 32 distinct sites of the process; waits from later sites are counted in `blocking_poll_sites_unnamed`, not named (ADR-4050) |
| `work` | The frame's own cost, not its interval (ADR-4947): per measured frame the larger of the CPU work (the display pacer's trace row: the frame's cost without the time blocked in the swapchain) and the GPU frame time, as `p50_ms`, `p99_ms`, `longest_ms`, with `cpu_p50_ms`/`cpu_p99_ms`, `frames` (frames with a pacer row) and `gpu_timed_frames`. On a vsync-paced panel `p99_ms` cannot drop under the refresh interval; `work.p99_ms` can, so it is what the Android `frame_p99` budget reads. `null` where no display pacer runs (desktop, headless) |
| `spikes` | The 32 worst frames over 2 × p50 with their CPU/GPU time, `bound` (the same class names, per frame) and creates |
| `streaming_waits` | Memory-pressure context over the measured window. The § 2.4 "no frame waits on a load" row has no numeric target, so it is reported here but **not judged** (`pass`/`failures` ignore it): `frames` (measured frames), `texture_held_frames` (frames the texture streamer held back mip uploads, within `MIP_HOLD` of its last out-of-memory signal) and `texture_backing_off_frames` (frames it backed off after a streamed texture failed to allocate), plus `render_residency` (`{ on, waits, timeouts, waiting_cells }`, deltas over the measured window; `null` without the render plugin). Nonzero values mean the streamer is under memory pressure: raise texture-memory headroom (pool size, quality) or prewarm. There is no per-frame upload-budget knob behind these counters |
| `pass`, `failures` | The verdict and every judged § 2.4 row missed, in words (the Streaming row is reported in `streaming_waits` but not judged) |

`creates` answers "what was created, and when" without a trace build. Ask for a short report
while the game does the thing that hitches (a cut, the first effect, a held camera), then look up
the labels in the table below:

```json
{"jsonrpc":"2.0","id":3,"method":"play.hitch_report","params":{"seconds":5,"warmup_s":2}}
```

```json
{"device_calls": {"bind_groups": 4, "pipelines": 0, "...": 0},
 "creates": [
   {"kind": "bind_group", "label": "hist bg", "count": 3, "first_frame": 212, "last_frame": 212},
   {"kind": "bind_group", "label": "post bg", "count": 1, "first_frame": 212, "last_frame": 212}],
 "creates_dropped": 0, "pass": false, "...": "..."}
```

The counters and `creates` see what the renderer creates through its tracked device
(`regia_render::TrackedDevice`). A crate that creates on a raw `wgpu::Device` is invisible here;
find it with the census build below.

`drive` names a scripted drive: the method inserts the `regia_agent::HitchDrive` resource for
the run and removes it after. A game that has scripted drives starts the named one when it sees the
resource. Only one report runs at a time (`F0017`).

## What causes a hitch, and the fix

| Signal in the report | Cause | Fix |
|---|---|---|
| `bind_groups`/`buffers` > 0, labels `material bg`, `mesh vertex arena` (`mesh vb` before batch v020) | A material variant seen for the first time, or meshes that outgrew the mesh arenas (rocks streaming in) | `FrameRenderer::prewarm(device, variants, max_instances)` at level load; `ScatterField::prewarm_meshes()` lists every rock LOD × variant |
| `bind_groups` > 0, labels `gpu cull`, `instance objects`, `velocity objects` | A per-instance buffer grew past a power of two | The `max_instances` of `prewarm` |
| `inline_sky_bakes` > 0, 40–80 ms frames on sun moves or 10 m camera height changes | Atmosphere tables and the sky IBL integrated on the CPU inside the frame | `FrameRenderer::set_async_sky(true)` (on by default in `RuntimeRenderer`) |
| Long frames on camera cuts with `scatter update` time | A cut asks for a whole region's scatter tiles at once | `ScatterField::set_tile_budget(12)` (nearest tiles first) |
| One 50–300 ms frame about half a second after spawn, nearly all `scatter update`, only on CPUs with ≤ 12 threads and with the GPU probes on (Desktop High/Ultra) | Before ADR-176 (scatter stall): `ScatterField::update` waited for a needed tile's background placement, queued on the async pool (cpus / 4 threads) behind the probes' brick bakes | Fixed: a needed tile still queued is placed on the caller (`ScatterStreamStats::replaced_tiles`); `update` never waits for the pool. If it comes back, look for a new `block_on` of pool work on the frame thread |
| 100–600 ms frames with CPU ≈ normal, no creates, no run-queue wait, and one or two **major** page faults (`do27_chase` CSV `major_faults`, `spawn_major_faults`), on spawns; only on a box short of memory | The spawn read data nothing else read for seconds (the terrain heights where the effect lands) and the OS had swapped it out | Data a frame may read at any point registers a `regia_core::residency::ResidentRegion` (`TerrainMap` does); `regia_app::ResidencyPlugin` (in `DefaultPlugins`) keeps it paged in. Reproduce on a loaded box, or with `do27_chase --memory-squeeze MB[:MS][:file]` / `SPAWN_SOAK_SQUEEZE` (mind: a few MiB/s already thrashes) |
| `bind_groups`/`buffers` > 0 on spawn frames in the runtime, labels `gpu cull`, `instance objects`, `velocity objects` | Spawned objects pushed the per-instance buffers past a power of two | `RuntimeRenderer` reserves 1024 draws (`RUNTIME_INSTANCE_RESERVE`); call `RuntimeRenderer::reserve_instances(n)` at level load for more |
| Every ~20 s a burst of `sssr *`, `spd depth`, `spd params`, `spd unused level` creates (≈ 70 MB of textures at 1440p), with an `sssr probe` bind group ~150 frames before it | Before ADR-168: the stochastic SSR pass went dormant when nothing glossy was in view, freed its targets, and re-allocated them on the wake | Fixed: dormant keeps the targets (≈ 185 MiB at 1440p, ≈ 416 MiB at 4K) and zeroes them on the wake. If these labels come back, something freed them: `FrameRenderer::trim_memory()` / `regia_render::signal_memory_pressure()` (the OS low-memory event) do on purpose, a resize or quality change does too |
| `pipelines`/`bind_groups`/`buffers` = 1 each on the first dust or smoke spawn, labels `vfx soft composite`, `vfx soft composite bg`, `vfx soft composite params` | Before midplay-creates: the soft-particle layer built its composite pipeline and bind group on its first soft particle | Fixed: built on the first HDR frame that enables the layer (warm-up), whether or not soft particles are alive |

`residency` (present when the keeper runs): `data_bytes` and `code_bytes` swept, `pages` per sweep,
`sweeps` and `restored_pages` during the run (pages found paged out and read back by the keeper's
thread, each a major fault a frame did not take), `last_sweep_us`/`max_sweep_us`. `sweeps`,
`restored_pages` and `max_sweep_us` accumulate across suspends: a suspend stops the sweep thread and
a resume starts a new one, and the totals add up (so the `end - start` window above stays correct
when the app suspends and resumes inside it).
`REGIA_KEEP_RESIDENT=0` turns the keeper off for an A/B; `=code` also sweeps the code.

## Spawning without hitches (`drive: "spawns"`)

`samples/arena` maps `HitchDrive { name: "spawns" }` onto its `SpawnDrive`: every 0.25 s a crate
(shared mesh and material, a dynamic body, a box collider), a showcase effect, a scorch decal and a
positional sound, the oldest crates despawned. Through the runtime frame (a texture-target camera)
`samples/arena/tests/spawn_hitch.rs` asks for the report over a real connection (wall-clock time:
`enable_fixed_step` would make every frame a synthetic 16.7 ms) and requires zero device calls after
the warm-up and, on a hardware GPU, no frame that spawned over 50 ms; it prints the spawn frames'
p99 and worst, and the long frames without a spawn (the machine's, on a shared box):

```json
{"jsonrpc":"2.0","id":2,"method":"play.hitch_report","params":{"seconds":60,"warmup_s":3,"drive":"spawns","timeout_ms":63000}}
```

The spawn path carries spans for a profiler or `do27_chase --slow-spans`: `vfx_spawn`,
`vfx_schedule`, `vfx_upload`, `decal_spawn`/`decal_update`, `scene_spawn`, `prefab_instantiate`,
`cooked_scene_spawn`, `level_spawn`, `asset_load` (with `kind`), `mesh_upload` and `material_upload`
(the renderer's first sight of a mesh or material), `audio_drain`, `audio_start_voice`,
`audio_start_emitter`, and the existing `sync_added_bodies` (physics) and `ScatterField::update`
(scatter re-tile). In `do27_chase` the soak's spawn step is the top-level `soak_spawn` span and the
`spawn_ms` column.

`SPAWN_SOAK_SPANS=1` on the arena test times every span per frame and prints, per span, the mean
time and entries on spawn frames against other frames, and the span breakdown of the five slowest
spawn frames with the frame before each. Read the entries before blaming the spawn: a time-driven
spawn lands on the frame after a long one, which also runs more fixed physics steps (2026-10-08 on
a loaded 5090: 1.5 steps against 0.87, every span ≈ 25 % slower; `spawn_drive` itself 0.06 ms).

## The DO27 soak (look-dev)

`do27_chase --soak long|minute` drives every region of both DO27 maps through the full runtime
frame (a boustrophedon over 84 % of each map after the road), cuts to the UE plate cameras and to
teleports every 12 s, jumps the time of day every 20 s, and spawns VFX plus a decal every 4 s (`--spawn-every-s S` for a spawn burst). It
prints this same report per map; `--timeline-dir DIR` writes `<map>.csv` (one row per frame:
wall, CPU phases, GPU, run-queue wait, every counter, events), `<map>.png` (the plot, legend in
`soak_plot.rs`) and `<map>-report.txt` (creates by label, the worst frames and what happened in
them). `--bare` reproduces the frame path of 2026-10-07 (no prewarm, no tile budget, inline sky).
`--submit-probe N` measures the box itself: `queue.submit` of an empty frame and a fixed CPU
workload. On a shared machine, read that floor before blaming the engine for CPU-side spikes.

To find creates the counters cannot see (a crate holding a raw `wgpu::Device`), build with
`cargo run -p lookdev --features wgpu-spans --bin do27_chase -- --census ...`. Every
`Device::create_*` inside a frame is printed with its call chain. `--slow-spans MS` prints the span
breakdown of every frame longer than `MS`.

## Soak on a DO27 bundle (lookdev harness)

This is a developer harness, not an agent method. `do27_chase --soak minute --content <bundle>` runs the hitch soak on a
DO27 content bundle named on the command line (read-only; `REGIA_DO27_CONTENT` is the fallback).
`--hitch-csv <path>` writes one row per measured frame (`frame,t_s,wall_ms,ratio,hitch,events`); `hitch` is 1 when the
frame took more than twice the run's median. `--camera-path orbit|line|figure8` replaces the plate-camera cuts with a
deterministic path. An output path inside the bundle is refused.

## Held views and plate cameras (ADR-2585)

A held camera freezes after 32 frames on a TAAU tier: the screen-space passes stop and post reads the resolve's last
output. A hitch after a cut to a plate camera is checked with a drive like the soak's, without DO27 content:
`cargo test -p regia_render --test warm_drive_creates` (a warm-up, a held camera until it freezes, a cut back to moving
frames, a second held camera; it asserts zero bind groups and pipelines after the warm-up, by label). On lavapipe the
same drive reproduced the soak's `hist bg` ×3 + `post bg` on the second hold before the fix. The fade overlay, HDR
display stage, fullscreen video, HW-RT shadow passes and outdoor targets now appear in `play.hitch_report` `creates`
when they create after the warm-up, as every other frame-path create does.

An empty VFX layer leaves the frame after its first 8 frames: an agent comparing `play.hitch_report` or the frame
timings with and without the layer should see no difference until an effect spawns.

## Counter ledger (integer caps, ADR-3943)

Frame-time milliseconds stay on the report above (`p50_ms`, `over_50ms`, `over_2x_p50`, the percentile
ratios). The create gate is also a named integer ledger, `regia_render::budget_ledger::BudgetLedger`.
A test records deltas and reads `misses(warm)`. `warm` is the first measured frame, the same index the
hitch recorder's report takes. A sum greater than the cap is a miss. A sum equal to the cap is not.

| Name | Default cap after `warm` | Where an agent reads the same integer |
|---|---|---|
| `pipeline_creates` | 0 | `play.hitch_report` `device_calls.pipelines`; `render.frame_work` `frame.pipelines_created` |
| `bind_group_creates` | 0 | `device_calls.bind_groups`; `frame.bind_groups_created` |
| `allocations` | none, until a caller sets one | `device_calls.buffers` and `device_calls.textures`; `frame.heap_allocations`, `buffers_created`, `textures_created` |
| `idle_frames` | none, until a caller sets one | recorded by the caller; this report leaves the name empty |
| `probe_prepares` | none, until a caller sets one | recorded by the caller; this report leaves the name empty |

`play.hitch_report` and `render.frame_work` already return these integers, so the protocol has no
separate ledger method. A pipeline-create delta of 1 on a measured frame is a miss. A delta of 0 is
not. An allocation under a cap the caller installed is not a miss. The ledger does not read a clock.

The hitch-frame feeder copies only pipeline and bind-group creations. For `allocations`, the caller
chooses a heap or device allocation count and records it explicitly; the fields above describe
available inputs. Use one consistent allocation metric when installing its cap. A `null`
`heap_allocations` value means that measurement is unavailable.

A newly accepted capture excludes the frame interval that ended before the request.
Warm-up and measurement count subsequent clock intervals, including their full unclamped
durations; a long startup interval cannot complete a capture before its drive has run.

## Hitches in player logs (v0.5.1)

`play.hitch_report` attributes one frame on request. The always-on `regia::hitch` telemetry event
classifies hitches the same way since v0.5.1 (ADR-7898): a frame spent waiting on the swapchain is
a `present_wait`, not a hitch; a frame under 1.5 pacing periods (frame cap or vsync) is never one;
the `cause` comes from the main and render threads only (worker spans are `background_spans`);
and `unaccounted_ms` with cause `outside_frame` marks time the engine did not measure at all. The
thresholds an agent can read are in `telemetry.live` (`thresholds.hitch_floor_ms`,
`pacing_period_ms`, `present_wait_share`). See [Telemetry](telemetry.md#hitch-truth-v051-adr-7898).

