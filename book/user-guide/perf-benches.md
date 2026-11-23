# Performance & benches

## Picking the GPU

Every headless or offscreen device goes through `RenderDevice::headless`. That covers tests,
`render_lab`, `lookdev` (`look_r2`, `do27_chase`), `vfx_lab`, `terrain_lab`, and the terrain's
block-compression device. On a machine with several cards, set **one** of these variables to choose the card for
all of them:

| variable | selects |
|---|---|
| `REGIA_GPU_UUID=GPU-748396be…` | an NVIDIA GPU by UUID or unique prefix (case-insensitive, optional `GPU-`). Linux resolves it from `/proc/driver/nvidia/gpus/*/information` when that procfs tree exists; if `/proc/driver/nvidia` is absent, Linux uses the same bounded `nvidia-smi` query as other platforms |
| `REGIA_GPU_PCI=0000:41:00.0` | the adapter on that PCI bus (Vulkan or `nvidia-smi` spelling) |
| `REGIA_GPU_INDEX=2` | the n-th adapter in Vulkan order |
| `REGIA_GPU_NAME=7900` | the first adapter whose name contains the text (case-insensitive) |

Precedence is **PCI, then UUID, then index, then name** (ADR-7958). The device logs the adapter it chose, with its backend and
driver, at `info`. If a value matches nothing (an index out of range, an unknown name or UUID), the device logs a
`warn` that lists the adapters and falls back to the usual high-performance pick.
`RenderDevice::adapter_selection()` reports what happened. With `REGIA_GPU_STRICT=1` an unmatched
request is an error (`RenderError::GpuSelection`) instead of a fallback. Use it on shared machines, where the
default pick may be someone else's card. The Vulkan loader's own filter
(`VK_LOADER_DRIVERS_SELECT`, e.g. `'*lvp*'` for lavapipe) applies first.

On Linux, UUID lookup reads the `GPU UUID`, `Bus Location` and `Device Minor` fields in
NVIDIA's procfs information files when `/proc/driver/nvidia` exists. It does not run `nvidia-smi`
on that path. An exact full UUID still selects its card when another card's file is malformed,
unreadable, or the uninitialized placeholder `GPU-????…` (persistence mode off, or the card has
not been opened). A shorter prefix fails if any card cannot be read, and the error names that
card. Set `REGIA_GPU_PCI` to pin directly in that case. When `/proc/driver/nvidia` does not exist
(WSL2, some containers), Linux uses the same three-second `nvidia-smi` query as other platforms.
An unknown UUID or an ambiguous prefix is still a selection error (or the usual warning and
fallback without strict mode).

If both PCI and UUID are set, PCI wins. Linux cross-checks only that card's information file:
a UUID that does not match the file logs a warning, but the explicit PCI value still wins **even
under `REGIA_GPU_STRICT=1`**. An unreadable or unresolved cross-check does not invalidate the PCI
pin, and it does not open any other card's file. No platform launches a UUID subprocess for a
PCI-pinned request. The pre-rename spellings of these variables follow the same rules (the
`REGIA_*` value takes precedence).

On other platforms, a UUID-only request runs `nvidia-smi` with a three-second deadline. Timeout,
unsuccessful exit, or invalid output is a selection error. On timeout the child is sent a kill
request; the caller does not wait indefinitely for cleanup. This bounds UUID lookup, not Vulkan
initialization or arbitrary driver calls. See [GPU selection for agents](../agents/gpu-selection.md).

List the adapters a variable can select, with their PCI buses and drivers:

```
cargo run --release -p lookdev --bin do27_chase -- --list-gpus
```

`regia_render::list_adapters()` returns the same list.

## The DO27 frame bench

`do27_chase` (in `samples/lookdev`) replays DO27's chase on DO27's own content through the full runtime frame
(`REGIA_DO27_CONTENT=<content dir>`). Two frames are kept in flight. It prints wall, CPU and per-pass GPU p50/p99,
the terrain's own share of the prepass and forward pass, and per-cascade shadow timing.

- `--plates` measures the six UE plate cameras instead of the chase.
- `--tier mid` turns on the opt-in `TerrainBudget`.
- `--sync --idle-ms N` lets the GPU idle between frames. An idle GPU clocks down, and every pass then reads about 2× longer.

Measure with the GPU busy. Results are in `docs/evidence/perf/`.

## The unit-budget bench

`crates/regia_render/tests/unit_budget.rs` measures DO27's frozen per-class triangle table on the GPU it runs on,
without DO27 content. Hulls are procedural (closed, displaced ellipsoid pieces with an exact triangle count). They
are written as a glTF and cooked by the real importer with `generate_lods`, so every LOD chain comes from the
engine's meshopt simplifier. Frames take the runtime path: ECS entities with `MeshRenderer` and `MeshLodChain`,
`extract_frame`, `select_mesh_lods`, `extract_glass`, then `render_extracted_into` with Desktop High.

```text
cargo test -p regia_render --test unit_budget --release -- --ignored --nocapture --test-threads=1
```

For each class (skimmer, battle, walker, structure exterior, walk-in interior, seated cockpit), resolution (720p,
1080p, 1440p) and hull count (1, 5, 10, 20, 40), it prints:

- GPU ms per frame (median and p90 of the timestamp nodes) and the top nodes;
- the geometry passes (shadows, prepass, cull, forward) and the glass section;
- visible triangles and the body LOD histogram;
- mesh draws (`DrawStats`), VRAM (allocator report: total and meshes), and CPU ms.

The cases cover (a) every hull at LOD0, (b) the engine's selection on a by-distance layout, and the same places
with DO27's intended mix pinned (2 at LOD0, a fifth at LOD1, the rest LOD2/LOD3), (c) the walker as rigid through
the visibility buffer and as skinned through the forward path, (d) glass in its `BLEND` material and in the opaque
body material, and (e) the seated cockpit (near plane 0.05 m). A least-squares fit then gives the GPU cost per 100k
visible triangles for each path.

Those layouts pin the mesh LOD tier scales at the values that select as before them (distance 1,
shadow 0.5), so their rows compare with earlier runs. Two layouts at the same places measure the tier
scales (v0.1.20, [Mesh LOD tiers](../agents/mesh-lod-tiers.md)): `close20` holds the body's colour LOD 0 to
20 m (the distance scale that puts its LOD0→1 colour switch there) with shadows at the table mix (shadow scale
1 / distance scale), and `ultra` runs Desktop Ultra's scales as shipped. Each row also gives the body's shadow
LOD histogram. Before the cases, a table lists every class's body switch distances (colour and shadow,
LOD0→1→2→3) for every preset at every resolution.

Narrow the run with `UNIT_BUDGET_CLASSES`, `UNIT_BUDGET_RES`, `UNIT_BUDGET_N`, `UNIT_BUDGET_LAYOUTS` (`lod0`,
`mix`, `table`, `close20`, `ultra`), `UNIT_BUDGET_WARM`, `UNIT_BUDGET_FRAMES` and `UNIT_BUDGET_TEXTURE`.
`UNIT_BUDGET_OUT` writes the markdown table to a file.
`unit_budget_depth_precision` (also ignored) counts z-fighting between an unlit panel and the wall a fraction of a
millimetre behind it, at 0.5 m to 50 m from a 0.05 m near plane. The fast, non-ignored smoke case renders 5 hulls
of 10k triangles on lavapipe and checks that the frame is not black; a second fast case checks the selection
with the tier scales on the cooked chain (no GPU).
