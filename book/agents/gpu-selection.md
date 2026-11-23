# GPU selection at startup

Set the GPU environment before launching the app, editor or benchmark. There is no new agent
method: adapter selection happens before the agent server starts. The same variables apply to
headless and windowed devices. See [Performance and benches](../user-guide/perf-benches.md).

```sh
REGIA_GPU_PCI=0000:21:00.0 REGIA_GPU_STRICT=1 regia bench synthetic_cubes --json
```

Precedence is PCI, UUID, index, name. `REGIA_GPU_UUID` accepts a full NVIDIA UUID or a unique
prefix, case-insensitively, with or without `GPU-`. The pre-rename spellings are read too ([environment variables](../start/running.md#environment-variables)).

Linux reads `/proc/driver/nvidia/gpus/*/information` when `/proc/driver/nvidia` exists, and does
not execute `nvidia-smi` on that path. An exact full UUID still matches when another card's file
is malformed, unreadable or uninitialized (`GPU-????…`). A shorter prefix fails if any card cannot
be read, and the error names that card. When `/proc/driver/nvidia` does not exist, Linux uses the
same three-second `nvidia-smi` deadline as other platforms. With both UUID and PCI set, the
explicit PCI pin wins and the cross-check reads only that card's information file. A mismatch
warns even in strict mode; an unavailable cross-check is ignored. No platform runs `nvidia-smi`
for a PCI pin.

The existing startup log and `RenderDevice::adapter_selection()` report the selected adapter or
fallback. Once the app is running, `render.get_capabilities` returns its `adapter` name and
`backend` (it does not distinguish identical cards by PCI bus). A strict failure remains `RenderError::GpuSelection`; no agent error code or response
schema changes. Benchmark JSON's `adapter` object reports the actual adapter. On shared machines,
keep `REGIA_GPU_STRICT=1` so a failed PCI selection cannot fall back to somebody else's card.

The regression check uses synthetic procfs files and a fake subprocess, without loading a GPU:

```sh
cargo test -p regia_render --lib gpu_select
```

It covers explicit PCI precedence, no-exec resolver seams, UUID-only fixture lookup, an exact UUID
that survives a bad neighbor, named card errors, a single-card PCI cross-check, the absent-procfs
fallback seam, ambiguous prefixes and timeout. The sleeping fake binary and the output fixture are
ignored tests. They return immediately unless the parent sets `REGIA_GPU_UUID_TEST_CHILD`, so
`--ignored` stays fast. Hardware acceptance still needs a healthy driver; this change does not
bound adapter enumeration or Vulkan device creation.

The existing `cargo test -p regia_agent --test render_graph_agent` smoke exercises
`render.get_capabilities` and offscreen screenshots after startup. It passes on lavapipe with
`REGIA_GPU_NAME=llvmpipe REGIA_GPU_STRICT=1` and the software Vulkan ICD selected.
