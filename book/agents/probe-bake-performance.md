# Probe bake performance

How `regia bake-gi` keeps a probe-volume bake fast, what its summary reports about it, and how to
measure a change. Design: ADR-862. Baking itself: [Baking GI from meshes](baked-gi-bake.md).

## What the bake does to save time

None of these change a valid probe's result except where noted. The tests in
`crates/regia_pt/tests/probe_bake_perf.rs` check each claim on the real kernels.

| Speed-up | What it does | Effect on the result | Switch |
|---|---|---|---|
| Coherent ray order | Each probe's rays run in Morton order over the octahedral map, so the rays a GPU wave traces together point the same way. | The same rays and paths. Only the order of the sums changes, so the last bits change. | `ProbeTraceSettings::coherent_rays` (on) |
| Path compaction | After the first bounce, only the paths still going are dispatched, from a queue, through an indirect dispatch. | None: the same paths, bit for bit. | `ProbeTraceSettings::compact_paths` (on) |
| Geometry-only relocation | Relocation rounds record only each ray's first hit (distance, face). They do no shading and send no shadow rays. | Relocation reads only those values, so it decides exactly as before. | `GiBakeSettings::geometry_only_relocation` (on) |
| Retrace only moved probes | A relocation round traces again only the probes the previous round moved. Every probe keeps its own random stream, so the others would trace the same and decide the same. | None. | Always on |
| Skip probes inside geometry | A probe whose last relocation trace, at its final position, saw back faces on at least half of 64 or more rays is marked inside geometry without the main trace. A probe the last round moved first gets one geometry-only classification trace at its final position (progress stage `classify`). | Its flags are the same (`INSIDE_GEOMETRY`, not `VALID`). Its stored SH are 0 instead of traced values that nothing reads. | `GiBakeSettings::skip_inside_fraction` (0.5; above 1 is off) |
| One GPU wait per batch | The batch's sample submissions run back to back. The readback waits once. | None. | Always on |
| Multithreaded BVH | Large meshes build their BVH on every core. Meshes build in parallel with each other. | None: the tree does not depend on the thread count. | Always on |

`GiBakeSettings::without_shortcuts()` turns off every switch in the table. Use it as the reference
when you check a result or time a change.

## The summary

The `report` object of the `regia bake-gi` summary (`GiBakeReport`) has these fields:

| Field | Meaning |
|---|---|
| `relocation_traces` | Probe traces the relocation rounds ran, plus the classification traces. With N live probes and R rounds this is at most N·(R + 1), and usually close to N. |
| `inside_skipped` | Probes marked inside geometry from their relocation trace, without the main trace. These probes are also counted in `inside_geometry`. |
| `relocation_seconds`, `trace_seconds`, `encode_seconds` | Wall time of the relocation rounds, of the main trace, and of the filter plus encoding. Their sum is a little less than `seconds`, which also covers setup. |
| `rays_traced` | Rays of the main trace. A skipped probe adds none. |

The fields are additive: older readers ignore them.

## Measuring

```text
cargo run --release -p regia_pt --example probe_bake_bench -- [--scene scattered-hills] [--runs 3] \
    [--rays 256] [--samples 16] [--cpu-only] [--bvh-tris 1000000]
```

The bench prints one JSON object per line:

- `"bench": "bvh"`: one large mesh's BVH, built on one thread and on every core. It reports the time of
  each, the speed-up, the SAH cost and `same_tree`, which must be `true`. This part needs no GPU.
- `"bench": "bake"`: each built-in scene, plus `scattered-hills`, baked in five modes, best of `--runs`.
  `scattered-hills` is `rolling-hills` with 300 boulders and four closed houses, each house inside a 1 m detail
  brick. It has buried probes and deep bounces, which the built-ins mostly lack. The modes:
  - `reference` is `without_shortcuts()`.
  - `coherent` adds only the ray order.
  - `compact` adds only path compaction.
  - `relocation` adds only geometry-only relocation and the inside skip.
  - `fast` is the default settings.
- `"bench": "speedup"`: the reference time divided by each mode's time.

The `reference` mode still uses the speed-ups that are always on. To time the bake before ADR-862, run
`regia bake-gi <scene>` from a build of the commit before it and from a build of this one.

## Errors

`PathTracer::trace_probe_streams` (and `ProbeTracer::trace_streams`, which the bake calls on the GPU and
the CPU tracer alike) takes one stream per probe. It returns an error (`PtError::Scene`) in two cases: the
stream count differs from the probe count, or a stream is 2^24 or more. The bake never passes either.
